//! The Wake: the boot sequence, the Sentinel's view of the world (runs, notices, discovery
//! offers), toasts, and the voice envelope that drives the presence meter.
//!
//! Everything here is plain state, advanced by daemon events and the clock (`State::now_ms`).
//! Nothing waits: the boot is drawn from the clock and any key ends it.

use marlowe_proto::{
    Notice, QuietHours, Request, RunInfo, RunStatus, Severity, VoiceStatus, WatchCandidate, WatchState,
    ENVELOPE_HZ,
};

use crate::composer::Composer;
use crate::crew::NO_CREW;
use crate::state::{NoteTone, State};

/// The full boot: assemble, five lines, hold. At most 1.2 s.
pub const BOOT_FULL_MS: i64 = 1_200;
/// A relaunch within [`RELAUNCH_WINDOW_MS`] boots in this long.
pub const BOOT_SHORT_MS: i64 = 300;
pub const RELAUNCH_WINDOW_MS: i64 = 30 * 60 * 1_000;
/// How long a toast stays up.
pub const TOAST_MS: i64 = 6_000;
/// Toasts on screen at once.
pub const MAX_TOASTS: usize = 3;
/// Notices asked for on connect and kept.
pub const NOTICE_LIMIT: u32 = 20;

/// Daemon error texts that mean "this subsystem is not running" (see `marlowe-daemon`).
pub const NO_SENTINEL: &str = "the Sentinel is not running";
pub const NO_VOICE: &str = "voice is not available";
pub const NO_MEMORY: &str = "memory is not available in this daemon";

/// Systems lines in the boot overlay.
pub const BOOT_LINES: usize = 5;

/// The boot sequence's timing. The overlay itself is derived from `State` at draw time, so the
/// values it shows are whatever has arrived so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Boot {
    pub started_ms: i64,
    pub short: bool,
}

impl Boot {
    /// A boot starting at `now_ms`; short when the last launch was within the relaunch window.
    pub fn new(now_ms: i64, last_launch_ms: Option<i64>) -> Self {
        let short = last_launch_ms.is_some_and(|t| (0..RELAUNCH_WINDOW_MS).contains(&(now_ms - t)));
        Self { started_ms: now_ms, short }
    }

    pub fn total_ms(&self) -> i64 {
        if self.short { BOOT_SHORT_MS } else { BOOT_FULL_MS }
    }

    /// The meter assembles in this long.
    pub fn assemble_ms(&self) -> i64 {
        if self.short { 90 } else { 360 }
    }

    /// Gap between systems lines.
    pub fn step_ms(&self) -> i64 {
        if self.short { 35 } else { 110 }
    }

    pub fn elapsed(&self, now_ms: i64) -> i64 {
        (now_ms - self.started_ms).max(0)
    }

    /// Still on screen at `now_ms`. The last frame (at exactly `total_ms`) is drawn.
    pub fn active(&self, now_ms: i64) -> bool {
        self.elapsed(now_ms) <= self.total_ms()
    }

    /// Fraction of the meter assembled, 0..=1.
    pub fn assembled(&self, now_ms: i64) -> f64 {
        (self.elapsed(now_ms) as f64 / self.assemble_ms() as f64).clamp(0.0, 1.0)
    }

    /// Systems lines visible at `now_ms`.
    pub fn lines_shown(&self, now_ms: i64) -> usize {
        let t = self.elapsed(now_ms) - self.assemble_ms();
        if t < 0 {
            return 0;
        }
        ((t / self.step_ms()) as usize + 1).min(BOOT_LINES)
    }

    /// Presence meter levels (0..=4 per dot column) while booting: columns fill left to right,
    /// then settle to the idle row.
    pub fn meter_levels(&self, now_ms: i64) -> [u8; 12] {
        let e = self.elapsed(now_ms);
        let a = self.assemble_ms();
        if e < a {
            let p = e as f64 / a as f64 * 12.0;
            return std::array::from_fn(|c| ((p - c as f64) * 4.0).clamp(0.0, 4.0) as u8);
        }
        let q = ((e - a) as f64 / (self.total_ms() - a).max(1) as f64).clamp(0.0, 1.0);
        let level = (4.0 - 3.0 * q).round().clamp(1.0, 4.0) as u8;
        [level; 12]
    }
}

