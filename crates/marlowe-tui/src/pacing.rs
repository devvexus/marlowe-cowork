//! When the next frame is drawn.
//!
//! A frame is drawn the instant something new exists, capped only by the display's refresh rate:
//! the first event after a quiet spell draws at once, a burst of events becomes one frame per
//! refresh. While something live is changing (a spinner, a timer, a stage) the clock itself ticks
//! at the refresh rate; otherwise the screen is redrawn once a second for the clock. The boot
//! keeps the 30 fps it was designed at.
//!
//! This is the policy only. It reads no clock: the run loop passes how long ago the last frame was
//! drawn, so the tests drive it with made-up time.

use std::time::Duration;

/// The slowest redraw: the title bar's clock.
pub const IDLE: Duration = Duration::from_millis(1_000);
/// The boot sequence's own pace.
pub const BOOT: Duration = Duration::from_millis(33);
/// Refresh rate assumed when the display cannot be asked.
pub const FALLBACK_HZ: u32 = 60;

/// How fast the screen is changing without input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cadence {
    /// Nothing moves on its own.
    Idle,
    /// The boot sequence.
    Boot,
    /// Something live changes with the clock: tick at the refresh rate.
    Live,
}

impl Cadence {
    /// Whether frames come faster than a coarse timer can pace them: only an idle screen can do
    /// without the 1 ms timer resolution.
    pub fn needs_fine_timer(self) -> bool {
        self != Cadence::Idle
    }
}

/// Holds a resource (the 1 ms Windows timer resolution) only while the cadence needs it, so an idle
/// UI leaves the system timer rate alone.
pub struct FineTimerHold<G> {
    held: Option<G>,
}

impl<G> Default for FineTimerHold<G> {
    fn default() -> Self {
        FineTimerHold { held: None }
    }
}

impl<G> FineTimerHold<G> {
    /// Takes the resource with `acquire` when `cadence` needs it and none is held, releases it when
    /// the cadence is idle again.
    pub fn follow(&mut self, cadence: Cadence, acquire: impl FnOnce() -> G) {
        match (cadence.needs_fine_timer(), self.held.is_some()) {
            (true, false) => self.held = Some(acquire()),
            (false, true) => self.held = None,
            _ => {}
        }
    }

