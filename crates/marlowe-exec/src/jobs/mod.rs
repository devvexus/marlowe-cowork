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

/// Output a background job may write before it is stopped (the monitor kills it at the cap; the
/// exit notice says so).
pub(crate) const BG_LOG_CAP: u64 = 256 << 20;

/// Most of a log an exit notice reads: the end of it, where failures print.
const SUMMARY_WINDOW: usize = 2 << 20;

/// The folder in a crew sandbox that holds its job logs.
const SANDBOX_LOGS: &str = ".marlowe-jobs";

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

/// Most unseen log a new watch looks back over for a line that already matches.
const WATCH_SCAN: u64 = 8 << 20;

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
    /// A monitor's process (track N): it is listed, announced and reported by the monitor, not as
    /// a job, and its exit queues no notice.
    pub hidden: bool,
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

    /// The log as a summary reads it: the raw text when it is small, else the condensed text
    /// (head, tail and failure blocks, already cleaned) and the true line count. Memory stays
    /// bounded by a few MiB whatever the log's size (the log holds up to its cap on disk).
    pub(crate) fn summary_source(&self, failed: bool) -> (String, Option<usize>) {
        let len = std::fs::metadata(&self.log).map(|m| m.len()).unwrap_or(0);
        if len <= summarize::WHOLE {
            return (String::from_utf8_lossy(&self.read_range(0, summarize::WHOLE as usize)).into_owned(), None);
        }
        match summarize::condense(&self.log, failed) {
            Some(c) => (c.text, Some(c.total_lines)),
            None => (String::new(), None),
        }
    }

    /// The last `max` bytes of the log (from a line start when that cuts one), and how many bytes
    /// came before them.
    pub(crate) fn read_tail(&self, max: usize) -> (Vec<u8>, u64) {
        let len = std::fs::metadata(&self.log).map(|m| m.len()).unwrap_or(0);
        if len <= max as u64 {
            return (self.read_range(0, max), 0);
        }
        let from = len - max as u64;
        let mut bytes = self.read_range(from, max);
        let mut skipped = from;
        if let Some(nl) = bytes.iter().position(|b| *b == b'\n') {
            bytes.drain(..=nl);
            skipped += nl as u64 + 1;
        }
        (bytes, skipped)
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
        if self.hidden {
            // The monitor that owns it reports the end.
            return;
        }
        // Queue the notice before announcing the exit: whoever hears of the exit finds it waiting.
        let (kind, text) = self.exit_text(code);
        self.notices.push(BgNotice { session: self.session.clone(), id: self.id.clone(), kind, text });
        if self.announced.load(Ordering::Relaxed) {
            bg_hub().publish(BgEvent::Exited { session: self.session.clone(), id: self.id.clone(), code });
        }
    }

    fn exit_text(&self, code: i32) -> (NoticeKind, String) {
        self.exit_text_in(code, SUMMARY_WINDOW)
    }

    /// The notice for the exit, summarising the last `window` bytes of the log.
    fn exit_text_in(&self, code: i32, window: usize) -> (NoticeKind, String) {
        let (killed, timed_out, flooded, cap) = {
            let st = lock(&self.st);
            (st.killed, st.timed_out, st.flooded, st.cap)
        };
        let took = fmt_secs(self.started.elapsed());
        let (tail_bytes, skipped) = self.read_tail(window);
        let raw = String::from_utf8_lossy(&tail_bytes).into_owned();
        let out = summarize::clean(&raw);
        let lines = if skipped == 0 {
            if out.is_empty() { 0 } else { out.split('\n').count() }
        } else {
            count_lines(&self.log)
        };
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
        } else if flooded {
            format!("[job {} was stopped after {took}: its output passed the {} cap: `{}`]", self.id, fmt_bytes(cap), self.command)
        } else if killed {
            format!("[job {} was stopped after {took}: `{}`]", self.id, self.command)
        } else {
            format!("[job {} exited with code {code} after {took}: `{}`]", self.id, self.command)
        };
        let budget = if code == 0 { 1_200 } else { 3_000 };
        let tail = summarize::summarize(&out, code != 0, budget);
        let mut text = head;
        if skipped > 0 {
            text.push_str(&format!("\n(the first {} of the log are not summarised here; they are in the log)", fmt_bytes(skipped)));
        }
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

    /// Starts following the job's output (replaces any earlier watch). Output the model has not
    /// seen yet (from the cursor) is checked first: when a line of it already matches `until`
    /// that line is returned and no watch is installed. Otherwise the half line the log ends on
    /// is carried over so the first live line is whole.
    pub(crate) fn watch(&self, filter: Option<Regex>, until: Option<Regex>, until_src: String, notify: bool) -> Option<String> {
        let mut st = lock(&self.st);
        let len = st.len;
        let from = st.cursor.min(len).max(len.saturating_sub(WATCH_SCAN));
        // Under the lock: the reader threads write the log and count it with the lock held.
        let seen = read_range(&self.log, from, (len - from) as usize);
        let line_start = seen.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
        if let (Some(u), true) = (until.as_ref(), line_start > 0) {
            for raw in seen[..line_start - 1].split(|b| *b == b'\n') {
                let line = summarize::clean(&String::from_utf8_lossy(raw));
                if filter.as_ref().is_some_and(|f| !f.is_match(&line)) {
                    continue;
                }
                if u.is_match(&line) {
                    return Some(line);
                }
            }
        }
        // When the scan began inside a line, the fragment is not a line start; with no newline
        // seen the log is one unfinished line from `from` on.
        st.partial = seen[line_start..].to_vec();
        st.watch = Some(Watch { filter, until, until_src, notify, buf: Vec::new(), dropped: 0 });
        None
    }
}

