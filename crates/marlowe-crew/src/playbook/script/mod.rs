//! Workflow runs (design 3.11, track W): a crew run whose `Run.workflow` names a Starlark script
//! runs it with `marlowe_workflow`, every `agent()` a crew step ([`host::CrewAgentHost`]).
//!
//! - Journal: `<home>\workflows\runs\<run>\journal.jsonl`, one line per finished agent, then the
//!   result. A run resumed in place (daemon restart) replays from its own journal; a run started
//!   with `resume_from` replays from that run's journal under the longest-unchanged-prefix rule.
//! - Progress: [`progress`] holds each live run's view; every change is also sent as a
//!   [`ScriptEvent`] for the daemon to broadcast.
//! - Control: [`control`] reaches a live run's engine (skip an agent, stop the run); the run's
//!   own cancel token stops it too.
//! - Hooks: the daemon registers a [`RunObserver`] per run ([`set_observer`]); it fires
//!   SubagentStart/SubagentStop (X22) around every agent on that agent's thread.

pub mod host;
pub mod structured;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use crossbeam_channel::Sender;
use marlowe_proto::{Confidence, Dispatch, Finding, RunId, SandboxKind, SessionId, WorkflowInfo, WorkflowRef};
use marlowe_workflow::journal::{self, AgentRecord, Line, Resume};
use marlowe_workflow::{AgentCall, AgentResult, Control, Observer, Progress, RunConfig, WfEvent, WorkflowError};

pub use host::{CrewAgentHost, WORKFLOW_ROLE};
pub use structured::{WithStructuredOutput, STRUCTURED_OUTPUT};

use crate::playbook::AgentRunner;
use crate::CrewError;

/// Hooks around a run's agents, set by the daemon.
pub trait RunObserver: Send + Sync {
    /// Before an agent starts (SubagentStart); extra context for its prompt.
    fn agent_start(&self, _run: &RunId, _call: &AgentCall) -> Option<String> {
        None
    }
    /// After it ended (SubagentStop).
    fn agent_stop(&self, _run: &RunId, _call: &AgentCall, _result: &AgentResult) {}
}

/// Builds the hooks of a run nobody registered an observer for (one started without a session's
/// hooks, or resumed in place after a restart): `(run, session, workspace)`.
pub type ObserverFactory = dyn Fn(&RunId, Option<&SessionId>, Option<&Path>) -> Option<Arc<dyn RunObserver>> + Send + Sync;

#[derive(Default)]
struct Registry {
    controls: HashMap<RunId, Sender<Control>>,
    progress: HashMap<RunId, WorkflowInfo>,
    observers: HashMap<RunId, Arc<dyn RunObserver>>,
    /// By profile home, so daemons in one process (tests) never use each other's.
    factories: HashMap<PathBuf, Arc<ObserverFactory>>,
}

fn registry() -> std::sync::MutexGuard<'static, Registry> {
    static R: OnceLock<Mutex<Registry>> = OnceLock::new();
    R.get_or_init(Mutex::default).lock().unwrap_or_else(|e| e.into_inner())
}

/// Sets the hooks for `run` (before it starts, or while it runs).
pub fn set_observer(run: &RunId, observer: Arc<dyn RunObserver>) {
    registry().observers.insert(run.clone(), observer);
}

/// Sets how runs under `home` get their hooks when none were set for them ([`ObserverFactory`]).
pub fn set_observer_factory(home: &Path, factory: Arc<ObserverFactory>) {
    registry().factories.insert(home.to_path_buf(), factory);
}

/// The hooks for `job`'s run: the observer set for it, else the home's factory.
fn observer_for(job: &ScriptJob<'_>) -> Option<Arc<dyn RunObserver>> {
    let factory = {
        let reg = registry();
        if let Some(o) = reg.observers.get(job.run) {
            return Some(o.clone());
        }
        reg.factories.get(job.home).cloned()?
    };
    let observer = factory(job.run, job.session, job.workspace)?;
    registry().observers.insert(job.run.clone(), observer.clone());
    Some(observer)
}

/// The control channel of a live workflow run.
pub fn control(run: &RunId) -> Option<Sender<Control>> {
    registry().controls.get(run).cloned()
}

