//! Kill-on-close Job Objects.

use std::process::{Child, Command};

use crate::OsError;

#[cfg(windows)]
use windows_sys::Win32::{
    Foundation::{CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE},
    System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
    },
    System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicAccountingInformation,
        JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
        TerminateJobObject, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    },
    System::Threading::{OpenThread, ResumeThread, CREATE_SUSPENDED, THREAD_SUSPEND_RESUME},
};

/// A Job Object whose processes are all terminated when its last handle closes.
pub struct JobObject {
    /// The job handle. Owned: closed exactly once, in `Drop`.
    #[cfg(windows)]
    handle: HANDLE,
    #[cfg(not(windows))]
    _unsupported: (),
}

// SAFETY: a job handle is a kernel object reference; Win32 job calls are thread-safe and the
// handle is never closed while `&self` exists (only in `Drop`).
#[cfg(windows)]
unsafe impl Send for JobObject {}
#[cfg(windows)]
unsafe impl Sync for JobObject {}

#[cfg(windows)]
fn last_error(call: &'static str) -> OsError {
    // SAFETY: reads the calling thread's last-error value; no preconditions.
    OsError::Win32 { call, code: unsafe { GetLastError() } }
}

impl JobObject {
    /// Creates an anonymous job with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` set. The handle is
    /// not inheritable, so a child holding it can never keep its own job alive.
    #[cfg(windows)]
    pub fn new_kill_on_close() -> Result<JobObject, OsError> {
        // SAFETY: null attributes (non-inheritable handle) and a null name (anonymous job).
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(last_error("CreateJobObjectW"));
        }
        let job = JobObject { handle };
        job.set_kill_on_close(true)?;
        Ok(job)
    }

    #[cfg(not(windows))]
    pub fn new_kill_on_close() -> Result<JobObject, OsError> {
        Err(OsError::Unsupported("job objects"))
    }

    /// Puts `child` into the job; processes it starts afterwards join the job too.
    #[cfg(windows)]
    pub fn assign(&self, child: &Child) -> Result<(), OsError> {
        use std::os::windows::io::AsRawHandle;
        // SAFETY: both handles are open for the duration of the call; `child` owns its process
        // handle (PROCESS_ALL_ACCESS from CreateProcessW) and keeps it open while borrowed.
        let ok = unsafe { AssignProcessToJobObject(self.handle, child.as_raw_handle() as HANDLE) };
        if ok == 0 {
            return Err(last_error("AssignProcessToJobObject"));
        }
        Ok(())
    }

    #[cfg(not(windows))]
    pub fn assign(&self, child: &Child) -> Result<(), OsError> {
        let _ = child;
        Err(OsError::Unsupported("job objects"))
    }

    /// Terminates every process in the job now with `exit_code`. The job stays usable.
    #[cfg(windows)]
    pub fn terminate(&self, exit_code: u32) -> Result<(), OsError> {
        // SAFETY: `handle` is an open job handle with JOB_OBJECT_ALL_ACCESS.
        if unsafe { TerminateJobObject(self.handle, exit_code) } == 0 {
            return Err(last_error("TerminateJobObject"));
        }
        Ok(())
    }

    #[cfg(not(windows))]
    pub fn terminate(&self, exit_code: u32) -> Result<(), OsError> {
        let _ = exit_code;
        Err(OsError::Unsupported("job objects"))
    }

    /// How many processes are in the job right now.
    #[cfg(windows)]
    pub fn active_processes(&self) -> Result<u32, OsError> {
        let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        // SAFETY: `info` is a correctly sized, writable buffer for this information class.
        let ok = unsafe {
            QueryInformationJobObject(
                self.handle,
                JobObjectBasicAccountingInformation,
                (&mut info as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(last_error("QueryInformationJobObject"));
        }
        Ok(info.ActiveProcesses)
    }

    #[cfg(not(windows))]
    pub fn active_processes(&self) -> Result<u32, OsError> {
        Err(OsError::Unsupported("job objects"))
    }

    /// Closes the job without killing anything: kill-on-close is cleared first, so processes
    /// still in it run on (they stay in the job, which lives until they exit).
    #[cfg(windows)]
    pub fn release(self) -> Result<(), OsError> {
        let result = self.set_kill_on_close(false);
        if result.is_err() {
            // Dropping would kill the tree; leak the handle instead so release never kills.
            std::mem::forget(self);
        }
        result
    }

    #[cfg(not(windows))]
    pub fn release(self) -> Result<(), OsError> {
        Ok(())
    }

    #[cfg(windows)]
    fn set_kill_on_close(&self, on: bool) -> Result<(), OsError> {
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        if on {
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        }
        // SAFETY: `info` is a correctly sized, readable buffer for this information class. No
        // other limits are ever set on our jobs, so writing the whole struct loses nothing.
        let ok = unsafe {
            SetInformationJobObject(
                self.handle,
                JobObjectExtendedLimitInformation,
                (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            return Err(last_error("SetInformationJobObject"));
        }
        Ok(())
    }
}

impl Drop for JobObject {
    /// Closing the last handle kills every process still in the job.
    fn drop(&mut self) {
        #[cfg(windows)]
        // SAFETY: `handle` was returned by CreateJobObjectW, is owned by this value, and is
        // closed only here.
        unsafe {
            CloseHandle(self.handle);
        }
    }
}

impl std::fmt::Debug for JobObject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("JobObject")
    }
}

/// A child process alone in its own kill-on-close job. Dropping it kills the whole tree.
#[derive(Debug)]
pub struct JobChild {
    child: Child,
    job: JobObject,
}

impl JobChild {
    /// The child's process id.
    pub fn id(&self) -> u32 {
        self.child.id()
    }

    /// The child, for its pipes and `wait`/`try_wait`.
    pub fn child_mut(&mut self) -> &mut Child {
        &mut self.child
    }

    /// The job the child (and everything it started) runs in.
    pub fn job(&self) -> &JobObject {
        &self.job
    }

    /// Kills the child and every process it started, then reaps the child.
    pub fn kill_tree(&mut self) -> Result<(), OsError> {
        let terminated = self.job.terminate(1);
        if terminated.is_err() {
            // Still take the direct child down; the job close on drop gets the rest.
            let _ = self.child.kill();
        }
        self.child.wait()?;
        terminated
    }

    /// Splits into the child and its job. Dropping the job kills the tree.
    pub fn into_parts(self) -> (Child, JobObject) {
        (self.child, self.job)
    }
}

/// Spawns `cmd` suspended (`CREATE_SUSPENDED`), assigns it to a new kill-on-close job, then
/// resumes its threads, so no grandchild can start outside the job. std cannot read flags
/// back from a `Command`, so this sets the creation flags to `CREATE_SUSPENDED` alone; use
/// [`spawn_in_job_with_flags`] to add others (e.g. `CREATE_NO_WINDOW`).
pub fn spawn_in_job(cmd: &mut Command) -> Result<JobChild, OsError> {
    spawn_in_job_with_flags(cmd, 0)
}

/// [`spawn_in_job`] with extra process creation flags (`CREATE_SUSPENDED` is always added).
/// If the child cannot be put in the job or resumed it is killed and the error returned.
#[cfg(windows)]
pub fn spawn_in_job_with_flags(cmd: &mut Command, flags: u32) -> Result<JobChild, OsError> {
    use std::os::windows::process::CommandExt;
    let job = JobObject::new_kill_on_close()?;
    cmd.creation_flags(flags | CREATE_SUSPENDED);
    let mut child = cmd.spawn()?;
    let started = job.assign(&child).and_then(|()| resume_threads(child.id()));
    if let Err(e) = started {
        let _ = child.kill();
        let _ = child.wait();
        return Err(e);
    }
    Ok(JobChild { child, job })
}

#[cfg(not(windows))]
pub fn spawn_in_job_with_flags(cmd: &mut Command, flags: u32) -> Result<JobChild, OsError> {
    let _ = (cmd, flags);
    Err(OsError::Unsupported("job objects"))
}

/// Resumes every thread of process `pid` (a freshly created suspended process has one). std
/// closes the main thread handle in `spawn`, so the threads are found through a Toolhelp
/// snapshot.
#[cfg(windows)]
fn resume_threads(pid: u32) -> Result<(), OsError> {
    // SAFETY: a system-wide thread snapshot; no pointers are passed.
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snap == INVALID_HANDLE_VALUE || snap.is_null() {
        return Err(last_error("CreateToolhelp32Snapshot"));
    }
    let mut entry =
        THREADENTRY32 { dwSize: std::mem::size_of::<THREADENTRY32>() as u32, ..Default::default() };
    let mut resumed = 0u32;
    let mut failure = None;
    // SAFETY: `snap` is an open snapshot handle; `entry` is writable with `dwSize` set.
    let mut more = unsafe { Thread32First(snap, &mut entry) } != 0;
    while more {
        if entry.th32OwnerProcessID == pid {
            // SAFETY: opens a thread by id with suspend/resume rights only; checked below.
            let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
            if thread.is_null() {
                failure = Some(last_error("OpenThread"));
            } else {
                // SAFETY: `thread` is an open handle with THREAD_SUSPEND_RESUME; closed once.
                unsafe {
                    if ResumeThread(thread) == u32::MAX {
                        failure = Some(last_error("ResumeThread"));
                    } else {
                        resumed += 1;
                    }
                    CloseHandle(thread);
                }
            }
        }
        // SAFETY: as for Thread32First.
        more = unsafe { Thread32Next(snap, &mut entry) } != 0;
    }
    // SAFETY: `snap` is owned here and closed once.
    unsafe { CloseHandle(snap) };
    match (resumed, failure) {
        (0, Some(e)) => Err(e),
        (0, None) => Err(OsError::Win32 { call: "ResumeThread (no threads found)", code: 0 }),
        _ => Ok(()),
    }
}
