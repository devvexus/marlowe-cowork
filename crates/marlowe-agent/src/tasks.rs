//! The background task registry (design 3.12, rows BG1-BG4, BG11): one table of everything the
//! conductor started and is not waiting for, with typed ids (`job_*`, `agent_*`, `wf_*`, `mon_*`,
//! `cron_*`), one stop and one output surface, and the queue that carries a task's news back into
//! a conversation.
//!
//! This file is the seam: the types, the [`TaskRegistry`] trait and a complete in-memory
//! [`MemoryTaskRegistry`]. Owners plug in without editing it:
//!
//! - A task the owner starts and tracks itself (a monitor, a workflow) calls
//!   [`TaskRegistry::register`] with an optional [`TaskControl`] (how to stop it, how to read its
//!   output) and reports changes with [`TaskRegistry::set_status`].
//! - A task whose state already lives elsewhere (B's job table, the crew's run store) adds a
//!   [`TaskSource`] that lists it on demand, so nothing is mirrored and nothing goes stale.
//! - News (a job exit, a monitor line, a cron fire, a workflow result, a message from another
//!   session) goes in with [`TaskRegistry::inject`]. The daemon's middleware drains it at a step
//!   boundary with [`TaskRegistry::take_injections`]; the waker set with
//!   [`TaskRegistry::set_waker`] starts a turn when the session is idle. Nothing here ever runs
//!   inside a model step, and nothing blocks.
//!
//! The daemon owns one registry and installs it as the [`Tasks`] service of every session, so a
//! tool reaches it through `CallCtx::scope.services`.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

pub use marlowe_proto::{TaskInfo, TaskKind, TaskStatus};
use marlowe_proto::SessionId;
use serde::{Deserialize, Serialize};

/// Most bytes one [`TaskRegistry::output`] call returns.
pub const MAX_OUTPUT_CHUNK: usize = 256 * 1024;
/// Finished tasks kept for `task_list` before the oldest are dropped.
pub const KEEP_FINISHED: usize = 500;
/// Injections held per session before the oldest are dropped.
pub const MAX_PENDING_INJECTIONS: usize = 512;

/// How a task is stopped and, if it is not backed by a file, how its output is read.
pub trait TaskControl: Send + Sync {
    /// Stops it: kills the shell tree, cancels the workflow (no new agent starts, in-flight agents
    /// are cancelled), disarms the monitor. Must not block for long; the registry marks the task
    /// stopped when this returns `Ok`.
    fn stop(&self) -> Result<(), String>;

    /// Output that is not in the task's `output_file`. `None`: use the file.
    fn output(&self, _since: Option<u64>, _filter: Option<&str>) -> Option<Result<TaskChunk, String>> {
        None
    }
}

/// What starts a task in the registry.
pub struct NewTask {
    pub kind: TaskKind,
    /// The owner's own id (`j7`, a run id); the task's id is `kind.id(raw)`. `None`: the registry
    /// allocates one. Registering an id that exists replaces it (a monitor re-armed under the
    /// same id).
    pub raw_id: Option<String>,
    pub session: Option<SessionId>,
    pub title: String,
    pub output_file: Option<String>,
    pub control: Option<Arc<dyn TaskControl>>,
}

impl NewTask {
    pub fn new(kind: TaskKind, title: impl Into<String>) -> Self {
        Self { kind, raw_id: None, session: None, title: title.into(), output_file: None, control: None }
    }

    pub fn session(mut self, session: Option<SessionId>) -> Self {
        self.session = session;
        self
    }

    pub fn raw_id(mut self, raw: impl Into<String>) -> Self {
        self.raw_id = Some(raw.into());
        self
    }

    pub fn output_file(mut self, path: impl Into<String>) -> Self {
        self.output_file = Some(path.into());
        self
    }

    pub fn control(mut self, control: Arc<dyn TaskControl>) -> Self {
        self.control = Some(control);
        self
    }
}

/// A window of a task's output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskChunk {
    pub text: String,
    /// The byte offset to ask from next.
    pub next_since: u64,
    /// The task ended and nothing is left to read.
    pub done: bool,
}

