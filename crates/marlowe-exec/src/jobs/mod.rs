//! Background shells: `run_in_background`, `job_output`, `job_watch`, `job_kill`, Ctrl+B detach and
//! the user's `!` shell (design 2.1 T15-T18). Track B.
//!
//! Every shell call is a job. [`HostJobs`] (one per tool host, in `BuiltinTools::state`) starts the
//! process through the workspace port, and one reader per stream writes its output to a log file
//! under the profile's `jobs` folder, so the whole of it stays readable whatever the model was
//! shown. A monitor thread per job notices the exit.
//!
//! - A foreground call waits on its job (timeout, interrupt, output cap), then answers. The user
//!   can detach it (`marlowe_os::job::bg_hub().detach`, Ctrl+B): the call returns at once with
//!   what the job had written, and the job runs on as a background job.
//! - A background call answers with the job's id at once. The job's exit queues a [`BgNotice`]; a
//!   `job_watch` queues the lines it follows (and its `until_regex` match) as they appear.
//!   [`JobsMiddleware`] hands the queue to the model at the next step boundary
//!   (`after_step`) or at the start of the next turn (`before_turn`); an idle session is woken by
//!   the daemon, which asks [`marlowe_os::job::BgHub::waking_sessions`] and relays the notices.
//! - Jobs die with their host: the session's tools being dropped kills every process tree. A
//!   crew host (`Leftovers::KillWithHost`) also holds processes a finished job left behind.
//!
//! The process-wide registry [`marlowe_os::job::bg_hub`] is how the daemon (which depends on no
//! executor crate) lists and kills jobs, runs `!` commands and subscribes to start, output and
//! exit events.

mod proc;
mod tools;

use std::collections::{BTreeSet, VecDeque};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use marlowe_os::job::{bg_hub, BgEvent, BgInfo, BgNotice, BgOwner, NoticeKind, Serves};
use marlowe_os::workspace::{ExecRequest, Workspace};
use regex::Regex;

use crate::shell::Leftovers;
use crate::summarize;
use proc::Proc;

pub(crate) use tools::{run_background, run_foreground, FgEnd, FgRun, JobsMiddleware};
pub use tools::{job_kill, job_output, job_watch};

/// Output a background job may write before it is stopped.
pub(crate) const BG_LOG_CAP: u64 = 256 << 20;

/// Finished jobs kept in a table (their logs stay on disk).
const KEEP_FINISHED: usize = 50;

/// Longest a job's log is kept on disk.
const LOG_DAYS: u64 = 7;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub(crate) fn fmt_secs(d: Duration) -> String {
    let s = d.as_secs_f64();
    if s < 10.0 {
        format!("{s:.1}s")
    } else if s < 120.0 {
        format!("{:.0}s", s)
    } else {
        format!("{}m{:02}s", d.as_secs() / 60, d.as_secs() % 60)
    }
}

/// Notices waiting for the model, oldest first.
#[derive(Default)]
pub(crate) struct Notices {
    inner: Mutex<NoticeQueue>,
}

#[derive(Default)]
struct NoticeQueue {
    q: VecDeque<BgNotice>,
    /// Jobs whose own tool result already told the model (a kill, a command that failed at
    /// once): nothing more is queued for them.
    told: BTreeSet<String>,
}

impl Notices {
    fn push(&self, n: BgNotice) {
        let mut g = lock(&self.inner);
        if !g.told.contains(&n.id) {
            g.q.push_back(n);
        }
    }

    fn take(&self, session: &str) -> Vec<BgNotice> {
        let mut g = lock(&self.inner);
        let (mine, rest): (Vec<_>, Vec<_>) = g.q.drain(..).partition(|n| n.session == session);
        g.q.extend(rest);
        mine
    }

    /// The model has heard about `job` already: drop what is queued and ignore what follows.
    fn forget(&self, job: &str) {
        let mut g = lock(&self.inner);
        g.q.retain(|n| n.id != job);
        g.told.insert(job.to_string());
    }