/// Whether a subsystem answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Presence {
    #[default]
    Unknown,
    Online,
    /// The daemon said it is not running.
    Absent,
}

/// A boot line's state glyph colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    /// Not answered yet.
    Pending,
    Ok,
    Degraded,
    Down,
}

/// One systems line: `memory ····· 390 beliefs · recall online`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SysLine {
    pub label: &'static str,
    pub value: String,
    pub health: Health,
}

/// A notice on screen for a while.
#[derive(Debug, Clone, PartialEq)]
pub struct Toast {
    pub notice: Notice,
    pub until_ms: i64,
}

/// What the Watch view is doing.
#[derive(Debug, Clone, Default)]
pub enum WatchMode {
    #[default]
    Browse,
    /// Typing a directory to watch.
    Add { input: Composer },
    /// Waiting for y/n on no longer watching this root.
    ConfirmRemove { path: String },
}

/// A selectable row in the Watch view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchRow {
    Root(String),
    Offer(WatchCandidate),
}

/// The Sentinel, the voice and the boot, as the client knows them.
#[derive(Debug, Clone, Default)]
pub struct Wake {
    pub boot: Option<Boot>,
    pub watch: Option<WatchState>,
    pub sentinel: Presence,
    /// Newest first.
    pub notices: Vec<Notice>,
    /// Discovery results; `None` until asked.
    pub candidates: Option<Vec<WatchCandidate>>,
    /// Offers that came with the brief.
    pub offers: Vec<WatchCandidate>,
    pub toasts: Vec<Toast>,
    pub voice: Option<VoiceStatus>,
    pub voice_presence: Presence,
    /// When the current envelope chunk arrived (`now_ms`).
    pub envelope_at_ms: i64,
    pub memory_presence: Presence,
    /// "not running" errors expected in answer to the connect probes; absorbed, not flashed.
    expected_errors: Vec<&'static str>,
    pub sel: usize,
    pub mode: WatchMode,
}

impl Wake {
    /// Quiet hours as the Sentinel reports them, else the default.
    pub fn quiet(&self) -> QuietHours {
        self.watch.as_ref().map(|w| w.quiet).unwrap_or_default()
    }

    /// Runs, most urgent first: failed, stalled, alive, finished, ended.
    pub fn runs(&self) -> Vec<&RunInfo> {
        let mut v: Vec<&RunInfo> = self.watch.as_ref().map(|w| w.runs.iter().collect()).unwrap_or_default();
        v.sort_by_key(|r| (run_rank(r.status), std::cmp::Reverse(r.since_ms)));
        v
    }

    /// Offers worth showing: the brief's and discovery's, minus roots already watched.
    pub fn open_offers(&self) -> Vec<WatchCandidate> {
        let roots: Vec<String> =
            self.watch.as_ref().map(|w| w.roots.iter().map(|r| norm(&r.path)).collect()).unwrap_or_default();
        let mut out: Vec<WatchCandidate> = Vec::new();
        for c in self.offers.iter().chain(self.candidates.iter().flatten()) {
            let p = norm(&c.path);
            if !roots.contains(&p) && !out.iter().any(|o| norm(&o.path) == p) {
                out.push(c.clone());
            }
        }
        out
    }

    /// The Watch view's selectable rows: roots, then offers.
    pub fn rows(&self) -> Vec<WatchRow> {
        let mut v: Vec<WatchRow> = self
            .watch
            .as_ref()
            .map(|w| w.roots.iter().map(|r| WatchRow::Root(r.path.clone())).collect())
            .unwrap_or_default();
        v.extend(self.open_offers().into_iter().map(WatchRow::Offer));
        v
    }

