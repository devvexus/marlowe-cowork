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
        AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob, JobObjectBasicAccountingInformation,
        JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
        TerminateJobObject, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    },
    System::Threading::{
        OpenProcess, OpenThread, ResumeThread, CREATE_SUSPENDED, PROCESS_QUERY_LIMITED_INFORMATION,
        THREAD_SUSPEND_RESUME,
    },
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

    /// Whether process `pid` runs in this job (the child and everything it started). A pid that
    /// cannot be opened (gone, or another user's) is not in it.
    #[cfg(windows)]
    pub fn contains(&self, pid: u32) -> bool {
        if pid == 0 {
            return false;
        }
        // SAFETY: opens a process by id for limited query only; checked for null, closed below.
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if process.is_null() {
            return false;
        }
        let mut inside = 0;
        // SAFETY: both handles are open; `inside` is a writable BOOL.
        let ok = unsafe { IsProcessInJob(process, self.handle, &mut inside) };
        // SAFETY: `process` was opened above and is closed exactly once.
        unsafe { CloseHandle(process) };
        ok != 0 && inside != 0
    }

    #[cfg(not(windows))]
    pub fn contains(&self, pid: u32) -> bool {
        let _ = pid;
        false
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

// ---------------------------------------------------------------------------------------------
// Background shell jobs: the process-wide registry (track B).
//
// `marlowe-exec` runs the jobs (`jobs/`); this registry is what the daemon, which depends on
// no executor crate, talks to: list and kill jobs, detach a foreground shell (Ctrl+B), run the
// user's `!` shell, subscribe to start/output/exit events, and collect the notices an idle
// session must be woken for. Every tool host registers its table once; the registry holds it
// weakly, so a host that is dropped (its jobs are killed) simply disappears.
// ---------------------------------------------------------------------------------------------

use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock, Weak};

/// One background job (or a foreground shell call that can still be detached).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BgInfo {
    pub id: String,
    pub session: String,
    pub command: String,
    pub started_ms: i64,
    /// `None` while it runs.
    pub exit_code: Option<i32>,
    /// A foreground call the user may still detach with Ctrl+B.
    pub foreground: bool,
    /// The full output on disk.
    pub log: Option<std::path::PathBuf>,
}

/// What the registry broadcasts to subscribers (the daemon turns these into wire events).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BgEvent {
    Started(BgInfo),
    /// New output from a watched job or the user's `!` shell, in order.
    Output { session: String, id: String, text: String },
    Exited { session: String, id: String, code: i32 },
}

/// Why a notice exists. [`NoticeKind::wakes`] says whether an idle session is woken for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeKind {
    /// The job exited.
    Exit,
    /// A `job_watch` line batch.
    Lines,
    /// A `job_watch` `until_regex` matched.
    Match,
    /// The user's `!` command finished: context for the next turn, never a wake.
    UserShell,
}

impl NoticeKind {
    pub fn wakes(self) -> bool {
        matches!(self, NoticeKind::Exit | NoticeKind::Match)
    }
}

/// Something the model should hear about a job, already worded for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BgNotice {
    pub session: String,
    pub id: String,
    pub kind: NoticeKind,
    pub text: String,
}

/// Whether a table runs the commands of a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Serves {
    /// It has run a turn or a command for the session, or was given the session ([`BgOwner::bg_claim`]).
    Yes,
    No,
}

