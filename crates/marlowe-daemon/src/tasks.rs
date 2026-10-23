//! The daemon's background task registry (design 3.12): one [`MemoryTaskRegistry`] for the
//! daemon's life, reached with [`registry`], installed as the [`Tasks`] service of every session
//! (`task_ops::services`), and mirrored to clients as `Event::TaskUpdated`.
//!
//! Tasks that live with their owner are listed through [`TaskSource`]s, so nothing is mirrored and
//! nothing goes stale: B's job table (`job_*`, through `marlowe_os::job::bg_hub`), the crew's runs
//! (`agent_*`, and `wf_*` for a run executing a workflow script) and each session's background
//! subagents. Monitors, schedules and W's workflows register themselves.
//!
//! News goes in with [`TaskRegistry::inject`] and comes out at a step boundary:
//! [`DrainMiddleware`] hands a session everything pending, batched into one message, after a step
//! and before it would finish. A session that is idle is woken by [`Waker`], a thread that starts
//! a relay turn (not the user's words, so memory never ingests it at user trust). Nothing here
//! runs inside a model step and nothing blocks the conductor.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use crossbeam_channel::{unbounded, Receiver, Sender};
use marlowe_agent::subagent::Subagents;
use marlowe_agent::{
    Conversation, FinalFlow, Injection, MemoryTaskRegistry, StepInfo, TaskChunk, TaskInfo, TaskKind, TaskRegistry,
    TaskSource, TaskStatus, ToolMiddleware,
};
use marlowe_crew::{CrewRunInfo, CrewRunStatus, RunId};
use marlowe_model::Message;
use marlowe_os::job::{bg_hub, BgEvent, BgInfo};
use marlowe_proto::{Event, NoticeSource, SessionId, Severity};

use crate::daemon::Core;
use crate::{lock, now_ms};

/// Injections one drain takes (what the character budget cannot carry goes back to the queue).
const DRAIN_MAX: usize = 64;
/// Most characters of news one step carries; the rest waits for the next.
const DRAIN_CHARS: usize = 12_000;
/// How long the waker waits between looks at sessions that still have news.
const WAKE_POLL: Duration = Duration::from_millis(500);
/// Tries to deliver news to a session the daemon cannot load before it is dropped.
const WAKE_TRIES: u32 = 6;

pub(crate) struct TaskState {
    registry: Arc<MemoryTaskRegistry>,
    broadcasting: AtomicBool,
    installed: AtomicBool,
    /// Each session's latest subagent service, for listing and stopping its background agents.
    agents: Mutex<HashMap<SessionId, Subagents>>,
    wake: Mutex<Option<Sender<SessionId>>>,
    /// When each background job was first seen finished (the job table keeps no end time).
    ended: Mutex<HashMap<String, i64>>,
}

impl Default for TaskState {
    fn default() -> Self {
        Self {
            registry: Arc::default(),
            broadcasting: AtomicBool::new(false),
            installed: AtomicBool::new(false),
            agents: Mutex::new(HashMap::new()),
            wake: Mutex::new(None),
            ended: Mutex::new(HashMap::new()),
        }
    }
}

fn state(core: &Core) -> Arc<TaskState> {
    core.ext.get_or_init(TaskState::default)
}

/// The daemon's registry.
pub(crate) fn registry(core: &Core) -> Arc<MemoryTaskRegistry> {
    state(core).registry.clone()
}

/// The registry as the trait object the services carry.
pub(crate) fn handle(core: &Core) -> Arc<dyn TaskRegistry> {
    registry(core)
}

/// Remembers a session's subagent service (called when the session's services are built).
pub(crate) fn note_subagents(core: &Core, session: &SessionId, subagents: Subagents) {
    lock(&state(core).agents).insert(session.clone(), subagents);
}

/// This session's subagents, as the last services built for it hold them.
pub(crate) fn subagents_of(core: &Core, session: &SessionId) -> Option<Subagents> {
    lock(&state(core).agents).get(session).cloned()
}

/// Broadcasts every task change to the clients, once. The listener holds the core weakly, so a
/// registry kept alive by a service never keeps a stopped daemon alive.
pub(crate) fn start_broadcast(core: &Arc<Core>) {
    let st = state(core);
    if st.broadcasting.swap(true, Ordering::SeqCst) {
        return;
    }
    let weak = Arc::downgrade(core);
    st.registry.subscribe(Arc::new(move |task| {
        if let Some(core) = weak.upgrade() {
            core.bus.publish(Event::TaskUpdated(task.clone()));
            task_notice(&core, task);
        }
    }));
}