    fn waking(&self) -> Vec<String> {
        let g = lock(&self.inner);
        let set: BTreeSet<&str> = g.q.iter().filter(|n| n.kind.wakes()).map(|n| n.session.as_str()).collect();
        set.into_iter().map(str::to_string).collect()
    }
}

/// What a `job_watch` follows.
struct Watch {
    filter: Option<Regex>,
    until: Option<Regex>,
    until_src: String,
    /// Queue the lines for the model (false: only broadcast them, as for the `!` shell).
    notify: bool,
    buf: Vec<String>,
    dropped: usize,
}

/// Lines kept for one watch between hand-overs to the model.
const WATCH_LINES: usize = 400;

struct St {
    file: Option<File>,
    len: u64,
    cap: u64,
    flooded: bool,
    partial: Vec<u8>,
    /// Where `job_output` without `since` continues from.
    cursor: u64,
    exit: Option<i32>,
    /// A tool call is waiting for this job.
    foreground: bool,
    detach: bool,
    killed: bool,
    timed_out: bool,
    watch: Option<Watch>,
}

/// One process and its log.
pub(crate) struct Job {
    pub id: String,
    pub session: String,
    pub command: String,
    pub call_id: Option<String>,
    pub started: Instant,
    pub started_ms: i64,
    pub log: PathBuf,
    /// The user's own `!` command: never wakes the model.
    pub user: bool,
    leftovers: Leftovers,
    deadline: Option<Instant>,
    announced: AtomicBool,
    notices: Arc<Notices>,
    st: Mutex<St>,
    cv: Condvar,
    proc: Mutex<Option<Proc>>,
}

impl Job {
    pub(crate) fn exit_code(&self) -> Option<i32> {
        lock(&self.st).exit
    }

    pub(crate) fn is_running(&self) -> bool {
        lock(&self.st).exit.is_none()
    }

    pub(crate) fn len(&self) -> u64 {
        lock(&self.st).len
    }

    pub(crate) fn flooded(&self) -> bool {
        lock(&self.st).flooded
    }

    pub(crate) fn detach_requested(&self) -> bool {
        lock(&self.st).detach
    }

    pub(crate) fn cursor(&self) -> u64 {
        lock(&self.st).cursor
    }

    pub(crate) fn set_cursor(&self, at: u64) {
        lock(&self.st).cursor = at;
    }

    pub(crate) fn info(&self) -> BgInfo {
        let st = lock(&self.st);
        BgInfo {
            id: self.id.clone(),
            session: self.session.clone(),
            command: self.command.clone(),
            started_ms: self.started_ms,
            exit_code: st.exit,
            foreground: st.foreground && st.exit.is_none(),
            log: Some(self.log.clone()),
        }
    }

    /// Blocks up to `dur` for a state change (an exit, a detach request, output past the cap).
    pub(crate) fn wait_tick(&self, dur: Duration) {
        let st = lock(&self.st);
        if st.exit.is_some() || st.detach || st.flooded {
            return;
        }
        let _ = self.cv.wait_timeout(st, dur);
    }

    /// Blocks until the monitor has recorded the exit, or `within` passes.
    pub(crate) fn wait_exit(&self, within: Duration) -> Option<i32> {
        let end = Instant::now() + within;
        let mut st = lock(&self.st);
        while st.exit.is_none() {
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            st = self.cv.wait_timeout(st, left).map(|(g, _)| g).unwrap_or_else(|p| p.into_inner().0);
        }
        st.exit
    }

    /// The process, once the monitor is done with it (a foreground call takes its leftovers).
    pub(crate) fn take_proc(&self) -> Option<Proc> {
        lock(&self.proc).take()
    }

    /// Bytes `from..from+max` of the log.
    pub(crate) fn read_range(&self, from: u64, max: usize) -> Vec<u8> {
        read_range(&self.log, from, max)
    }

