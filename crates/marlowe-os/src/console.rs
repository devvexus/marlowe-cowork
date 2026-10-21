//! Asking the terminal a question: a query written to stdout, the reply read back from the
//! console input queue.
//!
//! A terminal answers a query such as Device Attributes (`CSI c`) by typing the reply into the
//! application's input. On Windows that reply only arrives as characters when the console input
//! is in virtual-terminal mode, which crossterm leaves off. [`query`] turns it on for the length of
//! one exchange, reads the key events as characters until the caller says the reply is complete
//! or the deadline passes, and restores the mode it found.
//!
//! Call it before anything else reads input: every input event that arrives during the exchange
//! is consumed.
//!
//! [`on_interrupt`] runs a callback when the console is about to stop the process (Ctrl-Break,
//! Ctrl-C outside raw mode, the window closing, logoff, shutdown), so a full-screen client can put
//! the terminal back first.

use std::io::Write;
use std::time::{Duration, Instant};

use crate::OsError;

#[cfg(windows)]
use windows_sys::Win32::{
    Foundation::{GetLastError, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0},
    System::Console::{
        GetConsoleMode, GetStdHandle, ReadConsoleInputW, SetConsoleMode, CONSOLE_MODE,
        ENABLE_VIRTUAL_TERMINAL_INPUT, INPUT_RECORD, KEY_EVENT, STD_INPUT_HANDLE,
    },
    System::Threading::WaitForSingleObject,
};

#[cfg(windows)]
fn last_error(call: &'static str) -> OsError {
    // SAFETY: reads the calling thread's last-error value; no preconditions.
    OsError::Win32 { call, code: unsafe { GetLastError() } }
}

/// Restores the console input mode it was created with.
#[cfg(windows)]
struct ModeGuard {
    handle: HANDLE,
    mode: CONSOLE_MODE,
}

#[cfg(windows)]
impl Drop for ModeGuard {
    fn drop(&mut self) {
        // SAFETY: `handle` is the process's standard input console handle, which outlives this
        // guard; `mode` is the value `GetConsoleMode` returned for it.
        unsafe {
            SetConsoleMode(self.handle, self.mode);
        }
    }
}

/// Writes `request` to stdout and returns what the terminal typed back, as text.
///
/// Returns when `done` accepts the reply so far, or at `timeout` with whatever arrived (possibly
/// nothing). A terminal that does not understand a query simply does not answer it, so the
/// caller should end its request with one every terminal answers and stop on that.
#[cfg(windows)]
pub fn query(request: &[u8], done: impl Fn(&str) -> bool, timeout: Duration) -> Result<String, OsError> {
    // SAFETY: no preconditions; the returned handle is borrowed, never closed here.
    let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Err(last_error("GetStdHandle"));
    }
    let mut mode: CONSOLE_MODE = 0;
    // SAFETY: `handle` is a valid console input handle and `mode` is a valid out pointer.
    if unsafe { GetConsoleMode(handle, &mut mode) } == 0 {
        // Not a console (redirected input): there is nobody to ask.
        return Err(last_error("GetConsoleMode"));
    }
    let _guard = ModeGuard { handle, mode };
    // SAFETY: as above; only the virtual-terminal input flag is added.
    if unsafe { SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_INPUT) } == 0 {
        return Err(last_error("SetConsoleMode"));
    }

    let mut out = std::io::stdout();
    out.write_all(request)?;
    out.flush()?;

    let deadline = Instant::now() + timeout;
    let mut reply = String::new();
    let mut units: Vec<u16> = Vec::new();
    let mut records: [INPUT_RECORD; 64] = [INPUT_RECORD::default(); 64];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        let ms = u32::try_from(left.as_millis()).unwrap_or(u32::MAX).max(1);
        // SAFETY: waits on a valid console input handle; no memory is passed.
        if unsafe { WaitForSingleObject(handle, ms) } != WAIT_OBJECT_0 {
            break;
        }
        let mut read: u32 = 0;
        // SAFETY: `records` holds 64 elements and that length is passed; `read` is a valid out
        // pointer. The console fills at most `records.len()` records.
        let ok = unsafe { ReadConsoleInputW(handle, records.as_mut_ptr(), records.len() as u32, &mut read) };
        if ok == 0 {
            return Err(last_error("ReadConsoleInputW"));
        }
        for r in records.iter().take(read as usize) {
            if u32::from(r.EventType) != KEY_EVENT {
                continue;
            }
            // SAFETY: `EventType == KEY_EVENT` says the union holds a `KEY_EVENT_RECORD`.
            let key = unsafe { r.Event.KeyEvent };
            if key.bKeyDown == 0 {
                continue;
            }
            // SAFETY: `uChar` is a union of a UTF-16 unit and an ANSI byte; the console fills
            // the wide member for `ReadConsoleInputW`.
            let unit = unsafe { key.uChar.UnicodeChar };
            if unit != 0 {
                units.push(unit);
            }
        }
        reply = String::from_utf16_lossy(&units);
        if done(&reply) {
            break;
        }
    }
    Ok(reply)
}

