//! Finding, starting and stopping the daemon.

use std::fs::OpenOptions;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use marlowe_proto::DaemonInfo;

use crate::client::Client;

/// The running daemon's info, if `daemon.json` exists and the daemon answers the handshake.
pub fn live(home: &Path) -> Option<DaemonInfo> {
    let info = DaemonInfo::read(home).ok().flatten()?;
    Client::connect(&info, "probe").ok().map(|_| info)
}

/// Returns a live daemon, spawning one (detached, no window) when `daemon.json` is stale.
pub fn ensure(home: &Path) -> Result<DaemonInfo, String> {
    if let Some(info) = live(home) {
        return Ok(info);
    }
    std::fs::create_dir_all(home).map_err(|e| format!("cannot create {}: {e}", home.display()))?;
    // A stale file would make the wait below connect to a dead port; the new daemon rewrites it.
    if let Ok(Some(old)) = DaemonInfo::read(home) {
        let _ = DaemonInfo::remove_if_ours(home, old.pid);
    }
    spawn(home).map_err(|e| format!("cannot start the daemon: {e}"))?;
    let end = Instant::now() + Duration::from_secs(15);
    while Instant::now() < end {
        std::thread::sleep(Duration::from_millis(150));
        if let Some(info) = live(home) {
            return Ok(info);
        }
    }
    Err(format!(
        "the daemon did not come up within 15 s; see {}",
        home.join("daemon.log").display()
    ))
}

fn spawn(home: &Path) -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    let log = OpenOptions::new().create(true).append(true).open(home.join("daemon.log"))?;
    let mut cmd = Command::new(exe);
    cmd.arg("daemon")
        .env(marlowe_proto::paths::HOME_ENV, home)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        stop_std_handle_inheritance();
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }
    cmd.spawn().map(|_| ())
}

/// `Command::spawn` passes `bInheritHandles = TRUE`, so the detached daemon would inherit this
/// process's std handles. When those are a pipe (`marlowe ask .. | more`, a redirect), the reader
/// never sees EOF until the daemon exits. Clearing the flag is enough: std hands the child its own
/// inheritable duplicates of whatever `Stdio` it was given.
#[cfg(windows)]
#[allow(unsafe_code)]
fn stop_std_handle_inheritance() {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{SetHandleInformation, HANDLE_FLAG_INHERIT};
    let handles = [
        std::io::stdin().as_raw_handle(),
        std::io::stdout().as_raw_handle(),
        std::io::stderr().as_raw_handle(),
    ];
    for h in handles.into_iter().filter(|h| !h.is_null()) {
        // SAFETY: `h` is one of this process's std handles, open for the process lifetime. The call
        // only clears a flag on it; a non-handle value (no console) fails harmlessly.
        unsafe {
            SetHandleInformation(h, HANDLE_FLAG_INHERIT, 0);
        }
    }
}