    /// The whole log (capped at 64 MiB).
    pub(crate) fn read_all(&self) -> Vec<u8> {
        self.read_range(0, 64 << 20)
    }

    /// Output from a reader thread: to the log, to the watch.
    fn ingest(&self, bytes: &[u8]) {
        let mut emitted: Vec<String> = Vec::new();
        let mut matched: Option<(String, String)> = None;
        {
            let mut st = lock(&self.st);
            let room = st.cap.saturating_sub(st.len) as usize;
            let take = bytes.len().min(room);
            if take > 0 {
                if let Some(f) = st.file.as_mut() {
                    let _ = f.write_all(&bytes[..take]);
                }
                st.len += take as u64;
            }
            if take < bytes.len() && !st.flooded {
                st.flooded = true;
                self.cv.notify_all();
            }
            if st.watch.is_some() {
                st.partial.extend_from_slice(&bytes[..take]);
                while let Some(pos) = st.partial.iter().position(|b| *b == b'\n') {
                    let raw: Vec<u8> = st.partial.drain(..=pos).collect();
                    let line = summarize::clean(&String::from_utf8_lossy(&raw));
                    let Some(w) = st.watch.as_mut() else { break };
                    if w.filter.as_ref().is_some_and(|f| !f.is_match(&line)) {
                        continue;
                    }
                    if w.buf.len() < WATCH_LINES {
                        w.buf.push(line.clone());
                    } else {
                        w.dropped += 1;
                    }
                    emitted.push(line.clone());
                    if w.until.as_ref().is_some_and(|u| u.is_match(&line)) {
                        matched = Some((w.until_src.clone(), line));
                        break;
                    }
                }
            }
        }
        if !emitted.is_empty() {
            bg_hub().publish(BgEvent::Output {
                session: self.session.clone(),
                id: self.id.clone(),
                text: emitted.join("\n"),
            });
        }
        if let Some((src, line)) = matched {
            self.flush_watch();
            lock(&self.st).watch = None;
            self.notices.push(BgNotice {
                session: self.session.clone(),
                id: self.id.clone(),
                kind: NoticeKind::Match,
                text: format!("[job {} output matched /{src}/ — the watch has ended: {line}]", self.id),
            });
        }
    }

    /// Hands the watch's buffered lines to the model's queue.
    pub(crate) fn flush_watch(&self) {
        let (lines, dropped, notify) = {
            let mut st = lock(&self.st);
            match st.watch.as_mut() {
                Some(w) if !w.buf.is_empty() => (std::mem::take(&mut w.buf), std::mem::take(&mut w.dropped), w.notify),
                _ => return,
            }
        };
        if !notify {
            return;
        }
        let mut text = format!("[job {} output]", self.id);
        let mut used = 0usize;
        let mut shown = 0usize;
        for l in &lines {
            if used + l.len() > 6_000 {
                break;
            }
            text.push('\n');
            text.push_str(l);
            used += l.len() + 1;
            shown += 1;
        }
        let hidden = lines.len() - shown + dropped;
        if hidden > 0 {
            text.push_str(&format!("\n[... {hidden} more lines; job_output {{\"id\":\"{}\"}} reads them]", self.id));
        }
        self.notices.push(BgNotice {
            session: self.session.clone(),
            id: self.id.clone(),
            kind: NoticeKind::Lines,
            text,
        });
    }