    pub fn is_held(&self) -> bool {
        self.held.is_some()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pacer {
    refresh_hz: u32,
    /// `max_fps` from the settings or the environment.
    cap: Option<u32>,
    /// Windows' classic console: it cannot take more than 60 frames a second.
    conhost: bool,
}

/// The frame rate: the display's, held to `max_fps` when one is set and to 60 in a classic console.
pub fn effective_hz(refresh_hz: u32, max_fps: Option<u32>, conhost: bool) -> u32 {
    let mut hz = if refresh_hz < 2 { FALLBACK_HZ } else { refresh_hz };
    if let Some(cap) = max_fps.filter(|c| *c > 0) {
        hz = hz.min(cap);
    }
    if conhost {
        hz = hz.min(60);
    }
    hz.max(1)
}

impl Pacer {
    pub fn new(refresh_hz: u32, conhost: bool) -> Pacer {
        Pacer { refresh_hz, cap: None, conhost }
    }

    /// The display's rate from the OS and the terminal from the environment.
    pub fn detect() -> Pacer {
        let conhost = cfg!(windows) && std::env::var_os("WT_SESSION").is_none();
        let mut p = Pacer::new(marlowe_os::console::display_refresh_hz(), conhost);
        p.cap = std::env::var("MARLOWE_MAX_FPS").ok().and_then(|v| v.trim().parse().ok());
        p
    }

    /// `max_fps` as the settings say it; the environment's value stands when they say nothing.
    pub fn set_cap(&mut self, cap: Option<u32>) {
        if cap.is_some() {
            self.cap = cap;
        }
    }

    pub fn hz(&self) -> u32 {
        effective_hz(self.refresh_hz, self.cap, self.conhost)
    }

    /// Time between frames at the current rate.
    pub fn interval(&self) -> Duration {
        Duration::from_micros(1_000_000u64.div_ceil(u64::from(self.hz())))
    }

    /// How long until a frame is due, `since` being the time since the last one was drawn. `dirty`
    /// is whether anything changed since. Zero means draw now.
    pub fn until_draw(&self, since: Duration, dirty: bool, cadence: Cadence) -> Duration {
        let every = match cadence {
            Cadence::Boot => BOOT,
            Cadence::Live => self.interval(),
            Cadence::Idle => IDLE,
        };
        // A change is shown as soon as the refresh rate allows, however slow the cadence.
        let due = if dirty { every.min(self.interval()) } else { every };
        due.saturating_sub(since)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fine_timer_is_held_only_while_the_cadence_is_live_or_boot() {
        use std::{cell::Cell, rc::Rc};
        let (made, dropped) = (Rc::new(Cell::new(0)), Rc::new(Cell::new(0)));
        struct Guard(Rc<Cell<u32>>);
        impl Drop for Guard {
            fn drop(&mut self) {
                self.0.set(self.0.get() + 1);
            }
        }
        let mut hold = FineTimerHold::default();
        let step = |hold: &mut FineTimerHold<Guard>, c| {
            hold.follow(c, || {
                made.set(made.get() + 1);
                Guard(dropped.clone())
            })
        };
        step(&mut hold, Cadence::Idle);
        assert!(!hold.is_held() && made.get() == 0, "idle takes nothing");
        step(&mut hold, Cadence::Boot);
        step(&mut hold, Cadence::Live);
        step(&mut hold, Cadence::Live);
        assert!(hold.is_held() && made.get() == 1, "boot and live share one hold");
        step(&mut hold, Cadence::Idle);
        assert!(!hold.is_held() && dropped.get() == 1, "back to idle releases it");
        step(&mut hold, Cadence::Idle);
        step(&mut hold, Cadence::Live);
        assert!(hold.is_held() && made.get() == 2 && dropped.get() == 1, "live takes it again");
        drop(hold);
        assert_eq!(dropped.get(), 2, "dropping the hold releases it");
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// The run loop with made-up time: events arrive at `events` (ms), the loop draws whenever a
    /// frame is due. Returns the times frames were drawn.
    fn simulate(p: Pacer, cadence: Cadence, events: &[u64], until: u64) -> Vec<u64> {
        let mut drawn_at: Vec<u64> = Vec::new();
        let mut last: Option<u64> = None;
        let mut dirty = true;
        let mut now = 0u64;
        let mut next_event = 0;
        // 0.1 ms steps.
        let step = 100u64;
        let mut t_us = 0u64;
        while now <= until {
            while next_event < events.len() && events[next_event] <= now {
                dirty = true;
                next_event += 1;
            }
            let since = last.map_or(Duration::from_secs(3600), |l| Duration::from_micros(t_us - l));
            if p.until_draw(since, dirty, cadence).is_zero() {
                drawn_at.push(now);
                last = Some(t_us);
                dirty = false;
            }
            t_us += step;
            now = t_us / 1000;
        }
        drawn_at
    }

    #[test]
    fn the_rate_is_the_displays_held_to_max_fps_and_to_60_in_a_classic_console() {
        assert_eq!(effective_hz(144, None, false), 144);
        assert_eq!(effective_hz(144, Some(30), false), 30);
        assert_eq!(effective_hz(144, Some(240), false), 144, "max_fps never raises the rate");
        assert_eq!(effective_hz(144, None, true), 60);
        assert_eq!(effective_hz(0, None, false), 60, "an unknown display is 60");
        assert_eq!(effective_hz(1, None, false), 60, "so is the 1 Hz some drivers report");
        assert_eq!(effective_hz(30, None, true), 30);
        assert_eq!(effective_hz(144, Some(0), false), 144, "0 means no cap");
    }

    #[test]
    fn the_interval_is_the_refresh_period_rounded_up() {
        assert_eq!(Pacer::new(60, false).interval(), Duration::from_micros(16_667));
        assert_eq!(Pacer::new(144, false).interval(), Duration::from_micros(6_945));
        let mut p = Pacer::new(144, false);
        p.set_cap(Some(30));
        assert_eq!(p.interval(), Duration::from_micros(33_334));
    }

    #[test]
    fn the_first_event_after_a_quiet_spell_draws_at_once() {
        let p = Pacer::new(144, false);
        assert_eq!(p.until_draw(ms(900), true, Cadence::Idle), Duration::ZERO);
        // Not before the refresh allows it.
        assert_eq!(p.until_draw(ms(2), true, Cadence::Idle), p.interval() - ms(2));
    }

    #[test]
    fn a_flood_of_events_is_one_frame_per_refresh() {
        let p = Pacer::new(144, false);
        // 1000 events in 5 ms, then quiet.
        let events: Vec<u64> = (0..1000).map(|i| i * 5 / 1000).collect();
        let frames = simulate(p, Cadence::Idle, &events, 40);
        // Drawn at once, then at most one more per 6.9 ms.
        assert_eq!(frames[0], 0);
        assert!(frames.len() <= 2, "{frames:?}");
        // A steady stream of events draws every interval.
        let steady: Vec<u64> = (0..200).collect();
        let frames = simulate(p, Cadence::Idle, &steady, 199);
        let gaps: Vec<u64> = frames.windows(2).map(|w| w[1] - w[0]).collect();
        assert!(gaps.iter().all(|g| (6..=8).contains(g)), "{gaps:?}");
    }

    #[test]
    fn something_live_ticks_at_the_refresh_rate_and_idle_once_a_second() {
        let p = Pacer::new(60, false);
        let live = simulate(p, Cadence::Live, &[], 1000);
        assert!((59..=62).contains(&live.len()), "{}", live.len());
        let idle = simulate(p, Cadence::Idle, &[], 3500);
        assert_eq!(idle, vec![0, 1000, 2000, 3000], "{idle:?}");
    }

    #[test]
    fn the_boot_keeps_its_pace_and_an_event_still_draws_at_the_refresh_rate() {
        let p = Pacer::new(144, false);
        let boot = simulate(p, Cadence::Boot, &[], 1000);
        assert!((29..=32).contains(&boot.len()), "{}", boot.len());
        assert_eq!(p.until_draw(ms(20), true, Cadence::Boot), Duration::ZERO);
    }

    #[test]
    fn a_classic_console_and_max_fps_slow_the_live_tick() {
        let conhost = Pacer::new(144, true);
        let live = simulate(conhost, Cadence::Live, &[], 1000);
        assert!((59..=62).contains(&live.len()), "{}", live.len());
        let mut capped = Pacer::new(144, false);
        capped.set_cap(Some(30));
        let live = simulate(capped, Cadence::Live, &[], 1000);
        assert!((29..=32).contains(&live.len()), "{}", live.len());
    }
}