    /// Toasts still up at `now_ms`, oldest first.
    pub fn live_toasts(&self, now_ms: i64) -> impl Iterator<Item = &Toast> {
        self.toasts.iter().filter(move |t| t.until_ms > now_ms)
    }

    /// The meter while speaking: amplitude radiating from the centre, newest frame in the middle.
    /// `None` when not speaking or the chunk has run out.
    pub fn voice_levels(&self, now_ms: i64) -> Option<[u8; 12]> {
        let v = self.voice.as_ref().filter(|v| v.speaking && !v.envelope.is_empty())?;
        let idx = ((now_ms - self.envelope_at_ms).max(0) * ENVELOPE_HZ as i64 / 1_000) as usize;
        if idx >= v.envelope.len() + 6 {
            return None;
        }
        Some(std::array::from_fn(|i| {
            let d = if i < 6 { 5 - i } else { i - 6 };
            let amp = idx.checked_sub(d).and_then(|f| v.envelope.get(f)).copied().unwrap_or(0) as u32;
            (1 + (amp * 3 + 127) / 255).min(4) as u8
        }))
    }

    pub fn speaking(&self) -> bool {
        self.voice.as_ref().is_some_and(|v| v.speaking)
    }
}

fn run_rank(s: RunStatus) -> u8 {
    match s {
        RunStatus::Failed => 0,
        RunStatus::Stalled => 1,
        RunStatus::Alive => 2,
        RunStatus::Finished => 3,
        RunStatus::Ended => 4,
    }
}

/// Path comparison key: separators and case folded (Windows).
pub fn norm(p: &str) -> String {
    p.replace('\\', "/").trim_end_matches('/').to_lowercase()
}

impl State {
    /// Starts the boot overlay. Called once by the run loop with the previous launch time.
    pub fn start_boot(&mut self, last_launch_ms: Option<i64>) {
        self.wake.boot = Some(Boot::new(self.now_ms, last_launch_ms));
    }

    /// The boot overlay is up.
    pub fn booting(&self) -> bool {
        self.wake.boot.is_some_and(|b| b.active(self.now_ms))
    }

    /// Something on screen moves with the clock: redraw often.
    pub fn animating(&self) -> bool {
        self.booting() || self.wake.speaking() || self.activity().moving()
    }

    /// How fast the run loop redraws with no input: the boot at its own pace, anything live (a
    /// turn, the voice) at the display's refresh rate, a still screen once a second.
    pub fn cadence(&self) -> crate::pacing::Cadence {
        use crate::pacing::Cadence;
        if self.booting() {
            Cadence::Boot
        } else if self.animating() || self.live.is_some() {
            Cadence::Live
        } else {
            Cadence::Idle
        }
    }

    /// `max_fps` from the settings: the most frames a second to draw (unset: the display's rate).
    pub fn max_fps(&self) -> Option<u32> {
        let n = self.code.settings.as_ref()?.get("max_fps")?.as_u64()?;
        Some(n.min(u64::from(u32::MAX)) as u32)
    }

    /// Requests sent after every Welcome, for the boot lines, the rail and the meter.
    pub(crate) fn wake_probes(&mut self, out: &mut Vec<Request>) {
        self.wake.expected_errors = vec![NO_MEMORY, NO_SENTINEL, NO_SENTINEL, NO_VOICE, NO_CREW];
        out.push(Request::MemoryStats);
        out.push(Request::WatchList);
        out.push(Request::ListNotices { limit: NOTICE_LIMIT, unseen_only: false });
        out.push(Request::VoiceSettings { enabled: None, voice: None });
        out.push(Request::ListRuns { limit: crate::crew::RUN_LIMIT });
        if let Some(v) = &self.crew.open {
            out.push(Request::WatchRun { run: v.run.clone() });
        }
    }

    /// A "not running" answer to a request just sent is expected, not flashed.
    pub(crate) fn expect_error(&mut self, kind: &'static str) {
        self.wake.expected_errors.push(kind);
    }