    /// The monitor saw the process exit with `code`.
    fn finish(&self, pumps: Vec<JoinHandle<()>>, code: i32) {
        // The readers finish when the pipes close; a background grandchild that holds a pipe
        // open keeps them open, so give up waiting after a moment.
        let end = Instant::now() + Duration::from_millis(400);
        while pumps.iter().any(|p| !p.is_finished()) && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(5));
        }
        self.flush_watch();
        let foreground = {
            let mut st = lock(&self.st);
            st.exit = Some(code);
            self.cv.notify_all();
            st.foreground
        };
        if foreground {
            // A tool call is waiting on this job and will answer for it.
            return;
        }
        match self.leftovers {
            Leftovers::Release => {
                if let Some(job) = lock(&self.proc).take().and_then(Proc::into_leftover) {
                    let _ = job.release();
                }
            }
            // Kept until the host is dropped (`HostJobs::drop` kills the tree).
            Leftovers::KillWithHost => {}
        }
        // Queue the notice before announcing the exit: whoever hears of the exit finds it waiting.
        let (kind, text) = self.exit_text(code);
        self.notices.push(BgNotice { session: self.session.clone(), id: self.id.clone(), kind, text });
        if self.announced.load(Ordering::Relaxed) {
            bg_hub().publish(BgEvent::Exited { session: self.session.clone(), id: self.id.clone(), code });
        }
    }

    fn exit_text(&self, code: i32) -> (NoticeKind, String) {
        let (killed, timed_out, flooded) = {
            let st = lock(&self.st);
            (st.killed, st.timed_out, st.flooded)
        };
        let took = fmt_secs(self.started.elapsed());
        let raw = String::from_utf8_lossy(&self.read_all()).into_owned();
        let out = summarize::clean(&raw);
        let lines = if out.is_empty() { 0 } else { out.split('\n').count() };
        if self.user {
            let body = summarize::summarize(&out, code != 0, 8_000);
            let shown = if body.text.is_empty() { "(no output)".to_string() } else { body.text };
            return (
                NoticeKind::UserShell,
                format!(
                    "[the user ran `{}` themselves in the terminal (exit {code}); this is what it printed]\n{shown}",
                    self.command
                ),
            );
        }
        let head = if timed_out {
            format!("[job {} was stopped after {took} at its timeout: `{}`]", self.id, self.command)
        } else if killed {
            format!("[job {} was stopped after {took}: `{}`]", self.id, self.command)
        } else if flooded {
            format!("[job {} was stopped after {took}: its output passed the {} MiB cap: `{}`]", self.id, BG_LOG_CAP >> 20, self.command)
        } else {
            format!("[job {} exited with code {code} after {took}: `{}`]", self.id, self.command)
        };
        let budget = if code == 0 { 1_200 } else { 3_000 };
        let tail = summarize::summarize(&out, code != 0, budget);
        let mut text = head;
        if !tail.text.is_empty() {
            text.push('\n');
            if tail.cut {
                text.push_str("(output, abridged)\n");
            }
            text.push_str(&tail.text);
        }
        text.push_str(&format!("\n{}", summarize::pointer(&self.log, lines)));
        (NoticeKind::Exit, text)
    }

    /// Kills the process tree now.
    pub(crate) fn kill(&self) {
        {
            let mut st = lock(&self.st);
            if st.exit.is_some() {
                return;
            }
            st.killed = true;
        }
        if let Some(p) = lock(&self.proc).as_mut() {
            p.kill_tree();
        }
    }

    /// Moves a foreground job to the background; false when it already ended or was moved.
    pub(crate) fn detach(&self) -> bool {
        {
            let mut st = lock(&self.st);
            if st.exit.is_some() || !st.foreground || st.detach {
                return false;
            }
            st.foreground = false;
            st.detach = true;
            self.cv.notify_all();
        }
        self.announce();
        true
    }

    /// Tells subscribers a background job exists.
    pub(crate) fn announce(&self) {
        if !self.announced.swap(true, Ordering::Relaxed) {
            bg_hub().publish(BgEvent::Started(self.info()));
        }
    }

    /// Starts following the job's output (replaces any earlier watch).
    pub(crate) fn watch(&self, filter: Option<Regex>, until: Option<Regex>, until_src: String, notify: bool) {
        let mut st = lock(&self.st);
        st.partial.clear();
        st.watch = Some(Watch { filter, until, until_src, notify, buf: Vec::new(), dropped: 0 });
    }
}

