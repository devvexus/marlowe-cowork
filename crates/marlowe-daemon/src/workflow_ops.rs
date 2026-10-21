//! Workflows and findings (track W): ListWorkflows, WatchWorkflow, RunWorkflow,
//! SkipWorkflowAgent, ListFindings; the daemon side of the `workflow` and `report_findings` tools.
//!
//! - [`services`]: the session's [`Workflows`] launcher (checks the script, persists an inline one,
//!   queues a crew run with `Run.workflow` set and returns at once; non-negotiable 4) and the
//!   [`FindingsSink`] that broadcasts `Findings`.
//! - The run itself is the crew's (`marlowe_crew::playbook::script`); its progress, `log()` lines
//!   and findings reach the bus through `crew.rs`, and its result reaches the session as a relay
//!   (the same path as any crew run: an idle conductor starts a turn on it).
//! - Hooks (X22): every workflow agent fires SubagentStart/SubagentStop with `agent_type:
//!   "workflow"` through the session's hook runner, also for a run started from the TUI or resumed
//!   after a restart ([`install_hooks`]).
//! - Tasks (BG1-BG3): workflow runs are listed as `wf_<run>` tasks with their journal as the
//!   output file, and `task_stop` stops them ([`on_boot`] adds the source).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::Duration;

use marlowe_agent::commands::plugin::bundle_roots;
use marlowe_agent::hooks::HookRunner;
use marlowe_agent::{Conversation, Services, TaskChunk, TaskInfo, TaskKind, TaskRegistry, TaskSource, TaskStatus};
use marlowe_crew::playbook::script::{self, RunObserver};
use marlowe_crew::{CrewRunInfo, CrewRunStatus, DispatchArgs, RunId};
use marlowe_model::CancelToken;
use marlowe_proto::{Event, Request, SessionId, WorkflowInfo, WorkflowRef};
use marlowe_workflow::journal::{self, Line};
use marlowe_workflow::service::{self, FindingsSink, LaunchRequest, Launched, Waited, WorkflowLauncher};
use marlowe_workflow::{AgentCall, AgentResult, Control, Workflows};

use crate::daemon::Core;

const NO_CREW: &str = "team tasks run on the crew, and the crew is not running";
/// Runs scanned for workflow runs (lists, tasks).
const SCAN: u32 = 500;

pub(crate) fn dispatch(core: &Arc<Core>, client: u64, req: Request) {
    let reply = |event: Event| core.bus.reply(client, event);
    let fail = |message: String| reply(Event::Error { session: None, message });
    match req {
        Request::ListWorkflows => {
            let roots = service::roots(&core.home, None, bundle_roots(Some(&core.home), None));
            let saved = service::saved_list(&roots);
            reply(Event::WorkflowList { saved, runs: runs(core) })
        }
        Request::WatchWorkflow { run } => match view(core, &run) {
            Some(info) => reply(Event::WorkflowUpdated(info)),
            None => fail(format!("no team task run {}", run.short())),
        },
        Request::RunWorkflow { session, name, script_path, args, resume_from } => {
            let workspace = session.as_ref().and_then(|s| core.store.get(s).ok().flatten()).and_then(|s| s.workspace).map(PathBuf::from);
            let launcher = Launcher::new(core, session.clone(), None);
            // The user asked for this run by name: what it resolves to now is what they approved.
            let roots = service::roots(&core.home, workspace.as_deref(), bundle_roots(Some(&core.home), workspace.as_deref()));
            let approved_digest = service::prepare(&roots, None, script_path.as_deref(), name.as_deref()).ok().map(|p| p.digest);
            let req = LaunchRequest {
                session,
                workspace,
                script: None,
                script_path,
                name,
                args,
                background: true,
                resume_from: resume_from.map(|r| r.0),
                tainted: false,
                approved_digest,
            };
            match launcher.launch(req) {
                Ok(l) => match core.crew.get().and_then(|c| c.supervisor.get(&RunId(l.run_id.clone())).ok().flatten()) {
                    Some(info) => reply(Event::RunUpdated(info)),
                    None => fail(format!("team task run {} vanished after it was queued", l.run_id)),
                },
                Err(e) => fail(e),
            }
        }
        Request::SkipWorkflowAgent { run, agent } => match script::control(&run) {
            Some(tx) => {
                let _ = tx.send(Control::Skip(agent));
                if let Some(info) = view(core, &run) {
                    reply(Event::WorkflowUpdated(info));
                }
            }
            None => fail(format!("team task run {} is not running", run.short())),
        },
        Request::ListFindings { run, session } => {
            let findings = match (&run, &session) {
                (Some(r), _) => marlowe_workflow::findings::load(&core.home, &format!("run-{}", r.as_str())),
                (None, Some(s)) => marlowe_workflow::findings::load(&core.home, s.as_str()),
                (None, None) => Vec::new(),
            };
            reply(Event::Findings { session, run, findings })
        }
        other => fail(format!("not a team task request: {other:?}")),
    }
}