/// Lines in the file, counted a chunk at a time.
fn count_lines(path: &Path) -> usize {
    let Ok(mut f) = File::open(path) else { return 0 };
    let (mut n, mut last, mut buf) = (0usize, b'\n', vec![0u8; 1 << 20]);
    while let Ok(got) = f.read(&mut buf) {
        if got == 0 {
            break;
        }
        n += buf[..got].iter().filter(|b| **b == b'\n').count();
        last = buf[got - 1];
    }
    if last != b'\n' {
        n += 1;
    }
    n
}

fn fmt_bytes(n: u64) -> String {
    if n >= 1 << 20 {
        format!("{} MiB", n >> 20)
    } else if n >= 1 << 10 {
        format!("{} KiB", n >> 10)
    } else {
        format!("{n} bytes")
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
    /// A monitor's process: not listed, not announced, no exit notice (see [`Job::hidden`]).
    pub hidden: bool,
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
    /// The log folder was chosen by the host's setup (not the temp-folder default).
    placed: bool,
    /// The default folder has been moved into the sandbox, if it was going to be.
    relocated: AtomicBool,
    /// The loop's middleware has been asked for: notices reach the model between steps.
    delivering: AtomicBool,
}

impl Default for HostJobs {
    /// A host nobody set up (a crew sandbox): logs go to the temp folder until [`HostJobs::bind`]
    /// finds a sandbox to keep them in.
    fn default() -> Self {
        let mut jobs = Self::new(std::env::temp_dir().join("marlowe-jobs"));
        jobs.placed = false;
        jobs
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
            placed: true,
            relocated: AtomicBool::new(false),
            delivering: AtomicBool::new(false),
        }
    }

    /// The loop asked this host for its middleware, so job notices will be delivered.
    pub(crate) fn note_delivering(&self) {
        self.delivering.store(true, Ordering::Relaxed);
    }

    /// Whether notices reach the model between its steps in this host.
    pub(crate) fn delivers_notices(&self) -> bool {
        self.delivering.load(Ordering::Relaxed)
    }

    /// Records how the host runs commands and registers the table in the process-wide registry.
    /// Idempotent.
    pub(crate) fn bind(self: &Arc<Self>, tools: &crate::BuiltinTools) {
        *lock(&self.bound) = Some(Bound {
            root: tools.root.clone(),
            shell_dir: tools.shell_dir.clone(),
            scrub: tools.scrub.clone(),
            child_env: tools.child_env.clone(),
            port: Arc::clone(&tools.port),
            leftovers: tools.leftovers,
        });
        self.place_logs(tools);
        if !self.registered.swap(true, Ordering::SeqCst) {
            let weak: Weak<dyn BgOwner> = Arc::downgrade(self) as Weak<HostJobs>;
            bg_hub().register(weak);
        }
    }

    /// A host nobody set up keeps its logs where the model may read them back without an untrusted
    /// read: in a pinned sandbox, in a folder git ignores. (A configured host's folder is a read
    /// root already.)
    fn place_logs(&self, tools: &crate::BuiltinTools) {
        if self.placed || self.relocated.swap(true, Ordering::SeqCst) {
            return;
        }
        let Some(sandbox) = tools.shell_dir.as_deref() else { return };
        let dir = sandbox.join(SANDBOX_LOGS);
        if !tools.trusted_read(&dir.to_string_lossy()) {
            return;
        }
        if std::fs::create_dir_all(&dir).is_ok() {
            let _ = std::fs::write(dir.join(".gitignore"), "*\n");
            *lock(&self.dir) = dir;
        }
    }

    /// The folder job logs are kept in (a monitor keeps its output file beside them).
    pub(crate) fn dir(&self) -> PathBuf {
        lock(&self.dir).clone()
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
            hidden: l.hidden,
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
        if !l.foreground && !l.hidden {
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

    /// Jobs of `session` that still run, the user's `!` commands included: a session that has any
    /// must not be idled out, or its tool host would take them with it.
    pub(crate) fn running(&self, session: &str) -> usize {
        self.all().iter().filter(|j| j.session == session && j.is_running()).count()
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
        // A background job past its output cap is stopped; a foreground call does that itself.
        if job.flooded() && job.is_running() && !lock(&job.st).foreground {
            job.kill();
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
        self.all().iter().filter(|j| !j.hidden && session.is_none_or(|s| j.session == s)).map(|j| j.info()).collect()
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
        if lock(&self.bound).is_some() && lock(&self.sessions).contains(session) {
            Serves::Yes
        } else {
            Serves::No
        }
    }

    fn bg_claimable(&self, workspace: Option<&str>) -> bool {
        self.unclaimed_in(workspace) && lock(&self.sessions).is_empty()
    }

    fn bg_claim(&self, session: &str, workspace: Option<&str>) -> bool {
        if !self.unclaimed_in(workspace) {
            return false;
        }
        let mut sessions = lock(&self.sessions);
        if !sessions.is_empty() {
            return false;
        }
        sessions.insert(session.to_string());
        true
    }

    fn bg_user_shell(&self, session: &str, command: &str) -> Option<Result<String, String>> {
        if self.bg_serves(session) != Serves::Yes {
            return None;
        }
        Some(self.start_user_shell(session, command))
    }

    fn bg_running(&self, session: &str) -> usize {
        self.running(session)
    }
}

impl HostJobs {
    /// A user-session host (not a crew sandbox) whose workspace is `workspace`.
    fn unclaimed_in(&self, workspace: Option<&str>) -> bool {
        let bound = lock(&self.bound);
        let Some(b) = bound.as_ref().filter(|b| b.shell_dir.is_none() && b.leftovers == Leftovers::Release) else {
            return false;
        };
        match (b.root.as_deref(), workspace) {
            (None, None) => true,
            (Some(root), Some(ws)) => {
                let ws = Path::new(ws);
                crate::paths::inside(root, ws) && crate::paths::inside(ws, root)
            }
            _ => false,
        }
    }

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
                    hidden: false,
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