/// A workflow run's progress view (live, or the last one this process saw).
pub fn progress(run: &RunId) -> Option<WorkflowInfo> {
    registry().progress.get(run).cloned()
}

/// Every workflow run this process has seen.
pub fn progress_all() -> Vec<WorkflowInfo> {
    registry().progress.values().cloned().collect()
}

/// `<home>\workflows\runs\<run>`.
pub fn run_dir(home: &Path, run: &RunId) -> PathBuf {
    home.join("workflows").join("runs").join(run.as_str())
}

/// The run's journal (BG3's output file for a workflow task).
pub fn journal_path(home: &Path, run: &RunId) -> PathBuf {
    run_dir(home, run).join("journal.jsonl")
}

/// What a workflow run reports while it runs.
#[derive(Debug, Clone, PartialEq)]
pub enum ScriptEvent {
    Progress(WorkflowInfo),
    Log(String),
    /// The script returned findings in the 3.7 schema (`{"findings": [...]}`).
    Findings(Vec<Finding>),
}

/// One workflow run to execute.
pub struct ScriptJob<'a> {
    pub home: &'a Path,
    pub run: &'a RunId,
    pub session: Option<&'a SessionId>,
    pub workspace: Option<&'a Path>,
    pub workflow: &'a WorkflowRef,
    /// Plugin bundle roots for `workflow(name)` inside the script.
    pub plugins: Vec<PathBuf>,
}

/// Progress views of ended runs kept in memory (the rest are rebuilt from their journals).
const KEPT_VIEWS: usize = 64;

/// Longest return value the relay carries (the journal keeps it whole).
const SUMMARY_CHARS: usize = 12_000;

struct JournalObserver<'a> {
    path: PathBuf,
    progress: Progress,
    run: RunId,
    emit: &'a dyn Fn(ScriptEvent),
}

impl Observer for JournalObserver<'_> {
    fn event(&mut self, ev: &WfEvent) {
        if let WfEvent::Finished { call, result, cached, completion } = ev {
            let rec = AgentRecord {
                seq: call.index,
                id: call.id.clone(),
                key: call.key.clone(),
                completion: *completion,
                label: call.display_label(),
                phase: call.opts.phase.clone(),
                cached: *cached,
                result: result.clone(),
            };
            let _ = journal::append(&self.path, &Line::Agent(rec));
        }
        if let WfEvent::Log(l) = ev {
            (self.emit)(ScriptEvent::Log(l.clone()));
        }
        self.progress.apply(ev);
        let snap = self.progress.snapshot();
        registry().progress.insert(self.run.clone(), snap.clone());
        (self.emit)(ScriptEvent::Progress(snap));
    }
}

