//! Escape sequences the run loop writes around ratatui: the terminal's default background
//! (OSC 11 to set, OSC 111 to reset), the window title (OSC 2) and the title stack (XTWINOPS
//! 22/23).
//!
//! The background is the terminal's *default* colour, not a cell fill: every cell still has
//! `Color::Reset`, and the terminal paints that default in Marlowe's `#0f0e14` (the old harness's
//! background) until OSC 111 hands the profile's colour back. A terminal that does not know OSC 11
//! (or 111) ignores it, and that is the whole fallback: nothing is drawn differently.
//! `MARLOWE_KEEP_TERMINAL_BG=1` leaves the background alone.
//!
//! What was changed is recorded in process-wide flags, so every exit path — the normal return, a
//! panic hook, a console close — restores exactly once, from whichever thread gets there first.

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};

/// The old harness's background, `#0f0e14`, as OSC 11 spells it.
pub const SET_BACKGROUND: &[u8] = b"\x1b]11;rgb:0f/0e/14\x1b\\";
/// Back to the profile's default background.
pub const RESET_BACKGROUND: &[u8] = b"\x1b]111\x1b\\";
/// Saves the window title on the terminal's title stack; [`POP_TITLE`] puts it back.
pub const PUSH_TITLE: &[u8] = b"\x1b[22;0t";
pub const POP_TITLE: &[u8] = b"\x1b[23;0t";
/// Set (to anything but `0`/`false`) to keep the terminal's own background.
pub const KEEP_BG_ENV: &str = "MARLOWE_KEEP_TERMINAL_BG";

static BACKGROUND_SET: AtomicBool = AtomicBool::new(false);
static TITLE_PUSHED: AtomicBool = AtomicBool::new(false);

/// Whether the environment asks to keep the terminal's background.
pub fn keep_background(env: impl Fn(&str) -> Option<String>) -> bool {
    env(KEEP_BG_ENV).is_some_and(|v| {
        let v = v.trim();
        !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false")
    })
}

/// Sets the terminal's default background to `#0f0e14` (unless `keep`) and saves the window
/// title. Both are undone by [`restore`].
pub fn enter(out: &mut dyn Write, keep: bool) -> io::Result<()> {
    out.write_all(PUSH_TITLE)?;
    TITLE_PUSHED.store(true, Ordering::SeqCst);
    if !keep {
        out.write_all(SET_BACKGROUND)?;
        BACKGROUND_SET.store(true, Ordering::SeqCst);
    }
    out.flush()
}

/// `marlowe — <session>` as the window title. Control characters are dropped so a title cannot
/// end the sequence early.
pub fn title(out: &mut dyn Write, title: &str) -> io::Result<()> {
    let clean: String = title.chars().filter(|c| !c.is_control()).collect();
    write!(out, "\x1b]2;{clean}\x1b\\")?;
    out.flush()
}

/// Puts back whatever [`enter`] changed. Safe to call any number of times, from any thread;
/// only the first call after `enter` writes.
pub fn restore(out: &mut dyn Write) {
    if BACKGROUND_SET.swap(false, Ordering::SeqCst) {
        let _ = out.write_all(RESET_BACKGROUND);
    }
    if TITLE_PUSHED.swap(false, Ordering::SeqCst) {
        let _ = out.write_all(POP_TITLE);
    }
    let _ = out.flush();
}

/// A panic hook that runs `restore` and then `previous`: the terminal is put back before the
/// panic message is printed, so the message is readable and the background is the profile's.
pub fn restoring_hook(
    restore: impl Fn() + Send + Sync + 'static,
    previous: Box<dyn Fn(&std::panic::PanicHookInfo<'_>) + Send + Sync + 'static>,
) -> Box<dyn Fn(&std::panic::PanicHookInfo<'_>) + Send + Sync + 'static> {
    Box::new(move |info| {
        restore();
        previous(info);
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// The flags are process-wide; these tests take turns.
    static SERIAL: Mutex<()> = Mutex::new(());

    fn has(hay: &[u8], needle: &[u8]) -> bool {
        hay.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn enter_sets_the_background_and_restore_resets_it_once() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let mut out = Vec::new();
        enter(&mut out, false).unwrap();
        assert!(has(&out, b"\x1b]11;rgb:0f/0e/14\x1b\\"), "OSC 11 with #0f0e14");
        assert!(has(&out, PUSH_TITLE));
        let mut back = Vec::new();
        restore(&mut back);
        assert!(has(&back, b"\x1b]111\x1b\\"), "OSC 111 resets to the profile's background");
        assert!(has(&back, POP_TITLE));
        let mut again = Vec::new();
        restore(&mut again);
        assert!(again.is_empty(), "a second restore writes nothing");
    }

    #[test]
    fn keeping_the_background_writes_no_osc_11() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let mut out = Vec::new();
        enter(&mut out, true).unwrap();
        assert!(!has(&out, b"\x1b]11;"));
        let mut back = Vec::new();
        restore(&mut back);
        assert!(!has(&back, RESET_BACKGROUND), "nothing to reset");
        let env = |v: &'static str| move |k: &str| (k == KEEP_BG_ENV).then(|| v.to_string());
        assert!(keep_background(env("1")));
        assert!(!keep_background(env("0")));
        assert!(!keep_background(|_| None));
    }

    #[test]
    fn the_panic_hook_restores_before_the_message() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let mut out = Vec::new();
        enter(&mut out, false).unwrap();
        let sink = Arc::new(Mutex::new(Vec::<u8>::new()));
        let order = Arc::new(Mutex::new(Vec::<&'static str>::new()));
        let (s, o1, o2) = (sink.clone(), order.clone(), order.clone());
        let previous: Box<dyn Fn(&std::panic::PanicHookInfo<'_>) + Send + Sync> =
            Box::new(move |_| o2.lock().unwrap().push("message"));
        std::panic::set_hook(restoring_hook(
            move || {
                restore(&mut *s.lock().unwrap());
                o1.lock().unwrap().push("restore");
            },
            previous,
        ));
        let r = std::panic::catch_unwind(|| panic!("boom"));
        drop(std::panic::take_hook());
        assert!(r.is_err());
        assert!(has(&sink.lock().unwrap(), RESET_BACKGROUND), "the hook wrote OSC 111");
        assert_eq!(*order.lock().unwrap(), ["restore", "message"]);
    }

    #[test]
    fn titles_cannot_smuggle_control_characters() {
        let mut out = Vec::new();
        title(&mut out, "marlowe — cli\x1b]0;evil\x07").unwrap();
        assert_eq!(out, "\x1b]2;marlowe — cli]0;evil\x1b\\".as_bytes());
    }
}