/// Installs the sources, the waker and the job-event follower, once. `task_ops::on_boot` calls it.
pub(crate) fn install(core: &Arc<Core>) {
    let st = state(core);
    if st.installed.swap(true, Ordering::SeqCst) {
        return;
    }
    start_broadcast(core);
    let weak = Arc::downgrade(core);
    st.registry.add_source(Arc::new(JobSource { core: weak.clone() }));
    st.registry.add_source(Arc::new(CrewSource { core: weak.clone() }));
    st.registry.add_source(Arc::new(AgentSource { core: weak.clone() }));
    let (tx, rx) = unbounded();
    *lock(&st.wake) = Some(tx.clone());
    st.registry.set_waker(Arc::new(move |session: &SessionId| {
        let _ = tx.send(session.clone());
    }));
    let for_waker = weak.clone();
    spawn(core, "marlowe-task-waker", move || Waker::default().run(for_waker, rx));
    spawn(core, "marlowe-task-jobs", move || follow_jobs(weak));
}

fn spawn(core: &Arc<Core>, name: &str, f: impl FnOnce() + Send + 'static) {
    match std::thread::Builder::new().name(name.into()).spawn(f) {
        Ok(h) => core.add_writer(h),
        Err(e) => eprintln!("marlowe: {name} did not start: {e}"),
    }
}

pub(crate) fn clip_line(s: &str, max: usize) -> String {
    let line = s.lines().next().unwrap_or("").trim();
    if line.chars().count() <= max {
        return line.to_string();
    }
    let cut: String = line.chars().take(max).collect();
    format!("{}…", cut.trim_end())
}

// ---------------------------------------------------------------- sources

/// B's job table as `job_*` tasks.
struct JobSource {
    core: Weak<Core>,
}

fn job_task(core: &Core, j: &BgInfo) -> TaskInfo {
    let ended = lock(&state(core).ended).get(&j.id).copied();
    let (status, detail) = match j.exit_code {
        None => (TaskStatus::Running, None),
        Some(0) => (TaskStatus::Done, Some("exit 0".to_string())),
        Some(c) => (TaskStatus::Failed, Some(format!("exit {c}"))),
    };
    TaskInfo {
        id: TaskKind::Job.id(&j.id),
        kind: TaskKind::Job,
        session: Some(SessionId(j.session.clone())),
        title: j.command.clone(),
        status,
        started_ms: j.started_ms,
        ended_ms: j.exit_code.map(|_| ended.unwrap_or_else(now_ms)),
        output_file: j.log.as_ref().map(|p| p.display().to_string()),
        detail,
    }
}

impl TaskSource for JobSource {
    fn list(&self) -> Vec<TaskInfo> {
        let Some(core) = self.core.upgrade() else { return Vec::new() };
        bg_hub().list(None).iter().filter(|j| !j.foreground).map(|j| job_task(&core, j)).collect()
    }

    fn stop(&self, id: &str) -> Option<Result<(), String>> {
        let (TaskKind::Job, raw) = TaskKind::split(id)? else { return None };
        let known = bg_hub().list(None).into_iter().find(|j| j.id == raw)?;
        if let Some(code) = known.exit_code {
            return Some(Err(format!("`{id}` has already finished (exit {code})")));
        }
        bg_hub().kill(raw);
        Some(Ok(()))
    }
}

/// Crew runs as `agent_*` tasks, and the runs that execute a workflow script as `wf_*`.
struct CrewSource {
    core: Weak<Core>,
}

fn run_status(s: CrewRunStatus) -> TaskStatus {
    match s {
        CrewRunStatus::Queued | CrewRunStatus::NeedsYou | CrewRunStatus::Paused => TaskStatus::Waiting,
        CrewRunStatus::Running => TaskStatus::Running,
        CrewRunStatus::Done => TaskStatus::Done,
        CrewRunStatus::Failed => TaskStatus::Failed,
        CrewRunStatus::Cancelled => TaskStatus::Stopped,
    }
}

/// A run's transcript file, `<home>\jobs\agent_<run>.jsonl`: one JSON line per step, written as
/// the steps come, so `read` and `grep` follow a run while it works (BG3).
pub(crate) fn transcript_path(home: &std::path::Path, run: &RunId) -> std::path::PathBuf {
    home.join("jobs").join(format!("agent_{}.jsonl", run.as_str()))
}