/// Tasks whose state lives with their owner. Ids are typed already.
pub trait TaskSource: Send + Sync {
    fn list(&self) -> Vec<TaskInfo>;

    /// Stops `id`. `None`: not this source's.
    fn stop(&self, _id: &str) -> Option<Result<(), String>> {
        None
    }

    /// Output of `id` that is not in its `output_file`. `None`: not this source's, or use the file.
    fn output(&self, _id: &str, _since: Option<u64>, _filter: Option<&str>) -> Option<Result<TaskChunk, String>> {
        None
    }
}

/// Calls back on every task change (the daemon broadcasts `TaskUpdated`). Must not block.
pub type TaskListener = Arc<dyn Fn(&TaskInfo) + Send + Sync>;
/// Called after an injection is queued for the session (the daemon starts a turn if it is idle).
pub type InjectionWaker = Arc<dyn Fn(&SessionId) + Send + Sync>;

/// Where news came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InjectionSource {
    Job,
    Monitor,
    Cron,
    Wakeup,
    Workflow,
    Agent,
    Notice,
    /// Another session's `send_message`.
    Message,
}

impl InjectionSource {
    /// The `source` a hook sees on the turn this starts (X22).
    pub fn as_str(self) -> &'static str {
        match self {
            InjectionSource::Job => "job",
            InjectionSource::Monitor => "monitor",
            InjectionSource::Cron => "cron",
            InjectionSource::Wakeup => "wakeup",
            InjectionSource::Workflow => "workflow",
            InjectionSource::Agent => "agent",
            InjectionSource::Notice => "notice",
            InjectionSource::Message => "message",
        }
    }
}

/// One piece of news for a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Injection {
    pub session: SessionId,
    pub source: InjectionSource,
    /// The task it concerns, when it has one.
    pub task: Option<String>,
    pub text: String,
    /// It carries content the user did not write and the harness did not produce: web text in a
    /// workflow's return value, another session's words. The session is tainted before acting on
    /// it, and memory ingests it at agent trust, never user trust (non-negotiable 2).
    pub untrusted: bool,
    /// Where it came from, for a message: `session s-12`. Kept as lineage.
    pub from: Option<String>,
    pub at_ms: i64,
}

impl Injection {
    pub fn new(session: SessionId, source: InjectionSource, text: impl Into<String>) -> Self {
        Self { session, source, task: None, text: text.into(), untrusted: false, from: None, at_ms: now_ms() }
    }

    pub fn task(mut self, id: impl Into<String>) -> Self {
        self.task = Some(id.into());
        self
    }

    pub fn untrusted(mut self, untrusted: bool) -> Self {
        self.untrusted = untrusted;
        self
    }

    pub fn from(mut self, lineage: impl Into<String>) -> Self {
        self.from = Some(lineage.into());
        self
    }
}

/// The registry. Every method is cheap and non-blocking; listeners and the waker run after the
/// registry's lock is released.
pub trait TaskRegistry: Send + Sync {
    /// Adds a running task and returns its typed id.
    fn register(&self, new: NewTask) -> String;

    /// Moves a task to `status` (terminal states are final: a stopped task stays stopped).
    /// `detail` replaces the status detail when given.
    fn set_status(&self, id: &str, status: TaskStatus, detail: Option<String>);

    /// Replaces the title (a cron's next fire, a workflow's phase).
    fn set_title(&self, id: &str, title: &str);

    fn get(&self, id: &str) -> Option<TaskInfo>;

    /// The session's tasks plus daemon-wide ones, newest first; `None`: every task.
    fn list(&self, session: Option<&SessionId>) -> Vec<TaskInfo>;

    /// Stops a task. `Err` says why not (unknown, already finished, not stoppable).
    fn stop(&self, id: &str) -> Result<TaskInfo, String>;

    /// Reads the task's output from byte offset `since` (default 0), keeping only lines matching
    /// the regular expression `filter`. Cuts at a line end while the task runs.
    fn output(&self, id: &str, since: Option<u64>, filter: Option<&str>) -> Result<TaskChunk, String>;