/// Every workflow run: live progress where this process has it, else what the store and the
/// journal say.
fn runs(core: &Core) -> Vec<WorkflowInfo> {
    let Some(link) = core.crew.get() else { return Vec::new() };
    link.supervisor
        .list(SCAN)
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r.workflow.is_some())
        .filter_map(|r| view_of(core, &r))
        .collect()
}

fn view(core: &Core, run: &RunId) -> Option<WorkflowInfo> {
    if let Some(p) = script::progress(run) {
        return Some(p);
    }
    let info = core.crew.get()?.supervisor.get(run).ok().flatten()?;
    view_of(core, &info)
}

/// A workflow run's view from its journal (a run this process did not watch).
fn view_of(core: &Core, info: &CrewRunInfo) -> Option<WorkflowInfo> {
    let wf = info.workflow.as_ref()?;
    if let Some(p) = script::progress(&info.id) {
        return Some(p);
    }
    let name = wf.name.clone().unwrap_or_else(|| "workflow".into());
    let mut progress = marlowe_workflow::Progress::new(info.id.clone(), name, Vec::new(), None);
    for rec in journal::agents(&journal::load(&script::journal_path(&core.home, &info.id)).unwrap_or_default()) {
        let call = AgentCall {
            index: rec.seq,
            id: rec.id.clone(),
            key: rec.key.clone(),
            prompt: rec.label.clone(),
            opts: marlowe_workflow::AgentOpts { label: Some(rec.label.clone()), phase: rec.phase.clone(), ..Default::default() },
        };
        progress.apply(&marlowe_workflow::WfEvent::Queued { call: call.clone() });
        progress.apply(&marlowe_workflow::WfEvent::Finished { call, result: rec.result, cached: rec.cached, completion: rec.completion });
    }
    Some(progress.snapshot())
}

/// W's services for one session: the launcher and the findings broadcast.
pub(crate) fn services(core: &Arc<Core>, convo: &Conversation, services: Services) -> Services {
    let hooks = services.get::<Arc<HookRunner>>().cloned();
    let launcher = Launcher::new(core, Some(convo.session.clone()), hooks);
    let weak = Arc::downgrade(core);
    let sink = FindingsSink(Arc::new(move |session: &SessionId, findings: &[marlowe_proto::Finding]| {
        if let Some(core) = weak.upgrade() {
            core.bus.publish(Event::Findings { session: Some(session.clone()), run: None, findings: findings.to_vec() });
        }
    }));
    services.with(Workflows(Arc::new(launcher))).with(sink)
}

/// Lists workflow runs as `wf_<run>` tasks (BG1-BG3). Must not block.
pub(crate) fn on_boot(core: &Arc<Core>) {
    crate::tasks::registry(core).add_source(Arc::new(WorkflowTasks { core: Arc::downgrade(core) }));
    install_skill(&core.home);
}

/// The `team-task-authoring` skill (WF9: the long reference and examples), shipped as a user-level
/// skill on first start; a copy the user edited is left alone.
pub(crate) const AUTHORING_SKILL: &str = include_str!("../../../workflows/workflow-authoring.md");

pub(crate) fn install_skill(home: &Path) {
    let path = home.join("skills").join("team-task-authoring").join("SKILL.md");
    if path.exists() {
        return;
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&path, AUTHORING_SKILL);
}

/// Gives workflow runs under this home their session's hooks when none were set for them (a run
/// started by RunWorkflow, or resumed in place after a restart). Called before the crew starts.
pub(crate) fn install_hooks(core: &Arc<Core>) {
    let weak = Arc::downgrade(core);
    script::set_observer_factory(
        &core.home,
        Arc::new(move |_run: &RunId, session: Option<&SessionId>, workspace: Option<&Path>| {
            let core = weak.upgrade()?;
            let hooks = crate::mode_ops::session_hooks(&core, session?, workspace);
            Some(Arc::new(HookObserver { hooks }) as Arc<dyn RunObserver>)
        }),
    );
}

// ---------------------------------------------------------------- the launcher

struct Launcher {
    core: Weak<Core>,
    session: Option<SessionId>,
    hooks: Option<Arc<HookRunner>>,
}

impl Launcher {
    fn new(core: &Arc<Core>, session: Option<SessionId>, hooks: Option<Arc<HookRunner>>) -> Launcher {
        Launcher { core: Arc::downgrade(core), session, hooks }
    }
}