/// Longest stretch of one step's text a `task_output` shows (its end: where the step has got to).
const STEP_TEXT_SHOWN: usize = 500;

/// The transcript's lines, one per streamed piece, as one line per step and action: the pieces
/// joined, the end of a long step kept.
pub(crate) fn coalesce_transcript(lines: &str) -> String {
    let mut blocks: Vec<(i64, String, String)> = Vec::new();
    for line in lines.lines().filter(|l| !l.trim().is_empty()) {
        let (step, action, text) = match serde_json::from_str::<serde_json::Value>(line) {
            Ok(v) => (
                v["step"].as_i64().unwrap_or(0),
                v["action"].as_str().unwrap_or("").to_string(),
                v["text"].as_str().unwrap_or("").to_string(),
            ),
            Err(_) => (0, String::new(), line.to_string()),
        };
        match blocks.last_mut() {
            Some((s, a, t)) if *s == step && *a == action => t.push_str(&text),
            _ => blocks.push((step, action, text)),
        }
    }
    let mut out = String::new();
    for (step, action, text) in blocks {
        let text = text.trim();
        let n = text.chars().count();
        let shown: String = if n > STEP_TEXT_SHOWN { format!("...{}", text.chars().skip(n - STEP_TEXT_SHOWN).collect::<String>()) } else { text.to_string() };
        out.push_str(&format!("[step {step} {action}] {}{}\n", shown.replace('\n', " "), if n > STEP_TEXT_SHOWN { format!(" ({n} chars)") } else { String::new() }));
    }
    out
}

/// Appends a step to its run's transcript file. A step streams its text in pieces: a line is one
/// piece, in order.
pub(crate) fn crew_step(core: &Core, run: &RunId, step: u32, action: &str, text: Option<&str>) {
    use std::io::Write;
    let path = transcript_path(&core.home, run);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) else { return };
    let line = serde_json::json!({ "step": step, "action": action, "text": text.unwrap_or("") });
    let _ = writeln!(f, "{line}");
}

pub(crate) fn run_task(r: &CrewRunInfo) -> TaskInfo {
    let kind = if r.workflow.is_some() { TaskKind::Workflow } else { TaskKind::Agent };
    let detail = match r.status {
        CrewRunStatus::Running => r.current_action.clone().or_else(|| Some(format!("step {}", r.step))),
        CrewRunStatus::Queued | CrewRunStatus::NeedsYou | CrewRunStatus::Paused => {
            Some(format!("{}{}", r.status.as_str().replace('_', " "), r.note.as_deref().map(|n| format!(": {n}")).unwrap_or_default()))
        }
        _ => r.note.clone(),
    };
    TaskInfo {
        id: kind.id(r.id.as_str()),
        kind,
        session: r.session.clone(),
        title: match &r.workflow {
            Some(w) => w.name.clone().unwrap_or_else(|| r.title.clone()),
            None => format!("1 specialist · {}: {}", r.role, r.title),
        },
        status: run_status(r.status),
        started_ms: r.created_ms,
        ended_ms: r.status.is_terminal().then_some(r.updated_ms),
        output_file: None,
        detail,
    }
}

/// [`run_task`] with the transcript file it has, if it has one yet.
fn run_task_in(home: &std::path::Path, r: &CrewRunInfo) -> TaskInfo {
    let mut t = run_task(r);
    let path = transcript_path(home, &r.id);
    t.output_file = path.is_file().then(|| path.display().to_string());
    t
}

fn crew_run(core: &Core, id: &str) -> Option<RunId> {
    let (kind, raw) = TaskKind::split(id)?;
    if !matches!(kind, TaskKind::Agent | TaskKind::Workflow) {
        return None;
    }
    let link = core.crew.get()?;
    let run = RunId(raw.to_string());
    link.supervisor.get(&run).ok().flatten().map(|_| run)
}

impl TaskSource for CrewSource {
    fn list(&self) -> Vec<TaskInfo> {
        let Some(core) = self.core.upgrade() else { return Vec::new() };
        let Some(link) = core.crew.get() else { return Vec::new() };
        link.supervisor.list(200).unwrap_or_default().iter().map(|r| run_task_in(&core.home, r)).collect()
    }

    fn stop(&self, id: &str) -> Option<Result<(), String>> {
        let core = self.core.upgrade()?;
        let run = crew_run(&core, id)?;
        let link = core.crew.get()?;
        Some(link.supervisor.cancel(&run).map_err(|e| e.to_string()))
    }