    fn add_source(&self, source: Arc<dyn TaskSource>);
    fn subscribe(&self, listener: TaskListener);

    /// Queues news for a session and wakes it.
    fn inject(&self, injection: Injection);
    /// Removes and returns up to `max` pending injections for the session, oldest first.
    fn take_injections(&self, session: &SessionId, max: usize) -> Vec<Injection>;
    /// Puts injections `take_injections` returned back at the front of the session's queue, in
    /// order (a drain that could not hand them all over). It does not wake the session.
    fn requeue_front(&self, session: &SessionId, items: Vec<Injection>);
    fn pending_injections(&self, session: &SessionId) -> usize;
    fn set_waker(&self, waker: InjectionWaker);

    /// The session ended: its pending news is dropped. Its tasks stay (a job outlives a client);
    /// owners stop the session-scoped ones themselves.
    fn clear_session(&self, session: &SessionId);
}

/// The registry as a session service (`Services::get::<Tasks>()`).
#[derive(Clone)]
pub struct Tasks(pub Arc<dyn TaskRegistry>);

impl std::fmt::Debug for Tasks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Tasks")
    }
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

struct Entry {
    info: TaskInfo,
    control: Option<Arc<dyn TaskControl>>,
}

#[derive(Default)]
struct Inner {
    tasks: HashMap<String, Entry>,
    sources: Vec<Arc<dyn TaskSource>>,
    listeners: Vec<TaskListener>,
    waker: Option<InjectionWaker>,
    queues: HashMap<SessionId, VecDeque<Injection>>,
    next: u64,
}

/// The in-memory registry the daemon runs and tests use.
#[derive(Default)]
pub struct MemoryTaskRegistry {
    inner: Mutex<Inner>,
}

impl MemoryTaskRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn notify(&self, listeners: &[TaskListener], info: &TaskInfo) {
        for l in listeners {
            l(info);
        }
    }

    /// Looks `id` up in the pushed entries, then in the sources.
    fn find(&self, id: &str) -> Option<TaskInfo> {
        let sources = {
            let g = self.lock();
            if let Some(e) = g.tasks.get(id) {
                return Some(e.info.clone());
            }
            g.sources.clone()
        };
        sources.iter().flat_map(|s| s.list()).find(|t| t.id == id)
    }
}

impl TaskRegistry for MemoryTaskRegistry {
    fn register(&self, new: NewTask) -> String {
        let (info, listeners) = {
            let mut g = self.lock();
            let id = match new.raw_id.as_deref().map(str::trim).filter(|r| !r.is_empty()) {
                Some(raw) => new.kind.id(raw),
                None => {
                    g.next += 1;
                    new.kind.id(&g.next.to_string())
                }
            };
            let info = TaskInfo {
                id: id.clone(),
                kind: new.kind,
                session: new.session,
                title: new.title,
                status: TaskStatus::Running,
                started_ms: now_ms(),
                ended_ms: None,
                output_file: new.output_file,
                detail: None,
            };
            g.tasks.insert(id, Entry { info: info.clone(), control: new.control });
            prune(&mut g.tasks);
            (info, g.listeners.clone())
        };
        self.notify(&listeners, &info);
        info.id
    }

    fn set_status(&self, id: &str, status: TaskStatus, detail: Option<String>) {
        let (info, listeners) = {
            let mut g = self.lock();
            let Some(e) = g.tasks.get_mut(id) else { return };
            if e.info.status.is_terminal() {
                return;
            }
            e.info.status = status;
            if status.is_terminal() {
                e.info.ended_ms = Some(now_ms());
                e.control = None;
            }
            if detail.is_some() {
                e.info.detail = detail;
            }
            (e.info.clone(), g.listeners.clone())
        };
        self.notify(&listeners, &info);
    }