    /// A "not running" error: records the absence. True when it answered a probe and should not
    /// be flashed.
    pub(crate) fn absorb_absent(&mut self, message: &str) -> bool {
        let kind = [NO_SENTINEL, NO_VOICE, NO_MEMORY, NO_CREW].into_iter().find(|k| *k == message);
        let Some(kind) = kind else { return false };
        match kind {
            NO_SENTINEL => self.wake.sentinel = Presence::Absent,
            NO_VOICE => self.wake.voice_presence = Presence::Absent,
            NO_CREW => self.crew.presence = Presence::Absent,
            _ => self.wake.memory_presence = Presence::Absent,
        }
        match self.wake.expected_errors.iter().position(|k| *k == kind) {
            Some(i) => {
                self.wake.expected_errors.remove(i);
                true
            }
            None => false,
        }
    }

    pub(crate) fn on_watch_state(&mut self, ws: &WatchState) {
        self.wake.sentinel = Presence::Online;
        self.wake.watch = Some(ws.clone());
        let n = self.wake.rows().len();
        self.wake.sel = self.wake.sel.min(n.saturating_sub(1));
    }

    pub(crate) fn on_notice(&mut self, n: &Notice) {
        self.wake.sentinel = Presence::Online;
        let escalated = match self.wake.notices.iter().position(|o| o.id == n.id) {
            Some(i) => {
                let old = self.wake.notices.remove(i);
                n.severity > old.severity
            }
            None => true,
        };
        self.wake.notices.insert(0, n.clone());
        self.wake.notices.truncate(NOTICE_LIMIT as usize * 2);
        if n.severity.is_urgent() && !n.seen && escalated {
            let now = self.now_ms;
            self.wake.toasts.retain(|t| t.until_ms > now && t.notice.id != n.id);
            self.wake.toasts.push(Toast { notice: n.clone(), until_ms: now + TOAST_MS });
            let extra = self.wake.toasts.len().saturating_sub(MAX_TOASTS);
            self.wake.toasts.drain(..extra);
        }
    }

    pub(crate) fn on_notices(&mut self, list: &[Notice]) {
        self.wake.sentinel = Presence::Online;
        self.wake.notices = list.to_vec();
        self.wake.notices.sort_by_key(|n| std::cmp::Reverse(n.at_ms));
    }

    pub(crate) fn on_candidates(&mut self, c: &[WatchCandidate]) {
        self.wake.candidates = Some(c.to_vec());
    }

    pub(crate) fn on_voice(&mut self, v: &VoiceStatus) {
        self.wake.voice_presence = Presence::Online;
        if !v.envelope.is_empty() {
            self.wake.envelope_at_ms = self.now_ms;
        }
        self.wake.voice = Some(v.clone());
    }

    /// The brief arrives as Marlowe's first message. Repeats (a second client, a reconnect) are
    /// not shown twice.
    pub(crate) fn on_brief(&mut self, text: &str, offers: &[WatchCandidate]) {
        self.wake.offers = offers.to_vec();
        let shown = self.entries.iter().any(|e| matches!(e, crate::state::Entry::Brief { text: t, .. } if t == text));
        if !shown && !text.trim().is_empty() {
            self.entries.push(crate::state::Entry::Brief { text: text.to_string(), offers: offers.to_vec() });
            self.follow_tail();
        }
    }