    fn output(&self, id: &str, since: Option<u64>, filter: Option<&str>) -> Option<Result<TaskChunk, String>> {
        let core = self.core.upgrade()?;
        let run = crew_run(&core, id)?;
        let link = core.crew.get()?;
        // A run with a transcript file is read from it, but as the model's `task_output` it is
        // joined into one block per step: the file holds every streamed piece on a line of its own
        // (a poll of a running team returned 32k characters of `{"text":"I"}` lines).
        let path = transcript_path(&core.home, &run);
        if path.is_file() {
            let raw = std::fs::read(&path).ok()?;
            let done = link.supervisor.get(&run).ok().flatten().is_some_and(|r| r.status.is_terminal());
            let start = (since.unwrap_or(0) as usize).min(raw.len());
            let end = raw.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1).max(start);
            let text = String::from_utf8_lossy(&raw[start..end]);
            let blocks = coalesce_transcript(&text);
            let re = match filter.map(str::trim).filter(|f| !f.is_empty()).map(regex::Regex::new).transpose() {
                Ok(re) => re,
                Err(e) => return Some(Err(format!("filter is not a valid regular expression: {e}"))),
            };
            let shown: String = match &re {
                Some(re) => blocks.split_inclusive('\n').filter(|l| re.is_match(l.trim_end())).collect(),
                None => blocks,
            };
            return Some(Ok(TaskChunk { text: shown, next_since: end as u64, done }));
        }
        let (info, steps) = match link.supervisor.transcript(&run) {
            Ok(t) => t,
            Err(e) => return Some(Err(e.to_string())),
        };
        let mut text = String::new();
        for s in &steps {
            text.push_str(&format!("[step {} · {}] {}\n", s.step, s.role, s.action));
            if !s.text.trim().is_empty() {
                text.push_str(s.text.trim_end());
                text.push('\n');
            }
        }
        let re = match filter.map(str::trim).filter(|f| !f.is_empty()).map(regex::Regex::new).transpose() {
            Ok(re) => re,
            Err(e) => return Some(Err(format!("filter is not a valid regular expression: {e}"))),
        };
        let start = (since.unwrap_or(0) as usize).min(text.len());
        let start = (start..=text.len()).find(|i| text.is_char_boundary(*i)).unwrap_or(text.len());
        let window = &text[start..];
        let shown: String = match &re {
            Some(re) => window.split_inclusive('\n').filter(|l| re.is_match(l.trim_end())).collect(),
            None => window.to_string(),
        };
        Some(Ok(TaskChunk { text: shown, next_since: text.len() as u64, done: info.status.is_terminal() }))
    }
}

/// A session's background subagents as `agent_<session>.<agent-n>` tasks.
struct AgentSource {
    core: Weak<Core>,
}

fn agent_raw(session: &SessionId, agent: &str) -> String {
    format!("{session}.{agent}")
}

/// `step 4 · read src/lib.rs` while it runs; what to do with it once it has finished.
fn agent_detail(a: &marlowe_agent::subagent::AgentSummary) -> String {
    if !a.running {
        return format!("finished; send_message to {} continues it", a.id);
    }
    let p = &a.progress;
    let step = if p.steps == 0 { "starting".to_string() } else { format!("step {}", p.steps) };
    if p.doing.is_empty() { step } else { format!("{step} · {}", clip_line(&p.doing, 60)) }
}

impl TaskSource for AgentSource {
    fn list(&self) -> Vec<TaskInfo> {
        let Some(core) = self.core.upgrade() else { return Vec::new() };
        let agents: Vec<(SessionId, Subagents)> = lock(&state(&core).agents).iter().map(|(s, a)| (s.clone(), a.clone())).collect();
        let mut out = Vec::new();
        for (session, subs) in agents {
            for a in subs.agents().into_iter().filter(|a| a.background) {
                out.push(TaskInfo {
                    id: TaskKind::Agent.id(&agent_raw(&session, &a.id)),
                    kind: TaskKind::Agent,
                    session: Some(session.clone()),
                    title: a.label.clone(),
                    status: if a.running { TaskStatus::Running } else { TaskStatus::Done },
                    started_ms: a.progress.started_ms,
                    ended_ms: (!a.running).then_some(a.progress.ended_ms).filter(|e| *e > 0),
                    output_file: None,
                    detail: Some(agent_detail(&a)),
                });
            }
        }
        out
    }