/// The callback [`on_interrupt`] installed.
static ON_INTERRUPT: std::sync::OnceLock<Box<dyn Fn() + Send + Sync>> = std::sync::OnceLock::new();

#[cfg(windows)]
unsafe extern "system" fn interrupt_handler(_ctrl_type: u32) -> windows_sys::core::BOOL {
    if let Some(f) = ON_INTERRUPT.get() {
        f();
    }
    // Not handled: the default handler still ends the process.
    0
}

/// Runs `f` when the console signals the process to stop: Ctrl-C (when the console still
/// processes it), Ctrl-Break, the window closing, logoff or shutdown. The process still ends
/// afterwards; `f` only gets to run first, on the console's own thread. The first callback
/// installed wins.
#[cfg(windows)]
pub fn on_interrupt(f: impl Fn() + Send + Sync + 'static) -> Result<(), OsError> {
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
    if ON_INTERRUPT.set(Box::new(f)).is_err() {
        return Ok(());
    }
    // SAFETY: registers a handler with the `HandlerRoutine` signature; it only reads a OnceLock
    // and calls a `Send + Sync` closure, and stays valid for the life of the process.
    if unsafe { SetConsoleCtrlHandler(Some(interrupt_handler), 1) } == 0 {
        return Err(last_error("SetConsoleCtrlHandler"));
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn on_interrupt(f: impl Fn() + Send + Sync + 'static) -> Result<(), OsError> {
    let _ = ON_INTERRUPT.set(Box::new(f));
    Err(OsError::Unsupported("console interrupt handlers"))
}

#[cfg(not(windows))]
pub fn query(_request: &[u8], _done: impl Fn(&str) -> bool, _timeout: Duration) -> Result<String, OsError> {
    Err(OsError::Unsupported("terminal queries"))
}

/// The refresh rate of the primary display, in Hz; 60 when it cannot be asked or says nothing
/// useful (0 and 1 mean "hardware default" to the display driver).
///
/// The primary monitor's current mode (`EnumDisplaySettingsW`), not the one the terminal window is
/// on: Windows Terminal hides the console window, so there is no window to ask about. A frame
/// rate above what the window's own monitor shows is wasted, never wrong.
pub fn display_refresh_hz() -> u32 {
    #[cfg(windows)]
    {
        use windows::core::PCWSTR;
        use windows::Win32::Graphics::Gdi::{EnumDisplaySettingsW, DEVMODEW, ENUM_CURRENT_SETTINGS};
        let mut mode = DEVMODEW { dmSize: std::mem::size_of::<DEVMODEW>() as u16, ..Default::default() };
        // SAFETY: a null device name means the current display; `mode` is a valid out pointer with
        // `dmSize` set, as the call requires.
        let ok = unsafe { EnumDisplaySettingsW(PCWSTR::null(), ENUM_CURRENT_SETTINGS, &mut mode) };
        if ok.as_bool() {
            return sane_refresh_hz(mode.dmDisplayFrequency);
        }
    }
    FALLBACK_REFRESH_HZ
}

const FALLBACK_REFRESH_HZ: u32 = 60;

/// A driver's reported rate made usable: 0 and 1 are "default", and nothing real is past 1 kHz.
pub fn sane_refresh_hz(reported: u32) -> u32 {
    match reported {
        0 | 1 => FALLBACK_REFRESH_HZ,
        hz => hz.min(1_000),
    }
}

/// Asks Windows for 1 ms timer granularity while the guard lives, so a wait of a few milliseconds
/// (one frame at 144 Hz) is not rounded up to the default 15.6 ms. Nothing off Windows.
pub struct FineTimer {
    #[cfg(windows)]
    on: bool,
}

#[cfg(windows)]
#[link(name = "winmm")]
extern "system" {
    fn timeBeginPeriod(period: u32) -> u32;
    fn timeEndPeriod(period: u32) -> u32;
}

impl FineTimer {
    pub fn start() -> FineTimer {
        #[cfg(windows)]
        {
            // SAFETY: a plain call into winmm with a period the system supports; undone in `drop`.
            let on = unsafe { timeBeginPeriod(1) } == 0;
            FineTimer { on }
        }
        #[cfg(not(windows))]
        FineTimer {}
    }
}

#[cfg(windows)]
impl Drop for FineTimer {
    fn drop(&mut self) {
        if self.on {
            // SAFETY: pairs the successful `timeBeginPeriod(1)` in `start`.
            unsafe {
                timeEndPeriod(1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_drivers_default_rate_is_sixty() {
        assert_eq!(sane_refresh_hz(0), 60);
        assert_eq!(sane_refresh_hz(1), 60);
        assert_eq!(sane_refresh_hz(144), 144);
        assert_eq!(sane_refresh_hz(100_000), 1_000);
    }

    #[test]
    fn the_display_is_never_reported_as_zero() {
        assert!(display_refresh_hz() >= 24);
    }
}