    /// The boot's systems lines, from whatever has arrived.
    pub fn systems(&self) -> Vec<SysLine> {
        use crate::state::Conn;
        let daemon = match &self.conn {
            Conn::Online { daemon_version } => (format!("online · v{daemon_version}"), Health::Ok),
            Conn::Connecting => ("connecting".into(), Health::Pending),
            Conn::Offline { .. } => ("not answering".into(), Health::Down),
            Conn::Refused(_) => ("refused this client".into(), Health::Down),
        };
        let memory = match (&self.memory.stats, self.wake.memory_presence) {
            (Some(s), _) if s.degraded.is_some() => {
                (format!("{} beliefs · keyword only", crate::text::thousands(s.beliefs)), Health::Degraded)
            }
            (Some(s), _) => (format!("{} beliefs · recall online", crate::text::thousands(s.beliefs)), Health::Ok),
            (None, Presence::Absent) => ("not running".into(), Health::Down),
            (None, _) => (String::new(), Health::Pending),
        };
        let brains = if !self.library.known {
            (String::new(), Health::Pending)
        } else {
            let n = self.library.models.len();
            match self.library.conductor() {
                Some(r) => {
                    let name = crate::text::short_model(&r, self.library.info(&r));
                    (format!("{name} · {n} model{}", plural(n)), Health::Ok)
                }
                None if n == 0 => ("no models yet".into(), Health::Degraded),
                None => (format!("no conductor · {n} model{}", plural(n)), Health::Degraded),
            }
        };
        let watch = match (&self.wake.watch, self.wake.sentinel) {
            (Some(w), _) => {
                let roots = w.roots.len();
                let alive = w.runs.iter().filter(|r| r.status == RunStatus::Alive).count();
                let trouble = w.runs.iter().filter(|r| matches!(r.status, RunStatus::Stalled | RunStatus::Failed)).count();
                if roots == 0 {
                    let offers = self.wake.open_offers().len();
                    let tail = if offers > 0 { format!(" · {offers} offer{}", plural(offers)) } else { String::new() };
                    (format!("nothing watched{tail}"), Health::Degraded)
                } else {
                    let mut s = format!("{roots} root{} · {alive} run{} alive", plural(roots), plural(alive));
                    if trouble > 0 {
                        s.push_str(&format!(" · {trouble} need{} a look", if trouble == 1 { "s" } else { "" }));
                    }
                    (s, if trouble > 0 { Health::Degraded } else { Health::Ok })
                }
            }
            (None, Presence::Absent) => ("sentinel not running".into(), Health::Down),
            (None, _) => (String::new(), Health::Pending),
        };
        let voice = match (&self.wake.voice, self.wake.voice_presence) {
            (Some(v), _) if !v.available => ("no speech on this machine".into(), Health::Degraded),
            (Some(v), _) if !v.enabled => ("off".into(), Health::Degraded),
            (Some(v), _) => {
                let name = v.voice.as_deref().map(crate::text::clean).unwrap_or_else(|| "default voice".into());
                (format!("online · {name}"), Health::Ok)
            }
            (None, Presence::Absent) => ("not available".into(), Health::Down),
            (None, _) => (String::new(), Health::Pending),
        };
        [("daemon", daemon), ("memory", memory), ("brains", brains), ("watch", watch), ("voice", voice)]
            .into_iter()
            .map(|(label, (value, health))| SysLine { label, value, health })
            .collect()
    }

    /// Opens the Watch view: fresh state, discovery, notices; unseen notices become seen.
    pub(crate) fn open_watch(&mut self, out: &mut Vec<Request>) {
        self.view = crate::state::View::Watch;
        self.wake.mode = WatchMode::Browse;
        out.push(Request::WatchList);
        out.push(Request::WatchCandidates);
        out.push(Request::ListNotices { limit: NOTICE_LIMIT, unseen_only: false });
        for n in self.wake.notices.iter_mut().filter(|n| !n.seen) {
            n.seen = true;
            out.push(Request::NoticeSeen { id: n.id.clone() });
        }
        self.wake.toasts.clear();
    }