    fn stop(&self, id: &str) -> Option<Result<(), String>> {
        let core = self.core.upgrade()?;
        let (TaskKind::Agent, raw) = TaskKind::split(id)? else { return None };
        let (session, agent) = raw.split_once('.')?;
        let subs = subagents_of(&core, &SessionId(session.to_string()))?;
        if !subs.knows(agent) {
            return None;
        }
        Some(if subs.cancel_one(agent) { Ok(()) } else { Err(format!("`{id}` is not running")) })
    }

    fn output(&self, id: &str, _since: Option<u64>, _filter: Option<&str>) -> Option<Result<TaskChunk, String>> {
        let (TaskKind::Agent, raw) = TaskKind::split(id)? else { return None };
        let (_, agent) = raw.split_once('.')?;
        agent.starts_with("agent-").then(|| Err("a subagent's report comes to you when it finishes; its transcript is not kept".to_string()))
    }
}

// ---------------------------------------------------------------- notices and job events

/// A finished task is a notice (BG9), so `notices` and the brief see jobs, monitors, schedules,
/// runs and workflows beside the Sentinel's own.
pub(crate) fn task_notice(core: &Core, task: &TaskInfo) {
    if !task.status.is_terminal() {
        return;
    }
    let Some(sentinel) = &core.sentinel else { return };
    let severity = match task.status {
        TaskStatus::Failed => Severity::Attention,
        TaskStatus::Done => Severity::Good,
        _ => Severity::Info,
    };
    let kind = task.status.as_str();
    let title = format!("{} {}: {}", task.id, kind, clip_line(&task.title, 80));
    let detail = task.detail.clone().unwrap_or_default();
    let draft = marlowe_sentinel::NoticeDraft::new(NoticeSource::Task, severity, kind, title, detail, task.id.clone());
    if let Ok(Some(n)) = sentinel.raise(draft) {
        core.bus.publish(Event::Notice(n));
    }
}

/// Follows B's job events: a job's start and exit are `TaskUpdated` broadcasts and, at its exit, a
/// notice. (The model hears of an exit through B's own notices; this is for the panel and the
/// inbox.)
fn follow_jobs(core: Weak<Core>) {
    let events = bg_hub().subscribe();
    loop {
        let ev = match events.recv_timeout(Duration::from_millis(500)) {
            Ok(ev) => ev,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => match core.upgrade() {
                Some(c) if !c.is_shutting_down() => continue,
                _ => break,
            },
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };
        let Some(core) = core.upgrade() else { break };
        if core.is_shutting_down() {
            break;
        }
        match ev {
            BgEvent::Started(j) if !j.foreground => {
                core.bus.publish(Event::TaskUpdated(job_task(&core, &j)));
            }
            BgEvent::Exited { session, id, code } => {
                lock(&state(&core).ended).insert(id.clone(), now_ms());
                if let Some(j) = bg_hub().list(Some(&session)).into_iter().find(|j| j.id == id) {
                    let mut t = job_task(&core, &j);
                    t.status = if code == 0 { TaskStatus::Done } else { TaskStatus::Failed };
                    t.detail = Some(format!("exit {code}"));
                    core.bus.publish(Event::TaskUpdated(t.clone()));
                    task_notice(&core, &t);
                }
            }
            _ => {}
        }
    }
}

/// The crew's changes as `TaskUpdated` broadcasts and, when a run ends, a notice.
pub(crate) fn crew_changed(core: &Core, info: &CrewRunInfo) {
    let t = run_task_in(&core.home, info);
    core.bus.publish(Event::TaskUpdated(t.clone()));
    task_notice(core, &t);
}

// ---------------------------------------------------------------- the drain

/// Hands a session its news at a step boundary: every pending injection, batched into one
/// message, after a step's tool results and before the loop would finish. It never preempts a
/// model call and never starts anything.
pub(crate) struct DrainMiddleware {
    pub registry: Arc<dyn TaskRegistry>,
    pub session: SessionId,
}

/// The text of a batch of injections, and whether any of it is untrusted.
pub(crate) fn batch_text(items: &[Injection]) -> (String, bool) {
    let mut out = String::new();
    let mut untrusted = false;
    for i in items {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&i.text);
        untrusted |= i.untrusted;
    }
    (out, untrusted)
}