fn read_range(path: &Path, from: u64, max: usize) -> Vec<u8> {
    let Ok(mut f) = File::open(path) else { return Vec::new() };
    if f.seek(SeekFrom::Start(from)).is_err() {
        return Vec::new();
    }
    let mut buf = Vec::new();
    let _ = f.take(max as u64).read_to_end(&mut buf);
    buf
}

/// What a launch needs beyond the command.
pub(crate) struct Launch {
    pub req: ExecRequest,
    /// The command as the user wrote it, for listings and notices.
    pub display: String,
    pub session: String,
    pub call_id: Option<String>,
    /// A tool call waits on the job (it may be detached).
    pub foreground: bool,
    /// Output the job may write before it is stopped.
    pub cap: u64,
    pub user: bool,
    pub leftovers: Leftovers,
    /// A background job's own deadline (an explicit `timeout_ms`).
    pub timeout: Option<Duration>,
}

/// How a host's tools were configured, kept for the user's `!` shell.
struct Bound {
    root: Option<PathBuf>,
    shell_dir: Option<PathBuf>,
    scrub: Vec<String>,
    child_env: Vec<(String, String)>,
    port: Arc<dyn Workspace>,
    leftovers: Leftovers,
}

/// A tool host's jobs.
pub struct HostJobs {
    dir: Mutex<PathBuf>,
    jobs: Mutex<Vec<Arc<Job>>>,
    notices: Arc<Notices>,
    bound: Mutex<Option<Bound>>,
    /// Sessions this host serves (seen in a turn, or given a `!` command).
    sessions: Mutex<BTreeSet<String>>,
    registered: AtomicBool,
}

impl Default for HostJobs {
    fn default() -> Self {
        Self::new(std::env::temp_dir().join("marlowe-jobs"))
    }
}

impl HostJobs {
    /// Logs go under `dir` (created on first use; logs older than a week are removed now).
    pub fn new(dir: PathBuf) -> Self {
        prune_logs(&dir);
        Self {
            dir: Mutex::new(dir),
            jobs: Mutex::new(Vec::new()),
            notices: Arc::new(Notices::default()),
            bound: Mutex::new(None),
            sessions: Mutex::new(BTreeSet::new()),
            registered: AtomicBool::new(false),
        }
    }

    /// Records how the host runs commands and registers the table in the process-wide registry.
    /// Idempotent.
    pub(crate) fn bind(self: &Arc<Self>, tools: &crate::BuiltinTools) {
        {
            let mut b = lock(&self.bound);
            if b.is_none() {
                *b = Some(Bound {
                    root: tools.root.clone(),
                    shell_dir: tools.shell_dir.clone(),
                    scrub: tools.scrub.clone(),
                    child_env: tools.child_env.clone(),
                    port: Arc::clone(&tools.port),
                    leftovers: tools.leftovers,
                });
            }
        }
        if !self.registered.swap(true, Ordering::SeqCst) {
            let weak: Weak<dyn BgOwner> = Arc::downgrade(self) as Weak<HostJobs>;
            bg_hub().register(weak);
        }
    }

    /// Drops the queued notices of one job (its result already told the model).
    pub(crate) fn forget_notices(&self, job: &str) {
        self.notices.forget(job);
    }

    pub(crate) fn note_session(&self, session: &str) {
        lock(&self.sessions).insert(session.to_string());
    }

    pub(crate) fn find(&self, id: &str) -> Option<Arc<Job>> {
        lock(&self.jobs).iter().find(|j| j.id == id).cloned()
    }

    pub(crate) fn all(&self) -> Vec<Arc<Job>> {
        lock(&self.jobs).clone()
    }

    pub(crate) fn remove(&self, id: &str) {
        lock(&self.jobs).retain(|j| j.id != id);
    }

    /// Where a log for a run that ran through a port without background support is written.
    pub(crate) fn spill_log(&self, text: &str) -> Option<PathBuf> {
        let dir = lock(&self.dir).clone();
        std::fs::create_dir_all(&dir).ok()?;
        let path = dir.join(format!("j{}-{}.log", NEXT_ID.fetch_add(1, Ordering::Relaxed), now_ms()));
        std::fs::write(&path, text).ok()?;
        Some(path)
    }