/// A table of jobs the registry can reach. Implemented by `marlowe-exec`'s per-host job table.
pub trait BgOwner: Send + Sync {
    /// Jobs of `session` (all when `None`).
    fn bg_list(&self, session: Option<&str>) -> Vec<BgInfo>;
    /// Kills job `id` if this table has it (`None`: not here; `Some(false)`: already gone).
    fn bg_kill(&self, id: &str) -> Option<bool>;
    /// Moves a running foreground shell of `session` to the background: the one for `call_id`,
    /// else the session's only one. Returns the job id.
    fn bg_detach(&self, session: &str, call_id: Option<&str>) -> Option<String>;
    /// Sessions with a notice that wakes them.
    fn bg_waking(&self) -> Vec<String>;
    /// Removes and returns the pending notices of `session` (all kinds).
    fn bg_take(&self, session: &str) -> Vec<BgNotice>;
    /// Whether this table serves `session` (see [`Serves`]).
    fn bg_serves(&self, session: &str) -> Serves;
    /// Whether this table has served no session and runs commands in `workspace`: a candidate for
    /// [`BgOwner::bg_claim`].
    fn bg_claimable(&self, _workspace: Option<&str>) -> bool {
        false
    }
    /// Takes `session` as the table's own when it is still a candidate (see
    /// [`BgOwner::bg_claimable`]): a session that has not run a turn yet, whose runner has just
    /// built this host.
    fn bg_claim(&self, _session: &str, _workspace: Option<&str>) -> bool {
        false
    }
    /// Runs the user's `!` command in `session`'s workspace as a job; returns its id. `None`:
    /// this table does not serve `session`.
    fn bg_user_shell(&self, session: &str, command: &str) -> Option<Result<String, String>>;
    /// How many jobs of `session` are running (the daemon should not idle the session out).
    fn bg_running(&self, session: &str) -> usize;
}

/// The process-wide registry; see [`bg_hub`].
#[derive(Default)]
pub struct BgHub {
    owners: Mutex<Vec<Weak<dyn BgOwner>>>,
    subs: Mutex<Vec<Sender<BgEvent>>>,
}

/// The one registry in this process.
pub fn bg_hub() -> &'static BgHub {
    static HUB: OnceLock<BgHub> = OnceLock::new();
    HUB.get_or_init(BgHub::default)
}

fn guard<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl BgHub {
    /// Adds a table. Held weakly.
    pub fn register(&self, owner: Weak<dyn BgOwner>) {
        guard(&self.owners).push(owner);
    }

    fn live(&self) -> Vec<Arc<dyn BgOwner>> {
        let mut owners = guard(&self.owners);
        owners.retain(|w| w.strong_count() > 0);
        owners.iter().filter_map(Weak::upgrade).collect()
    }

    /// A stream of every event from now on. Drop the receiver to unsubscribe.
    pub fn subscribe(&self) -> Receiver<BgEvent> {
        let (tx, rx) = channel();
        guard(&self.subs).push(tx);
        rx
    }

    /// Broadcasts `event` (called by the tables).
    pub fn publish(&self, event: BgEvent) {
        let mut subs = guard(&self.subs);
        subs.retain(|s| s.send(event.clone()).is_ok());
    }

    /// Jobs of `session` (all when `None`), running first, newest first.
    pub fn list(&self, session: Option<&str>) -> Vec<BgInfo> {
        let mut all: Vec<BgInfo> = self.live().iter().flat_map(|o| o.bg_list(session)).collect();
        all.sort_by(|a, b| a.exit_code.is_some().cmp(&b.exit_code.is_some()).then(b.started_ms.cmp(&a.started_ms)));
        all
    }

    /// Kills job `id`; true when it was running.
    pub fn kill(&self, id: &str) -> bool {
        self.live().iter().find_map(|o| o.bg_kill(id)).unwrap_or(false)
    }

    /// Ctrl+B: moves the running foreground shell call (`call_id`, else the session's only one)
    /// to the background and returns the new job's id.
    pub fn detach(&self, session: &str, call_id: Option<&str>) -> Option<String> {
        self.live().iter().find_map(|o| o.bg_detach(session, call_id))
    }

    /// Sessions that have a notice that should wake them.
    pub fn waking_sessions(&self) -> Vec<String> {
        let mut s: Vec<String> = self.live().iter().flat_map(|o| o.bg_waking()).collect();
        s.sort();
        s.dedup();
        s
    }

    /// Removes and returns `session`'s pending notices.
    pub fn take_notices(&self, session: &str) -> Vec<BgNotice> {
        self.live().iter().flat_map(|o| o.bg_take(session)).collect()
    }

    /// Whether some table serves `session`.
    pub fn serves(&self, session: &str) -> bool {
        self.live().iter().any(|o| o.bg_serves(session) == Serves::Yes)
    }

    /// Gives a session that no table serves yet to the idle table that runs commands in
    /// `workspace`; true when some table serves it afterwards. Only when exactly one table
    /// qualifies: with several (another session's host built but not yet used) nothing says which
    /// one belongs to `session`, and the wrong one would run the command out of the session's
    /// sight, so it stays unclaimed until the host binds its session when it is built.
    pub fn claim(&self, session: &str, workspace: Option<&str>) -> bool {
        let owners = self.live();
        if owners.iter().any(|o| o.bg_serves(session) == Serves::Yes) {
            return true;
        }
        let mut candidates = owners.iter().filter(|o| o.bg_claimable(workspace));
        match (candidates.next(), candidates.next()) {
            (Some(only), None) => only.bg_claim(session, workspace),
            _ => false,
        }
    }

    /// Runs the user's `!` command for `session`; the job id, or why not. Only a table that serves
    /// the session runs it: [`BgHub::claim`] first for a session that has not run a turn.
    pub fn user_shell(&self, session: &str, command: &str) -> Result<String, String> {
        self.live()
            .iter()
            .find(|o| o.bg_serves(session) == Serves::Yes)
            .and_then(|o| o.bg_user_shell(session, command))
            .unwrap_or_else(|| Err("this session has no shell to run it in".to_string()))
    }

    /// Running jobs of `session` across tables.
    pub fn running(&self, session: &str) -> usize {
        self.live().iter().map(|o| o.bg_running(session)).sum()
    }
}