/// Splits taken news into what one message carries (at most [`DRAIN_CHARS`] of text) and what
/// waits for the next step. The first item is always sent: one longer than the budget is cut at a
/// line end and its tail leads the rest, so nothing is ever dropped.
pub(crate) fn fit_budget(items: Vec<Injection>) -> (Vec<Injection>, Vec<Injection>) {
    let mut sent: Vec<Injection> = Vec::new();
    let mut rest: Vec<Injection> = Vec::new();
    let mut used = 0usize;
    let mut it = items.into_iter();
    while let Some(i) = it.next() {
        let sep = if sent.is_empty() { 0 } else { 2 };
        if used + sep + i.text.len() <= DRAIN_CHARS {
            used += sep + i.text.len();
            sent.push(i);
        } else if sent.is_empty() {
            let mut cut = DRAIN_CHARS;
            while !i.text.is_char_boundary(cut) {
                cut -= 1;
            }
            if let Some(nl) = i.text[..cut].rfind('\n').filter(|n| *n > cut / 2) {
                cut = nl + 1;
            }
            let head = format!("{}\n[... continues in the next message]", &i.text[..cut]);
            let tail = format!("[... continued]\n{}", &i.text[cut..]);
            let mut first = i.clone();
            first.text = head;
            let mut next = i;
            next.text = tail;
            sent.push(first);
            rest.push(next);
            break;
        } else {
            rest.push(i);
            break;
        }
    }
    rest.extend(it);
    (sent, rest)
}

/// Takes a session's pending news for one message; what does not fit goes back to the front.
pub(crate) fn drain(reg: &dyn TaskRegistry, session: &SessionId) -> Vec<Injection> {
    let (sent, rest) = fit_budget(reg.take_injections(session, DRAIN_MAX));
    reg.requeue_front(session, rest);
    sent
}

impl DrainMiddleware {
    fn take(&self) -> Option<(String, bool)> {
        let items = drain(self.registry.as_ref(), &self.session);
        if items.is_empty() {
            return None;
        }
        Some(batch_text(&items))
    }
}

impl ToolMiddleware for DrainMiddleware {
    fn name(&self) -> &str {
        "background-news"
    }

    fn after_step(&self, _info: StepInfo, convo: &mut Conversation) -> Vec<Message> {
        match self.take() {
            Some((text, untrusted)) => {
                if untrusted {
                    convo.exposure.tainted = true;
                }
                vec![Message::user(text)]
            }
            None => Vec::new(),
        }
    }

    fn before_final(&self, _info: StepInfo, convo: &mut Conversation, _text: &str) -> FinalFlow {
        match self.take() {
            Some((text, untrusted)) => {
                if untrusted {
                    convo.exposure.tainted = true;
                }
                FinalFlow::Continue(text)
            }
            None => FinalFlow::Accept,
        }
    }
}

// ---------------------------------------------------------------- the waker

/// Wakes idle sessions that have news.
#[derive(Default)]
struct Waker {
    /// Sessions with news not yet handed over, and how many times delivery has failed.
    pending: HashMap<SessionId, u32>,
}