    fn set_title(&self, id: &str, title: &str) {
        let (info, listeners) = {
            let mut g = self.lock();
            let Some(e) = g.tasks.get_mut(id) else { return };
            if e.info.title == title {
                return;
            }
            e.info.title = title.to_string();
            (e.info.clone(), g.listeners.clone())
        };
        self.notify(&listeners, &info);
    }

    fn get(&self, id: &str) -> Option<TaskInfo> {
        self.find(id)
    }

    fn list(&self, session: Option<&SessionId>) -> Vec<TaskInfo> {
        let (mut all, sources) = {
            let g = self.lock();
            (g.tasks.values().map(|e| e.info.clone()).collect::<Vec<_>>(), g.sources.clone())
        };
        for s in sources {
            for t in s.list() {
                if !all.iter().any(|a| a.id == t.id) {
                    all.push(t);
                }
            }
        }
        if let Some(s) = session {
            all.retain(|t| t.session.as_ref().map_or(true, |owner| owner == s));
        }
        all.sort_by(|a, b| b.started_ms.cmp(&a.started_ms).then_with(|| a.id.cmp(&b.id)));
        all
    }

    fn stop(&self, id: &str) -> Result<TaskInfo, String> {
        enum Plan {
            Control(Arc<dyn TaskControl>),
            Sources(Vec<Arc<dyn TaskSource>>),
        }
        let plan = {
            let g = self.lock();
            match g.tasks.get(id) {
                Some(e) if e.info.status.is_terminal() => {
                    return Err(format!("`{id}` has already finished ({})", e.info.status.as_str()))
                }
                Some(e) => match &e.control {
                    Some(c) => Plan::Control(c.clone()),
                    None => return Err(format!("`{id}` cannot be stopped")),
                },
                None => Plan::Sources(g.sources.clone()),
            }
        };
        match plan {
            Plan::Control(c) => {
                c.stop()?;
                self.set_status(id, TaskStatus::Stopped, None);
                self.get(id).ok_or_else(|| format!("no task `{id}`"))
            }
            Plan::Sources(sources) => {
                let before = self.find(id);
                if let Some(t) = &before {
                    if t.status.is_terminal() {
                        return Err(format!("`{id}` has already finished ({})", t.status.as_str()));
                    }
                }
                for s in sources {
                    if let Some(r) = s.stop(id) {
                        r?;
                        let mut t = self.find(id).or(before).ok_or_else(|| format!("no task `{id}`"))?;
                        if !t.status.is_terminal() {
                            t.status = TaskStatus::Stopped;
                            t.ended_ms = Some(now_ms());
                        }
                        let listeners = self.lock().listeners.clone();
                        self.notify(&listeners, &t);
                        return Ok(t);
                    }
                }
                Err(if before.is_some() { format!("`{id}` cannot be stopped") } else { format!("no task `{id}`; task_list shows what is running") })
            }
        }
    }

    fn output(&self, id: &str, since: Option<u64>, filter: Option<&str>) -> Result<TaskChunk, String> {
        let re = match filter.map(str::trim).filter(|f| !f.is_empty()) {
            Some(f) => Some(regex::Regex::new(f).map_err(|e| format!("filter is not a valid regular expression: {e}"))?),
            None => None,
        };
        let (control, sources) = {
            let g = self.lock();
            (g.tasks.get(id).and_then(|e| e.control.clone()), g.sources.clone())
        };
        let info = self.find(id).ok_or_else(|| format!("no task `{id}`; task_list shows what is running"))?;
        if let Some(r) = control.and_then(|c| c.output(since, filter)) {
            return r;
        }
        for s in sources {
            if let Some(r) = s.output(id, since, filter) {
                return r;
            }
        }
        let file = info.output_file.as_deref().ok_or_else(|| format!("`{id}` has no output file"))?;
        read_chunk(Path::new(file), since.unwrap_or(0), re.as_ref(), info.status.is_terminal())
    }

    fn add_source(&self, source: Arc<dyn TaskSource>) {
        self.lock().sources.push(source);
    }

    fn subscribe(&self, listener: TaskListener) {
        self.lock().listeners.push(listener);
    }