    /// Starts the process and its monitor.
    pub(crate) fn launch(&self, port: &dyn Workspace, l: Launch) -> std::io::Result<Arc<Job>> {
        let dir = lock(&self.dir).clone();
        std::fs::create_dir_all(&dir)?;
        let n = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let id = format!("j{n}");
        let started_ms = now_ms();
        let log = dir.join(format!("{id}-{started_ms}.log"));
        let file = File::create(&log)?;
        let job = Arc::new(Job {
            id: id.clone(),
            session: l.session,
            command: l.display,
            call_id: l.call_id,
            started: Instant::now(),
            started_ms,
            log,
            user: l.user,
            leftovers: l.leftovers,
            deadline: l.timeout.map(|t| Instant::now() + t),
            announced: AtomicBool::new(false),
            notices: Arc::clone(&self.notices),
            st: Mutex::new(St {
                file: Some(file),
                len: 0,
                cap: l.cap,
                flooded: false,
                partial: Vec::new(),
                cursor: 0,
                exit: None,
                foreground: l.foreground,
                detach: false,
                killed: false,
                timed_out: false,
                watch: None,
            }),
            cv: Condvar::new(),
            proc: Mutex::new(None),
        });
        let sink = Arc::clone(&job);
        let started = proc::start(port, &l.req, Arc::new(move |b: &[u8]| sink.ingest(b)))?;
        *lock(&job.proc) = Some(started.proc);
        let for_monitor = Arc::clone(&job);
        std::thread::Builder::new()
            .name(format!("marlowe-job-{id}"))
            .spawn(move || monitor(for_monitor, started.pumps))?;
        {
            let mut jobs = lock(&self.jobs);
            jobs.push(Arc::clone(&job));
            let finished = jobs.iter().filter(|j| !j.is_running()).count();
            if finished > KEEP_FINISHED {
                let mut drop_n = finished - KEEP_FINISHED;
                jobs.retain(|j| {
                    if drop_n > 0 && !j.is_running() {
                        drop_n -= 1;
                        false
                    } else {
                        true
                    }
                });
            }
        }
        if !l.foreground {
            job.announce();
        }
        Ok(job)
    }

    /// The notices for `session`: buffered watch lines first, then the queue in order.
    pub(crate) fn drain(&self, session: &str) -> Vec<BgNotice> {
        for j in self.all() {
            if j.session == session {
                j.flush_watch();
            }
        }
        self.notices.take(session)
    }

    /// Whether any job of `session` still runs.
    pub(crate) fn running(&self, session: &str) -> usize {
        self.all().iter().filter(|j| j.session == session && j.is_running() && !j.user).count()
    }
}

