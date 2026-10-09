//! Process queries. Query only: never signals, suspends, renices or kills. Every handle is
//! opened with `PROCESS_QUERY_LIMITED_INFORMATION` and closed before returning.

/// True when `pid` names a running process. Opens it with `PROCESS_QUERY_LIMITED_INFORMATION`
/// and checks the exit code; access denied means it exists.
#[cfg(windows)]
#[allow(unsafe_code)]
pub fn pid_alive(pid: u32) -> bool {
    use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ACCESS_DENIED, STILL_ACTIVE};
    use windows::Win32::System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    if pid == 0 {
        return false;
    }
    // SAFETY: plain Win32 calls with valid arguments; the handle is closed before returning.
    unsafe {
        match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
            Ok(h) => {
                let mut code = 0u32;
                let ok = GetExitCodeProcess(h, &mut code).is_ok();
                let _ = CloseHandle(h);
                ok && code == STILL_ACTIVE.0 as u32
            }
            Err(_) => GetLastError() == ERROR_ACCESS_DENIED,
        }
    }
}

/// True when `pid` names a running process (`/proc` check; never signals).
#[cfg(not(windows))]
pub fn pid_alive(pid: u32) -> bool {
    pid != 0 && std::path::Path::new(&format!("/proc/{pid}")).exists()
}

/// When `pid` was created, Unix epoch ms. Lets a heartbeat's pid be told apart from a later
/// process that reused the number. `None` when it cannot be read.
#[cfg(windows)]
#[allow(unsafe_code)]
pub fn started_ms(pid: u32) -> Option<i64> {
    use windows::Win32::Foundation::{CloseHandle, FILETIME};
    use windows::Win32::System::Threading::{GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    if pid == 0 {
        return None;
    }
    // SAFETY: valid out-pointers to locals; the handle is closed before returning.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let (mut created, mut exited, mut kernel, mut user) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
        let ok = GetProcessTimes(h, &mut created, &mut exited, &mut kernel, &mut user).is_ok();
        let _ = CloseHandle(h);
        if !ok {
            return None;
        }
        let ticks = (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime);
        // 100 ns ticks since 1601-01-01 -> ms since 1970-01-01.
        Some((ticks / 10_000) as i64 - 11_644_473_600_000)
    }
}

#[cfg(not(windows))]
pub fn started_ms(_pid: u32) -> Option<i64> {
    None
}

/// The executable's file name (`python.exe`) for `pid`, when readable.
#[cfg(windows)]
#[allow(unsafe_code)]
pub fn name(pid: u32) -> Option<String> {
    use windows::core::PWSTR;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION};
    if pid == 0 {
        return None;
    }
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    // SAFETY: `buf` outlives the call and `len` holds its capacity; the handle is closed.
    let ok = unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
        let _ = CloseHandle(h);
        ok
    };
    if !ok {
        return None;
    }
    let path = String::from_utf16_lossy(&buf[..len as usize]);
    path.rsplit(['\\', '/']).next().map(str::to_string).filter(|n| !n.is_empty())
}

#[cfg(not(windows))]
pub fn name(pid: u32) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{pid}/comm")).ok().map(|s| s.trim().to_string())
}

/// Whether `pid` is alive *and* is plausibly the process that wrote a heartbeat at
/// `heartbeat_ms`: a process created more than a minute after the heartbeat reused the pid.
pub fn alive_for(pid: u32, heartbeat_ms: Option<i64>) -> bool {
    if !pid_alive(pid) {
        return false;
    }
    match (started_ms(pid), heartbeat_ms) {
        (Some(start), Some(hb)) => start <= hb + 60_000,
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_process_is_alive_and_pid_zero_is_not() {
        assert!(pid_alive(std::process::id()));
        assert!(!pid_alive(0));
    }

    #[cfg(windows)]
    #[test]
    fn reads_start_time_and_name_and_spots_pid_reuse() {
        let me = std::process::id();
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
        let start = started_ms(me).unwrap();
        assert!(start <= now && start > now - 24 * 3600 * 1000, "{start} vs {now}");
        assert!(name(me).unwrap().to_ascii_lowercase().ends_with(".exe"));
        assert!(alive_for(me, Some(now)));
        assert!(!alive_for(me, Some(start - 10 * 60_000)), "a heartbeat older than the process is not its own");
    }
}