    fn inject(&self, injection: Injection) {
        let (session, waker) = {
            let mut g = self.lock();
            let q = g.queues.entry(injection.session.clone()).or_default();
            if q.len() >= MAX_PENDING_INJECTIONS {
                q.pop_front();
            }
            let session = injection.session.clone();
            q.push_back(injection);
            (session, g.waker.clone())
        };
        if let Some(w) = waker {
            w(&session);
        }
    }

    fn take_injections(&self, session: &SessionId, max: usize) -> Vec<Injection> {
        let mut g = self.lock();
        let Some(q) = g.queues.get_mut(session) else { return Vec::new() };
        let n = max.min(q.len());
        let out: Vec<Injection> = q.drain(..n).collect();
        if q.is_empty() {
            g.queues.remove(session);
        }
        out
    }

    fn requeue_front(&self, session: &SessionId, items: Vec<Injection>) {
        if items.is_empty() {
            return;
        }
        let mut g = self.lock();
        let q = g.queues.entry(session.clone()).or_default();
        for i in items.into_iter().rev() {
            q.push_front(i);
        }
    }

    fn pending_injections(&self, session: &SessionId) -> usize {
        self.lock().queues.get(session).map_or(0, VecDeque::len)
    }

    fn set_waker(&self, waker: InjectionWaker) {
        self.lock().waker = Some(waker);
    }

    fn clear_session(&self, session: &SessionId) {
        self.lock().queues.remove(session);
    }
}

/// Keeps the newest [`KEEP_FINISHED`] finished tasks.
fn prune(tasks: &mut HashMap<String, Entry>) {
    let finished = tasks.values().filter(|e| e.info.status.is_terminal()).count();
    if finished <= KEEP_FINISHED {
        return;
    }
    let mut done: Vec<(i64, String)> = tasks
        .values()
        .filter(|e| e.info.status.is_terminal())
        .map(|e| (e.info.ended_ms.unwrap_or(e.info.started_ms), e.info.id.clone()))
        .collect();
    done.sort();
    for (_, id) in done.into_iter().take(finished - KEEP_FINISHED) {
        tasks.remove(&id);
    }
}

/// Reads `path` from byte `since`: at most [`MAX_OUTPUT_CHUNK`] bytes, cut at the last line end
/// while the task runs (so `next_since` never splits a line), and with `filter` applied per line.
fn read_chunk(path: &Path, since: u64, filter: Option<&regex::Regex>, finished: bool) -> Result<TaskChunk, String> {
    let mut f = std::fs::File::open(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let len = f.metadata().map_err(|e| e.to_string())?.len();
    if since >= len {
        return Ok(TaskChunk { text: String::new(), next_since: len, done: finished });
    }
    f.seek(SeekFrom::Start(since)).map_err(|e| e.to_string())?;
    let want = ((len - since) as usize).min(MAX_OUTPUT_CHUNK);
    let mut buf = vec![0u8; want];
    f.read_exact(&mut buf).map_err(|e| e.to_string())?;
    let reached_end = since + want as u64 >= len;
    // While the task runs, a half-written last line waits for its end.
    if !(finished && reached_end) {
        if let Some(i) = buf.iter().rposition(|&b| b == b'\n') {
            buf.truncate(i + 1);
        } else if reached_end && !finished {
            buf.clear();
        }
    }
    // Never end inside a UTF-8 sequence (a cut at the chunk limit can split one).
    if let Err(e) = std::str::from_utf8(&buf) {
        if e.error_len().is_none() && !(finished && reached_end) {
            buf.truncate(e.valid_up_to());
        }
    }
    let next_since = since + buf.len() as u64;
    let text = String::from_utf8_lossy(&buf).into_owned();
    let text = match filter {
        Some(re) => text.split_inclusive('\n').filter(|l| re.is_match(l.trim_end_matches(['\r', '\n']))).collect(),
        None => text,
    };
    Ok(TaskChunk { text, next_since, done: finished && next_since >= len })
}

#[cfg(test)]
mod tests;