fn prune_logs(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let cutoff = std::time::SystemTime::now() - Duration::from_secs(LOG_DAYS * 24 * 3600);
    for e in rd.flatten() {
        let old = e.metadata().and_then(|m| m.modified()).is_ok_and(|t| t < cutoff);
        if old && e.path().extension().is_some_and(|x| x == "log") {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

fn monitor(job: Arc<Job>, pumps: Vec<JoinHandle<()>>) {
    let mut offset = 0u64;
    loop {
        if let Some(end) = job.deadline {
            if Instant::now() >= end && job.is_running() {
                lock(&job.st).timed_out = true;
                job.kill();
            }
        }
        let step = {
            let mut proc = lock(&job.proc);
            let Some(p) = proc.as_mut() else { return };
            let more = p.drain(&mut offset);
            if !more.is_empty() {
                job.ingest(&more);
            }
            p.try_wait()
        };
        match step {
            Ok(Some(code)) => {
                // A port's last output may still be buffered.
                if let Some(p) = lock(&job.proc).as_mut() {
                    let more = p.drain(&mut offset);
                    if !more.is_empty() {
                        job.ingest(&more);
                    }
                }
                job.finish(pumps, code);
                return;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(15)),
            Err(_) => {
                job.finish(pumps, -1);
                return;
            }
        }
    }
}

impl Drop for HostJobs {
    /// A host that goes away takes its jobs with it: every process tree is killed.
    fn drop(&mut self) {
        for j in lock(&self.jobs).drain(..) {
            if let Some(p) = lock(&j.proc).as_mut() {
                p.kill_tree();
            }
        }
    }
}

impl BgOwner for HostJobs {
    fn bg_list(&self, session: Option<&str>) -> Vec<BgInfo> {
        self.all().iter().filter(|j| session.is_none_or(|s| j.session == s)).map(|j| j.info()).collect()
    }

    fn bg_kill(&self, id: &str) -> Option<bool> {
        let job = self.find(id)?;
        let was_running = job.is_running();
        job.kill();
        Some(was_running)
    }

    fn bg_detach(&self, session: &str, call_id: Option<&str>) -> Option<String> {
        let fg: Vec<Arc<Job>> =
            self.all().into_iter().filter(|j| j.session == session && j.info().foreground).collect();
        let job = match call_id.and_then(|c| fg.iter().find(|j| j.call_id.as_deref() == Some(c))) {
            Some(j) => Some(j),
            None if fg.len() == 1 && call_id.is_none_or(|_| fg[0].call_id.is_none()) => fg.first(),
            None => None,
        }?;
        job.detach().then(|| job.id.clone())
    }

    fn bg_waking(&self) -> Vec<String> {
        self.notices.waking()
    }

    fn bg_take(&self, session: &str) -> Vec<BgNotice> {
        // Only notices the daemon is waking the session for; lines wait for the next step.
        self.drain(session)
    }

    fn bg_serves(&self, session: &str) -> Serves {
        if lock(&self.bound).is_none() {
            return Serves::No;
        }
        let s = lock(&self.sessions);
        if s.contains(session) {
            Serves::Yes
        } else if s.is_empty() {
            // A host that has not run a turn yet: the first `!` of a fresh session lands here.
            Serves::Maybe
        } else {
            Serves::No
        }
    }

    fn bg_user_shell(&self, session: &str, command: &str) -> Option<Result<String, String>> {
        if self.bg_serves(session) == Serves::No {
            return None;
        }
        self.note_session(session);
        Some(self.start_user_shell(session, command))
    }

    fn bg_running(&self, session: &str) -> usize {
        self.running(session)
    }
}

impl HostJobs {
    /// Runs the user's `!` command as a job whose output is broadcast and, when it ends, queued
    /// as context for the next turn.
    fn start_user_shell(&self, session: &str, command: &str) -> Result<String, String> {
        let (req, port, leftovers) = {
            let b = lock(&self.bound);
            let b = b.as_ref().ok_or("the shell is not set up for this session yet")?;
            let cmd = crate::shell::bash_command(command).unwrap_or_else(|_| crate::shell::powershell_command(command));
            let mut req = ExecRequest::from_command(&cmd);
            req.spec.env_remove.extend(b.scrub.iter().cloned());
            req.spec.env.extend(b.child_env.iter().cloned());
            req.spec.cwd = b.shell_dir.clone().or_else(|| b.root.clone());
            (req, Arc::clone(&b.port), b.leftovers)
        };
        let job = self
            .launch(
                port.as_ref(),
                Launch {
                    req,
                    display: command.to_string(),
                    session: session.to_string(),
                    call_id: None,
                    foreground: false,
                    cap: BG_LOG_CAP,
                    user: true,
                    leftovers,
                    timeout: Some(Duration::from_millis(crate::shell::SHELL_TIMEOUT_MAX_MS)),
                },
            )
            .map_err(|e| format!("could not start the command: {e}"))?;
        job.watch(None, None, String::new(), false);
        Ok(job.id.clone())
    }
}

#[cfg(test)]
mod tests;