    /// Keys in the Watch view.
    pub(crate) fn watch_key(&mut self, k: crossterm::event::KeyEvent, out: &mut Vec<Request>) {
        use crossterm::event::{KeyCode, KeyModifiers};
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match &mut self.wake.mode {
            WatchMode::ConfirmRemove { path } => {
                match k.code {
                    KeyCode::Char('y') | KeyCode::Enter => {
                        out.push(Request::WatchRemove { path: std::mem::take(path) });
                    }
                    KeyCode::Char('n') | KeyCode::Esc => {}
                    _ => return,
                }
                self.wake.mode = WatchMode::Browse;
            }
            WatchMode::Add { input } => match k.code {
                KeyCode::Esc => self.wake.mode = WatchMode::Browse,
                KeyCode::Enter => {
                    if !input.is_blank() {
                        let path = input.take().trim().trim_matches('"').to_string();
                        out.push(Request::WatchAdd { path });
                        self.wake.mode = WatchMode::Browse;
                    }
                }
                KeyCode::Backspace if input.text().is_empty() => self.wake.mode = WatchMode::Browse,
                KeyCode::Backspace => input.backspace(),
                KeyCode::Delete => input.delete(),
                KeyCode::Left => input.left(),
                KeyCode::Right => input.right(),
                KeyCode::Home => input.home(),
                KeyCode::End => input.end(),
                KeyCode::Char('u') if ctrl => input.clear(),
                KeyCode::Char(c) if !ctrl => input.insert(c),
                _ => {}
            },
            WatchMode::Browse => {
                let rows = self.wake.rows();
                match k.code {
                    KeyCode::Esc | KeyCode::Backspace => self.view = crate::state::View::Conversation,
                    KeyCode::Up | KeyCode::Char('k') => self.wake.sel = self.wake.sel.saturating_sub(1),
                    KeyCode::Down | KeyCode::Tab | KeyCode::Char('j') => {
                        self.wake.sel = (self.wake.sel + 1).min(rows.len().saturating_sub(1))
                    }
                    KeyCode::Char('a') => self.wake.mode = WatchMode::Add { input: Composer::default() },
                    KeyCode::Char('r') => {
                        out.push(Request::WatchList);
                        out.push(Request::WatchCandidates);
                        self.flash("looking for runs…", NoteTone::Dim);
                    }
                    KeyCode::Enter | KeyCode::Char('w') => match rows.get(self.wake.sel) {
                        Some(WatchRow::Offer(c)) => {
                            out.push(Request::WatchAdd { path: c.path.clone() });
                            self.flash(format!("watching {}", crate::text::short_path(&c.path)), NoteTone::Dim);
                        }
                        Some(WatchRow::Root(_)) | None => {}
                    },
                    KeyCode::Char('x') | KeyCode::Delete => {
                        if let Some(WatchRow::Root(p)) = rows.get(self.wake.sel) {
                            self.wake.mode = WatchMode::ConfirmRemove { path: p.clone() };
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// Glyph and severity for a run's status, for the rail and the Watch view.
pub fn run_glyph(s: RunStatus) -> (&'static str, Severity) {
    match s {
        RunStatus::Alive => ("●", Severity::Info),
        RunStatus::Stalled => ("◐", Severity::Attention),
        RunStatus::Finished => ("✓", Severity::Good),
        RunStatus::Failed => ("✗", Severity::Alarm),
        RunStatus::Ended => ("○", Severity::Info),
    }
}

/// Glyph for a notice's severity.
pub fn notice_glyph(s: Severity) -> &'static str {
    match s {
        Severity::Info => "·",
        Severity::Good => "✓",
        Severity::Attention => "▲",
        Severity::Alarm => "✗",
    }
}

/// One-line run detail: what the rail shows under the name.
pub fn run_detail(r: &RunInfo, now_ms: i64) -> String {
    let clean = |s: &str| crate::text::clean(s.lines().next().unwrap_or("").trim());
    match r.status {
        RunStatus::Alive => {
            let mut parts = Vec::new();
            if let Some(s) = &r.stage {
                parts.push(clean(s));
            }
            if let Some(l) = r.layer {
                parts.push(format!("layer {l}"));
            }
            if let Some(step) = r.step {
                parts.push(format!("step {}", crate::text::thousands(step)));
            }
            if let Some(loss) = r.loss {
                parts.push(format!("loss {loss:.4}"));
            }
            if let Some(h) = r.heartbeat_ms {
                parts.push(short_ago(h, now_ms));
            }
            parts.join(" · ")
        }
        RunStatus::Stalled => {
            let since = r.heartbeat_ms.map(|h| silent_for(h, now_ms)).unwrap_or_else(|| "no heartbeat".into());
            match r.pid {
                Some(p) => format!("{since} · pid {p} alive"),
                None => since,
            }
        }
        RunStatus::Finished | RunStatus::Failed => match &r.last_line {
            Some(l) if !l.trim().is_empty() => clean(l),
            _ => format!("ended {}", crate::text::ago(r.since_ms, now_ms)),
        },
        RunStatus::Ended => format!("ended {}", crate::text::ago(r.since_ms, now_ms)),
    }
}

/// `40s ago`, `3m ago`: compact enough for the rail.
fn short_ago(then_ms: i64, now_ms: i64) -> String {
    let s = (now_ms - then_ms).max(0) / 1_000;
    match s {
        0..=59 => format!("{s}s ago"),
        60..=3_599 => format!("{}m ago", s / 60),
        3_600..=86_399 => format!("{}h ago", s / 3_600),
        _ => format!("{}d ago", s / 86_400),
    }
}

/// `silent 23m`.
fn silent_for(then_ms: i64, now_ms: i64) -> String {
    let s = (now_ms - then_ms).max(0) / 1_000;
    match s {
        0..=59 => format!("silent {s}s"),
        60..=3_599 => format!("silent {}m", s / 60),
        _ => format!("silent {}h {}m", s / 3_600, (s % 3_600) / 60),
    }
}

/// Minute of the day from the title clock (`21:40`).
pub fn clock_minute(clock: &str) -> Option<u16> {
    let (h, m) = clock.split_once(':')?;
    Some(h.trim().parse::<u16>().ok()? * 60 + m.trim().parse::<u16>().ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boot_timing_full_and_short() {
        let b = Boot::new(1_000, None);
        assert!(!b.short);
        assert_eq!(b.lines_shown(1_000), 0);
        assert_eq!(b.lines_shown(1_000 + 360), 1);
        assert_eq!(b.lines_shown(1_000 + 1_200), BOOT_LINES);
        assert!(b.active(1_000 + 1_200));
        assert!(!b.active(1_000 + 1_201));
        let s = Boot::new(1_000, Some(1_000 - 29 * 60 * 1_000));
        assert!(s.short);
        assert_eq!(s.lines_shown(1_000 + 300), BOOT_LINES);
        assert!(!s.active(1_000 + 301));
        assert!(!Boot::new(1_000, Some(1_000 - 31 * 60 * 1_000)).short);
        // A clock that went backwards is not a relaunch.
        assert!(!Boot::new(1_000, Some(5_000)).short);
    }

    #[test]
    fn boot_meter_assembles_then_settles() {
        let b = Boot::new(0, None);
        assert_eq!(b.meter_levels(0), [0; 12]);
        let mid = b.meter_levels(180);
        assert_eq!(mid[0], 4);
        assert_eq!(mid[11], 0);
        assert_eq!(b.meter_levels(360), [4; 12]);
        assert_eq!(b.meter_levels(1_200), [1; 12]);
    }

    #[test]
    fn voice_levels_follow_the_envelope_from_the_centre() {
        let mut w = Wake::default();
        assert!(w.voice_levels(0).is_none());
        w.voice = Some(VoiceStatus { available: true, enabled: true, voice: None, speaking: true, envelope: vec![255, 0, 128] });
        w.envelope_at_ms = 0;
        let l = w.voice_levels(0).unwrap();
        assert_eq!((l[5], l[6]), (4, 4));
        assert_eq!(l[0], 1);
        // One frame later the loud frame has moved out one column; the centre is silent.
        let l = w.voice_levels(34).unwrap();
        assert_eq!((l[4], l[5], l[6], l[7]), (4, 1, 1, 4));
        assert!(w.voice_levels(10_000).is_none());
    }

    #[test]
    fn clock_minutes_parse() {
        assert_eq!(clock_minute("21:40"), Some(21 * 60 + 40));
        assert_eq!(clock_minute(""), None);
    }
}