impl Waker {
    fn run(mut self, core: Weak<Core>, rx: Receiver<SessionId>) {
        loop {
            match rx.recv_timeout(WAKE_POLL) {
                Ok(s) => {
                    self.pending.entry(s).or_insert(0);
                    while let Ok(s) = rx.try_recv() {
                        self.pending.entry(s).or_insert(0);
                    }
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
            }
            let Some(core) = core.upgrade() else { return };
            if core.is_shutting_down() {
                return;
            }
            self.deliver(&core);
        }
    }

    fn deliver(&mut self, core: &Arc<Core>) {
        let reg = handle(core);
        let ids: Vec<SessionId> = self.pending.keys().cloned().collect();
        for id in ids {
            if reg.pending_injections(&id) == 0 {
                self.pending.remove(&id);
                continue;
            }
            let Ok(session) = core.session(&id) else {
                let tries = self.pending.entry(id.clone()).or_insert(0);
                *tries += 1;
                if *tries >= WAKE_TRIES {
                    reg.clear_session(&id);
                    self.pending.remove(&id);
                }
                continue;
            };
            // A running turn takes its news at the next step boundary (or before it finishes).
            if session.is_busy() {
                continue;
            }
            let items = drain(reg.as_ref(), &id);
            if items.is_empty() {
                self.pending.remove(&id);
                continue;
            }
            let (body, untrusted) = batch_text(&items);
            let mut sources: Vec<&str> = items.iter().map(|i| i.source.as_str()).collect();
            sources.dedup();
            let text = format!("{}{}]\n{body}", marlowe_agent::background::RELAY_BACKGROUND, sources.join(", "));
            let delivered = session.relay(text.clone(), untrusted).is_ok()
                || core.session(&id).is_ok_and(|h| h.relay(text, untrusted).is_ok());
            if !delivered {
                // Put it back: the session closed under us.
                reg.requeue_front(&id, items);
                *self.pending.entry(id.clone()).or_insert(0) += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use marlowe_agent::InjectionSource;

    fn sid() -> SessionId {
        SessionId("s-drain".into())
    }

    fn big(marker: &str, kb: usize) -> String {
        let mut t = format!("[monitor mon_a · lines 1-50] {marker}\n");
        while t.len() < kb * 1024 {
            t.push_str("a log line with a few words in it, 0123456789\n");
        }
        t
    }

    #[test]
    fn a_flood_larger_than_the_budget_is_all_delivered_over_the_next_drains() {
        let reg: Arc<MemoryTaskRegistry> = Arc::new(MemoryTaskRegistry::new());
        for (i, m) in ["FIRST", "SECOND", "THIRD"].into_iter().enumerate() {
            let mut inj = Injection::new(sid(), InjectionSource::Monitor, big(m, 20));
            inj.at_ms = i as i64;
            reg.inject(inj);
        }
        reg.inject(Injection::new(sid(), InjectionSource::Monitor, "[monitor mon_a ended: `x` exited with code 0 after 1 s]"));
        let sent_total: usize = 3 * 20 * 1024;
        let mw = DrainMiddleware { registry: reg.clone(), session: sid() };
        let mut messages = Vec::new();
        while let Some((text, _)) = mw.take() {
            assert!(text.len() <= DRAIN_CHARS + 200, "a message stays within the budget: {}", text.len());
            messages.push(text);
            assert!(messages.len() < 50, "drains must make progress");
        }
        assert!(messages.len() >= 2, "{}", messages.len());
        assert_eq!(reg.pending_injections(&sid()), 0, "nothing is left behind");
        let all = messages.join("\n");
        for want in ["FIRST", "SECOND", "THIRD", "ended: `x` exited with code 0"] {
            assert!(all.contains(want), "{want} was lost");
        }
        assert!(all.len() >= sent_total, "no text was dropped: {} of {sent_total}", all.len());
        // In order: the end of the monitor comes last.
        assert!(all.rfind("FIRST").unwrap() < all.rfind("SECOND").unwrap());
        assert!(all.rfind("THIRD").unwrap() < all.rfind("ended: `x`").unwrap());
    }

    #[test]
    fn small_news_goes_in_one_message_and_a_failed_delivery_goes_back_to_the_front() {
        let reg: Arc<MemoryTaskRegistry> = Arc::new(MemoryTaskRegistry::new());
        for t in ["one", "two", "three"] {
            reg.inject(Injection::new(sid(), InjectionSource::Notice, t));
        }
        let sent = drain(reg.as_ref(), &sid());
        assert_eq!(sent.iter().map(|i| i.text.as_str()).collect::<Vec<_>>(), ["one", "two", "three"]);
        reg.inject(Injection::new(sid(), InjectionSource::Notice, "four"));
        reg.requeue_front(&sid(), sent);
        let again = drain(reg.as_ref(), &sid());
        assert_eq!(again.iter().map(|i| i.text.as_str()).collect::<Vec<_>>(), ["one", "two", "three", "four"]);
    }

    #[test]
    fn a_step_streamed_in_pieces_reads_as_one_line() {
        let raw = "{\"step\":1,\"action\":\"researcher 1\",\"text\":\"I\"}
{\"step\":1,\"action\":\"researcher 1\",\"text\":\"'ll look\"}
{\"step\":2,\"action\":\"researcher 2\",\"text\":\"Hello\"}
";
        assert_eq!(coalesce_transcript(raw), "[step 1 researcher 1] I'll look
[step 2 researcher 2] Hello
");
        let long = format!("{{\"step\":1,\"action\":\"a\",\"text\":\"{}\"}}
", "x".repeat(2_000));
        let shown = coalesce_transcript(&long);
        assert!(shown.len() < 700 && shown.contains("(2000 chars)") && shown.contains("...xxx"), "{}", shown.len());
    }
}