/// Runs the workflow `job` names on `runner`, to its end. Blocks the run thread (never the
/// conductor's).
pub fn run_workflow(runner: &AgentRunner, job: ScriptJob<'_>, emit: &dyn Fn(ScriptEvent)) -> Result<Dispatch, CrewError> {
    let wf = job.workflow;
    let source = std::fs::read_to_string(&wf.script_path)
        .map_err(|e| CrewError::Step { step: "workflow".into(), message: format!("cannot read the script {}: {e}", wf.script_path) })?;
    if marlowe_workflow::digest(&source) != wf.digest {
        return Err(CrewError::Step {
            step: "workflow".into(),
            message: format!("{} changed since it was approved; start it again to review the new version", wf.script_path),
        });
    }
    let journal = journal_path(job.home, job.run);
    // In place: this run's own journal; else the run it resumes from, under the prefix rule.
    let resume = if journal.is_file() {
        Resume::from_journal(&journal, false).ok()
    } else {
        wf.resumed_from.as_ref().and_then(|from| Resume::from_journal(&journal_path(job.home, from), true).ok())
    };
    let filename = Path::new(&wf.script_path).file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_else(|| "workflow.star".into());
    let name = wf.name.clone().unwrap_or_else(|| filename.trim_end_matches(".star").to_string());

    let mut cfg = RunConfig::new(filename, wf.args.clone());
    cfg.resume = resume;
    cfg.budget = job.session.and_then(marlowe_workflow::pools::get);
    cfg.resolver = Some(Arc::new(marlowe_workflow::service::roots(job.home, job.workspace, job.plugins.clone())));
    let local = runner.model_for(WORKFLOW_ROLE).ok().filter(|m| m.model.local || m.provider.kind().is_local()).map(|m| m.model.model_ref());
    cfg.max_concurrency = marlowe_workflow::default_concurrency(match &local {
        Some(model) => (runner.local_slots)(model).max(1),
        None => marlowe_workflow::MAX_CONCURRENCY,
    });

    let meta = marlowe_workflow::validate::static_checks(&cfg.filename, &source).map(|v| v.meta).unwrap_or_default();
    let budget_total = cfg.budget.as_ref().and_then(|b| b.total());
    let (ctl_tx, ctl_rx) = crossbeam_channel::unbounded();
    registry().controls.insert(job.run.clone(), ctl_tx.clone());
    // The run's cancel (the user's stop, a pause, shutdown) stops the engine: no new agent starts.
    let _stop_on_cancel = runner.cancel.on_cancel(move || {
        let _ = ctl_tx.send(Control::Stop);
    });
    let observer = observer_for(&job);
    let host = CrewAgentHost::new(runner.clone(), job.run.clone(), job.home.to_path_buf(), observer);
    let mut obs = JournalObserver { path: journal.clone(), progress: Progress::new(job.run.clone(), name.clone(), meta.phases.clone(), budget_total), run: job.run.clone(), emit };
    crate::playbook::StepRunner::progress(runner, &format!("workflow {name}"));
    let outcome = marlowe_workflow::run(&source, cfg, host, &mut obs, &ctl_rx);
    {
        // The run is over in this process: its live entries go. The last progress view stays for
        // the most recent runs; older ones are read back from their journals.
        let mut reg = registry();
        reg.controls.remove(job.run);
        reg.observers.remove(job.run);
        if reg.progress.len() > KEPT_VIEWS {
            let reg = &mut *reg;
            let ended: Vec<RunId> = reg.progress.keys().filter(|r| *r != job.run && !reg.controls.contains_key(*r)).cloned().collect();
            for r in ended.into_iter().take(reg.progress.len() - KEPT_VIEWS) {
                reg.progress.remove(&r);
            }
        }
    }

    match outcome {
        Ok(out) => {
            let _ = journal::append(&journal, &Line::Result { value: out.value.clone() });
            if let Some(list) = out.value.get("findings").filter(|f| f.is_array()) {
                if let Ok(found) = marlowe_workflow::findings::parse_findings(list) {
                    if !found.is_empty() {
                        let _ = marlowe_workflow::findings::append(job.home, &format!("run-{}", job.run.as_str()), &found);
                        emit(ScriptEvent::Findings(found));
                    }
                }
            }
            let text = serde_json::to_string_pretty(&out.value).unwrap_or_default();
            let shown = clip(&text, SUMMARY_CHARS);
            let mut summary = format!(
                "Workflow `{name}` finished: {} agent(s), {} from cache, {} output tokens.\nReturn value:\n```json\n{shown}\n```",
                out.agents, out.cached, out.tokens
            );
            if text.len() > shown.len() {
                summary.push_str(&format!("\n(The whole value is in the journal: {}.)", journal.display()));
            }
            let sandbox = crate::playbook::StepRunner::sandbox(runner);
            let diff = if sandbox.kind == SandboxKind::Worktree { sandbox.diff_summary().ok().filter(|d| !d.files.is_empty()) } else { None };
            Ok(Dispatch { summary, artifacts: vec![journal.display().to_string()], confidence: Confidence::Medium, citations: vec![], diff })
        }
        Err(e) => {
            let _ = journal::append(&journal, &Line::Ended { message: e.to_string() });
            Err(match e {
                WorkflowError::Stopped => CrewError::Cancelled,
                other => CrewError::Step { step: "workflow".into(), message: other.to_string() },
            })
        }
    }
}

fn clip(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n… (cut)", &s[..end])
}

#[cfg(test)]
mod tests;