#[cfg(test)]
mod bg_tests {
    use super::*;

    struct Fake;
    impl BgOwner for Fake {
        fn bg_list(&self, _: Option<&str>) -> Vec<BgInfo> {
            Vec::new()
        }
        fn bg_kill(&self, id: &str) -> Option<bool> {
            (id == "x").then_some(true)
        }
        fn bg_detach(&self, _: &str, _: Option<&str>) -> Option<String> {
            None
        }
        fn bg_waking(&self) -> Vec<String> {
            vec!["s".into()]
        }
        fn bg_take(&self, _: &str) -> Vec<BgNotice> {
            Vec::new()
        }
        fn bg_serves(&self, _: &str) -> Serves {
            Serves::No
        }
        fn bg_user_shell(&self, _: &str, _: &str) -> Option<Result<String, String>> {
            None
        }
        fn bg_running(&self, _: &str) -> usize {
            2
        }
    }

    #[test]
    fn the_registry_holds_tables_weakly_and_fans_events_out() {
        let hub = BgHub::default();
        let owner: Arc<dyn BgOwner> = Arc::new(Fake);
        hub.register(Arc::downgrade(&owner));
        assert!(hub.kill("x") && !hub.kill("y"));
        assert_eq!(hub.waking_sessions(), ["s"]);
        assert_eq!(hub.running("s"), 2);
        let rx = hub.subscribe();
        hub.publish(BgEvent::Exited { session: "s".into(), id: "j1".into(), code: 0 });
        assert!(matches!(rx.try_recv(), Ok(BgEvent::Exited { code: 0, .. })));
        drop(owner);
        assert!(hub.waking_sessions().is_empty(), "a dropped table disappears");
        drop(rx);
        hub.publish(BgEvent::Exited { session: "s".into(), id: "j2".into(), code: 1 });
        assert!(guard(&hub.subs).is_empty(), "a dropped subscriber is pruned");
        assert!(hub.user_shell("s", "x").is_err());
    }

    #[test]
    fn only_exits_and_matches_wake_an_idle_session() {
        assert!(NoticeKind::Exit.wakes() && NoticeKind::Match.wakes());
        assert!(!NoticeKind::Lines.wakes() && !NoticeKind::UserShell.wakes());
    }
}
