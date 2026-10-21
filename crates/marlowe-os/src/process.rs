//! Other processes' command lines, read without starting anything (no WMI, no PowerShell): the
//! planner reads the running model runner's own argv (`llama-server.exe … -np 1`) to know how many
//! requests the engine really serves at once.

/// `(pid, command line)` of every running process whose image is `image` (`llama-server.exe`;
/// case-insensitive). A process that cannot be opened (another user's, protected) is skipped.
#[cfg(windows)]
pub fn command_lines(image: &str) -> Vec<(u32, String)> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
    };

    // SAFETY: a system-wide process snapshot; no pointers are passed.
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snap == INVALID_HANDLE_VALUE || snap.is_null() {
        return Vec::new();
    }
    let mut entry = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
    let mut pids = Vec::new();
    // SAFETY: `snap` is an open snapshot handle; `entry` is writable with `dwSize` set.
    let mut more = unsafe { Process32FirstW(snap, &mut entry) } != 0;
    while more {
        let len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
        if String::from_utf16_lossy(&entry.szExeFile[..len]).eq_ignore_ascii_case(image) {
            pids.push(entry.th32ProcessID);
        }
        // SAFETY: as for Process32FirstW.
        more = unsafe { Process32NextW(snap, &mut entry) } != 0;
    }
    // SAFETY: `snap` is owned here and closed once.
    unsafe { CloseHandle(snap) };
    pids.into_iter().filter_map(|pid| command_line(pid).map(|c| (pid, c))).collect()
}

#[cfg(not(windows))]
pub fn command_lines(image: &str) -> Vec<(u32, String)> {
    let _ = image;
    Vec::new()
}

/// Process `pid`'s command line (`ProcessCommandLineInformation`, Windows 8.1 and later).
#[cfg(windows)]
fn command_line(pid: u32) -> Option<String> {
    use windows_sys::Wdk::System::Threading::{NtQueryInformationProcess, ProcessCommandLineInformation};
    use windows_sys::Win32::Foundation::{CloseHandle, UNICODE_STRING};
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

    // SAFETY: opens a process by id for limited query only; checked for null, closed below.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return None;
    }
    // A UNICODE_STRING header followed by the text it points into (at most 32,767 UTF-16 units);
    // u64 keeps it aligned.
    let mut buf = vec![0u64; 8200];
    let mut needed = 0u32;
    let bytes = (buf.len() * 8) as u32;
    // SAFETY: `process` is open with query rights; `buf` is writable for `bytes` bytes and
    // aligned for UNICODE_STRING; `needed` is a writable u32.
    let status = unsafe { NtQueryInformationProcess(process, ProcessCommandLineInformation, buf.as_mut_ptr().cast(), bytes, &mut needed) };
    // SAFETY: `process` was opened above and is closed exactly once.
    unsafe { CloseHandle(process) };
    if status < 0 {
        return None;
    }
    // SAFETY: on success the buffer starts with a UNICODE_STRING whose `Buffer` points at
    // `Length` bytes inside `buf`, which is still alive and unchanged.
    let text = unsafe {
        let us = &*(buf.as_ptr() as *const UNICODE_STRING);
        if us.Buffer.is_null() || us.Length == 0 {
            return None;
        }
        let start = buf.as_ptr() as usize;
        let at = us.Buffer as usize;
        let n = usize::from(us.Length) / 2;
        if at < start || at + n * 2 > start + buf.len() * 8 {
            return None;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(us.Buffer, n))
    };
    Some(text)
}

#[cfg(test)]
mod tests {
    #[test]
    #[cfg(windows)]
    fn this_test_finds_its_own_command_line() {
        let exe = std::env::current_exe().unwrap();
        let image = exe.file_name().unwrap().to_string_lossy().into_owned();
        let me = super::command_lines(&image);
        let mine = me.iter().find(|(pid, _)| *pid == std::process::id()).expect("this process is listed");
        assert!(mine.1.to_ascii_lowercase().contains(&image.to_ascii_lowercase()), "{mine:?}");
        assert!(super::command_lines("no-such-image-marlowe.exe").is_empty());
    }
}