/// Writes the script the run will read: inline and built-in sources go under the run's directory.
fn persist(home: &Path, run: &RunId, prepared: &service::Prepared) -> Result<PathBuf, String> {
    if let Some(p) = &prepared.path {
        return Ok(p.clone());
    }
    let dir = script::run_dir(home, run);
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let path = dir.join("script.star");
    std::fs::write(&path, &prepared.source).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(path)
}

impl WorkflowLauncher for Launcher {
    fn launch(&self, req: LaunchRequest) -> Result<Launched, String> {
        let core = self.core.upgrade().ok_or("the daemon is stopping")?;
        let link = core.crew.get().ok_or(NO_CREW)?;
        let roots = service::roots(&core.home, req.workspace.as_deref(), bundle_roots(Some(&core.home), req.workspace.as_deref()));
        let prepared = service::prepare(&roots, req.script.as_deref(), req.script_path.as_deref(), req.name.as_deref())?;
        // Bound to what the approval showed (WF1): a script that changed since runs nothing.
        service::may_run(&core.home, &prepared, req.approved_digest.as_deref())?;
        let args = service::remembered_args(&core.home, req.workspace.as_deref(), &prepared.name, req.args.clone());
        let resumed_from = match req.resume_from.as_deref().map(str::trim).filter(|r| !r.is_empty()) {
            None => None,
            Some(r) => {
                let found = link.supervisor.list(SCAN).unwrap_or_default().into_iter().find(|i| i.id.as_str() == r || i.id.as_str().starts_with(r));
                let info = found.ok_or_else(|| format!("no run {r} to resume from"))?;
                if !script::journal_path(&core.home, &info.id).is_file() {
                    return Err(format!("run {} has no team task journal to resume from", info.id.short()));
                }
                Some(info.id)
            }
        };
        let session = req.session.clone().or_else(|| self.session.clone());
        let goal = format!("team task {}: {}", prepared.name, prepared.meta.description.trim());
        let dispatch = DispatchArgs {
            role: script::WORKFLOW_ROLE.into(),
            goal,
            playbook: None,
            workspace: req.workspace.as_ref().map(|w| w.display().to_string()),
            budget: None,
            tainted: req.tainted,
        };
        let mut script_path = String::new();
        let home = core.home.clone();
        let receipt = link
            .supervisor
            .dispatch_workflow(session, dispatch, |run| {
                let path = persist(&home, run, &prepared)?;
                script_path = path.display().to_string();
                if let Some(h) = &self.hooks {
                    script::set_observer(run, Arc::new(HookObserver { hooks: h.clone() }));
                }
                if !req.background {
                    // Before the run starts, so its end can never relay ahead of the waiter.
                    crate::crew::wait_starts(&core, run);
                }
                Ok(WorkflowRef {
                    name: Some(prepared.name.clone()),
                    script_path: script_path.clone(),
                    digest: prepared.digest.clone(),
                    args: args.clone(),
                    foreground: !req.background,
                    resumed_from: resumed_from.clone(),
                })
            })
            .map_err(|e| e.to_string())?;
        let _ = service::trust_approved(&core.home, &prepared, req.approved_digest.as_deref());
        Ok(Launched { run_id: receipt.run_id.0, script_path, name: prepared.name, meta: prepared.meta })
    }

    fn wait(&self, run: &str, cancel: &CancelToken) -> Waited {
        let Some(core) = self.core.upgrade() else { return Waited::Failed("the daemon is stopping".into()) };
        let id = RunId(run.to_string());
        let (waited, delivered) = wait_for(&core, &id, cancel);
        // Exactly one delivery: the tool result, or (detached) the relay notice.
        crate::crew::wait_ends(&core, &id, delivered);
        waited
    }
}

/// Polls the run until it ends or `cancel` fires; true when the run's end is what it returns.
fn wait_for(core: &Arc<Core>, id: &RunId, cancel: &CancelToken) -> (Waited, bool) {
    let Some(link) = core.crew.get() else { return (Waited::Failed(NO_CREW.into()), false) };
    loop {
        if cancel.is_cancelled() {
            return (Waited::Detached, false);
        }
        match link.supervisor.get(id) {
            Ok(Some(info)) => {
                let finished = info.status.is_terminal() || (info.status == CrewRunStatus::NeedsYou && info.result.is_some());
                if finished {
                    return (outcome(&core.home, &info), true);
                }
            }
            Ok(None) => return (Waited::Failed(format!("run {} is gone", id.short())), false),
            Err(e) => return (Waited::Failed(e.to_string()), false),
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

/// How a finished workflow run ended, from its journal (the whole value) and the run record.
fn outcome(home: &Path, info: &CrewRunInfo) -> Waited {
    let lines = journal::load(&script::journal_path(home, &info.id)).unwrap_or_default();
    match lines.iter().rev().find(|l| matches!(l, Line::Result { .. } | Line::Ended { .. })) {
        Some(Line::Result { value }) => {
            let mut value = value.clone();
            if info.status == CrewRunStatus::NeedsYou {
                if let Some(note) = &info.note {
                    if let Some(o) = value.as_object_mut() {
                        o.insert("needs_you".into(), serde_json::Value::String(note.clone()));
                    }
                }
            }
            Waited::Done { value, tainted: info.tainted }
        }
        Some(Line::Ended { message }) => Waited::Failed(message.clone()),
        _ => Waited::Failed(info.note.clone().unwrap_or_else(|| format!("the run ended {}", info.status.as_str()))),
    }
}

/// SubagentStart/SubagentStop for a run's agents (X22), on the agent's own thread.
struct HookObserver {
    hooks: Arc<HookRunner>,
}

impl RunObserver for HookObserver {
    fn agent_start(&self, run: &RunId, call: &AgentCall) -> Option<String> {
        let out = self.hooks.subagent_start(&agent_id(run, call), "workflow");
        let ctx = out.additional_context.join("\n");
        (!ctx.trim().is_empty()).then_some(ctx)
    }

    fn agent_stop(&self, run: &RunId, call: &AgentCall, result: &AgentResult) {
        let last = match &result.value {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Null => result.note.clone().unwrap_or_default(),
            v => v.to_string(),
        };
        let _ = self.hooks.subagent_stop(&agent_id(run, call), "workflow", &last);
    }
}

fn agent_id(run: &RunId, call: &AgentCall) -> String {
    format!("wf_{}:{}", run.short(), call.index)
}

// ---------------------------------------------------------------- tasks

struct WorkflowTasks {
    core: Weak<Core>,
}

fn task_status(s: CrewRunStatus) -> TaskStatus {
    match s {
        CrewRunStatus::Queued | CrewRunStatus::Running => TaskStatus::Running,
        CrewRunStatus::NeedsYou | CrewRunStatus::Paused => TaskStatus::Waiting,
        CrewRunStatus::Done => TaskStatus::Done,
        CrewRunStatus::Failed => TaskStatus::Failed,
        CrewRunStatus::Cancelled => TaskStatus::Stopped,
    }
}

impl TaskSource for WorkflowTasks {
    fn list(&self) -> Vec<TaskInfo> {
        let Some(core) = self.core.upgrade() else { return Vec::new() };
        let Some(link) = core.crew.get() else { return Vec::new() };
        link.supervisor
            .list(SCAN)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|r| {
                let wf = r.workflow.as_ref()?;
                let progress = script::progress(&r.id);
                let done = progress.as_ref().map(|p| p.agents.iter().filter(|a| a.status != marlowe_proto::WorkflowAgentStatus::Queued && a.status != marlowe_proto::WorkflowAgentStatus::Running).count());
                let total = progress.as_ref().map(|p| p.agents.len());
                let phase = progress.as_ref().and_then(|p| p.phase.clone());
                let mut title = format!("team task {}", wf.name.clone().unwrap_or_else(|| "inline".into()));
                if let Some(ph) = phase {
                    title.push_str(&format!(" · {ph}"));
                }
                Some(TaskInfo {
                    id: TaskKind::Workflow.id(r.id.as_str()),
                    kind: TaskKind::Workflow,
                    session: r.session.clone(),
                    title,
                    status: task_status(r.status),
                    started_ms: r.created_ms,
                    ended_ms: r.status.is_terminal().then_some(r.updated_ms),
                    output_file: Some(script::journal_path(&core.home, &r.id).display().to_string()),
                    detail: match (done, total) {
                        (Some(d), Some(t)) => Some(format!("{d} of {t} agents")),
                        _ => r.note.clone(),
                    },
                })
            })
            .collect()
    }

    /// Stops the run's engine (no new agent starts, in-flight agents are cancelled) and the run;
    /// the journal keeps every finished agent, so `resume_from` picks it up.
    fn stop(&self, id: &str) -> Option<Result<(), String>> {
        let (kind, raw) = TaskKind::split(id)?;
        if kind != TaskKind::Workflow {
            return None;
        }
        let core = self.core.upgrade()?;
        let link = core.crew.get()?;
        let run = RunId(raw.to_string());
        let info = link.supervisor.get(&run).ok().flatten()?;
        info.workflow.as_ref()?;
        if let Some(tx) = script::control(&run) {
            let _ = tx.send(Control::Stop);
        }
        Some(link.supervisor.cancel(&run).map_err(|e| e.to_string()))
    }

    fn output(&self, _id: &str, _since: Option<u64>, _filter: Option<&str>) -> Option<Result<TaskChunk, String>> {
        None
    }
}
