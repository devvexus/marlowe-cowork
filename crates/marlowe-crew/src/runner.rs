//! One run's thread: sandbox, the agent-backed step runner, and the hooks that make the run
//! durable and controllable (checkpoints, spend, budget approvals, pause/stop, live events).

use std::collections::{HashMap, HashSet};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use marlowe_agent::{AgentEvent, ApprovalGate, ApprovalReq, Budget, ToolHost, MAX_STEPS};
use marlowe_mind::{Kind, MemoryIn, ProfileQuery, Provenance, RecallQuery};
use marlowe_model::{CancelToken, Message, ModelRef, Pricing, Usage};
use marlowe_proto::{
    ApprovalDecision, CostQuote, CrewRunStatus, CrewSpent, Dispatch, MemoryScope, MemoryTrust, PlanDecision, Playbook, ResearchPlan,
    PhaseEvent, PhaseKind, ResearchProgress, RunId, SandboxKind, SessionId,
};

use crate::playbook::build_v2::{BuildPlan, BuildQuote};
use crate::playbook::research_v2::{Finding, Recalled};
use crate::playbook::{
    self, sandbox_shell, AgentRunner, CallCheckpoint, PartialStep, PlaybookConfig, ResearchIo, ShellResult, StepFailure,
    StepHooks, StepModel, StepResult, StepSpec,
};
use crate::role::RoleCard;
use crate::run::{call_line, clip, Run, StepRecord};
use crate::sandbox::{Sandbox, ShellVerdict};
use crate::shell_policy::{Dialect, Verdict};
use crate::tools::CrewTools;
use crate::supervisor::{extend_budget, result_memory, Control, CrewEvent, Inner, PendingPlan};
use crate::CrewError;

/// The current action of a step held while the GPU is busy.
const GPU_WAIT: &str = "waiting for the GPU";

/// The session id a run's own conversations and approval requests carry.
fn crew_session(run: &RunId) -> SessionId {
    SessionId::from(format!("crew:{run}").as_str())
}

/// Body of a run thread: executes the run behind `catch_unwind`, then records how it ended and
/// frees its provider slot.
pub(crate) fn run_thread(inner: Arc<Inner>, id: RunId, provider: String) {
    let outcome = catch_unwind(AssertUnwindSafe(|| execute(&inner, &id)));
    let outcome = outcome.map_err(|p| {
        p.downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| p.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown panic".into())
    });
    finish(&inner, &id, &provider, outcome);
    inner.wake();
}

/// Creates (or finds) the sandbox and drives the playbook.
fn execute(inner: &Arc<Inner>, id: &RunId) -> Result<Dispatch, CrewError> {
    let (run, ctl, inbox) = {
        let st = inner.state();
        let live = st.live.get(id).ok_or_else(|| CrewError::UnknownRun(id.clone()))?;
        (live.run.clone(), live.ctl.clone(), live.steer_rx.clone())
    };
    let roles = inner.roles.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let card = roles.get(&run.role).ok_or_else(|| CrewError::UnknownRole(run.role.clone()))?;

    // The run spinning up, step by step as each really ends (a client shows them as they land).
    let spin = |ev: PhaseEvent| inner.emit(CrewEvent::Phase(PhaseEvent { run: Some(id.clone()), ..ev }));
    spin(PhaseEvent::start(PhaseKind::Spinup).detail(run.role.as_str()));
    let began = Instant::now();
    let want = match run.playbook {
        Some(Playbook::Build) => SandboxKind::Worktree,
        Some(Playbook::DeepResearch) => SandboxKind::Scratch,
        None => card.sandbox,
    };
    // A workflow makes its worktree when its first agent that may write starts; until then (and
    // for good, when its agents only read) it has a scratch directory and reads the workspace.
    let deferred = run.workflow.is_some() && want == SandboxKind::Worktree;
    let sandbox = match run.sandbox.clone() {
        Some(sb) if sb.root.exists() => sb,
        _ => {
            inner.update(id, |r| r.current_action = Some("preparing the sandbox".into()));
            let now = if deferred { SandboxKind::Scratch } else { want };
            // A workflow works on the checkout as it is, uncommitted changes included.
            Sandbox::create_with(&inner.home, id, run.workspace.as_deref(), now, run.workflow.is_some())?
        }
    };
    let run_worktree = (deferred && sandbox.kind != SandboxKind::Worktree).then(|| {
        let (inner, id, workspace) = (inner.clone(), id.clone(), run.workspace.clone());
        playbook::script::RunWorktree::new(move || {
            let sb = Sandbox::create_with(&inner.home, &id, workspace.as_deref(), SandboxKind::Worktree, true)?;
            inner.update(&id, |r| r.sandbox = Some(sb.clone()));
            Ok(sb)
        })
    });
    inner.update(id, |r| r.sandbox = Some(sandbox.clone()));
    spin(PhaseEvent::end(PhaseKind::Spinup, began.elapsed().as_millis() as u64).stage("sandbox"));

    let done: HashMap<String, StepResult> = inner
        .store()
        .steps(id)?
        .into_iter()
        .filter_map(|s| {
            let (tainted, truncated) = (s.tainted, s.truncated);
            s.output.clone().map(|text| (s.key, StepResult { text, usage: s.usage, tainted, truncated }))
        })
        .collect();
    let rows = inner.store().steps(id)?.iter().map(|s| s.step).max().unwrap_or(0);
    let hooks = Arc::new(Hooks {
        inner: inner.clone(),
        id: id.clone(),
        session: run.session.clone(),
        ctl: ctl.clone(),
        done: Mutex::new(done),
        rows: Mutex::new(rows),
        live_rows: Mutex::new(HashMap::new()),
        mid_line: Mutex::new(HashSet::new()),
        budget_gate: Mutex::new(()),
        in_flight: Mutex::new(0),
        pricing: Mutex::new(HashMap::new()),
        model_spun: std::sync::atomic::AtomicBool::new(false),
    });

    let resolver = hooks.clone();
    let models: playbook::ModelResolver = Arc::new(move |card| resolver.resolve(card));
    let tools = (inner.deps.tools)(&sandbox, run.workspace.as_deref());
    let begun = Instant::now();
    let n_tools = CrewTools::new(tools.clone(), card.tools.clone(), sandbox.clone()).specs().len() as u64;
    spin(PhaseEvent::end(PhaseKind::Spinup, begun.elapsed().as_millis() as u64).stage("tools").with_done(n_tools));
    spin(PhaseEvent::end(PhaseKind::Spinup, 0).stage("budget").detail(budget_echo(&run.budget)));
    let mut runner = AgentRunner::new(id.clone(), roles.clone(), models, tools, sandbox, run.workspace.clone());
    runner.tool_factory = Some(inner.deps.tools.clone());
    runner.approvals = Arc::new(RunGate { inner: inner.clone(), id: id.clone(), session: run.session.clone() });
    runner.cancel = ctl.cancel.clone();
    runner.inbox = inbox;
    runner.hooks = hooks.clone();
    runner.shell = guarded_shell(inner.clone(), id.clone(), run.session.clone());
    runner.per_provider = inner.cfg.per_provider;
    runner.local_slots = {
        let brains = inner.deps.brains.clone();
        Arc::new(move |model| brains.context().crew_slots(model))
    };
    runner.named_model = {
        let inner = inner.clone();
        Some(Arc::new(move |card: &RoleCard, name: &str| inner.named(card, name)))
    };
    runner.slots_note = {
        let brains = inner.deps.brains.clone();
        Arc::new(move |model| brains.context().slots_note(model))
    };

    let cfg = PlaybookConfig {
        max_scouts: inner.cfg.max_scouts,
        max_test_iterations: inner.cfg.max_test_iterations,
        test_command: None,
        research: inner.cfg.research.clone(),
        build: inner.cfg.build.clone(),
    };
    hooks.check()?;
    // A workflow script (track W): its agents are this run's steps.
    if let Some(wf) = run.workflow.clone() {
        let (sink, run_id, session) = (inner.clone(), id.clone(), run.session.clone());
        let emit = move |ev: playbook::script::ScriptEvent| match ev {
            playbook::script::ScriptEvent::Progress(info) => sink.emit(CrewEvent::Workflow(info)),
            playbook::script::ScriptEvent::Log(line) => sink.emit(CrewEvent::WorkflowLog { run: run_id.clone(), line }),
            playbook::script::ScriptEvent::Findings(findings) => {
                sink.emit(CrewEvent::Findings { run: run_id.clone(), session: session.clone(), findings })
            }
        };
        let job = playbook::script::ScriptJob {
            home: &inner.home,
            run: id,
            session: run.session.as_ref(),
            workspace: run.workspace.as_deref(),
            workflow: &wf,
            plugins: marlowe_agent::commands::plugin::bundle_roots(Some(&inner.home), run.workspace.as_deref()),
            worktree: run_worktree,
        };
        return playbook::script::run_workflow(&runner, job, &emit);
    }
    let research = match (&run.playbook, &inner.deps.research) {
        (Some(Playbook::DeepResearch), Some(io)) => Some(RunResearch {
            hooks: hooks.clone(),
            io: io.clone(),
            workspace: run.workspace.clone(),
        }),
        _ => None,
    };
    let build = (run.playbook == Some(Playbook::Build)).then(|| RunBuild { hooks: hooks.clone(), created_ms: run.created_ms });
    let hosts = playbook::Hosts {
        research: research.as_ref().map(|r| r as &dyn playbook::ResearchHost),
        build: build.as_ref().map(|b| b as &dyn playbook::BuildHost),
    };
    playbook::run_with(run.playbook, &run.role, &run.goal, &runner, &cfg, hosts)
}

/// Records how a run thread ended. A user's cancel wins over pause and shutdown; a stopped run
/// (shutdown) is left `running` in the store so the next start resumes it.
fn finish(inner: &Arc<Inner>, id: &RunId, provider: &str, outcome: Result<Result<Dispatch, CrewError>, String>) {
    let mut events = Vec::new();
    let mut memory: Option<(Run, Dispatch)> = None;
    {
        let mut st = inner.state();
        if let Some(n) = st.active.get_mut(provider) {
            *n = n.saturating_sub(1);
        }
        st.threads = st.threads.saturating_sub(1);
        let Some(live) = st.live.get_mut(id) else { return };
        live.running = false;
        let ctl = live.ctl.clone();
        let user_cancel = ctl.user_cancel.load(Ordering::SeqCst);
        // Parked at the plan gate: the run stays `needs_you` with no thread, unless the answer
        // came while the thread was still ending, which queues it again now.
        if ctl.parked.swap(false, Ordering::SeqCst) && !user_cancel {
            let answered = inner.decided.lock().unwrap_or_else(|e| e.into_inner()).contains_key(id);
            let ev = if answered && !ctl.stop.load(Ordering::SeqCst) {
                if !st.queue.contains(id) {
                    st.queue.push_back(id.clone());
                }
                inner.touch(&mut st, id, |r| {
                    r.status = CrewRunStatus::Queued;
                    r.note = Some("plan approved; waiting for a slot to explore".into());
                    r.current_action = None;
                })
            } else {
                let ev = inner.touch(&mut st, id, |r| r.current_action = None);
                st.live.remove(id);
                ev
            };
            drop(st);
            if let Some(ev) = ev {
                inner.emit(ev);
            }
            return;
        }
        if ctl.stop.load(Ordering::SeqCst) && !user_cancel {
            st.live.remove(id);
            return;
        }
        let paused = ctl.pause.load(Ordering::SeqCst);
        let mut keep = false;
        let ev = match outcome {
            Ok(Ok(dispatch)) => {
                let needs_merge = dispatch.diff.as_ref().is_some_and(|d| !d.files.is_empty());
                let ev = inner.touch(&mut st, id, |r| {
                    r.status = if needs_merge { CrewRunStatus::NeedsYou } else { CrewRunStatus::Done };
                    r.note = needs_merge.then(|| "review the diff, then merge or discard".to_string());
                    r.current_action = None;
                    r.result = Some(dispatch.clone());
                    r.relay_due = r.session.is_some();
                });
                if let Some(run) = st.live.get(id).map(|l| l.run.clone()) {
                    events.extend(ev);
                    events.push(CrewEvent::Finished { run: id.clone(), session: run.session.clone(), dispatch: dispatch.clone() });
                    // Deep research v2 stores its own findings (REMEMBER); v1 leaves one summary.
                    if run.playbook == Some(Playbook::DeepResearch) && inner.deps.research.is_none() {
                        memory = Some((run, dispatch));
                    }
                }
                None
            }
            Ok(Err(_)) if user_cancel || (ctl.cancel.is_cancelled() && !paused) => {
                let note = ctl.end_note.lock().unwrap_or_else(|e| e.into_inner()).take();
                inner.touch(&mut st, id, |r| {
                    r.status = CrewRunStatus::Cancelled;
                    r.note = note;
                    r.current_action = None;
                })
            }
            Ok(Err(CrewError::Stopped | CrewError::Cancelled)) if paused => {
                keep = true;
                ctl.pause.store(false, Ordering::SeqCst);
                inner.touch(&mut st, id, |r| {
                    r.status = CrewRunStatus::Paused;
                    r.current_action = None;
                })
            }
            Ok(Err(e)) => inner.touch(&mut st, id, |r| {
                r.status = CrewRunStatus::Failed;
                r.note = Some(e.to_string());
                r.current_action = None;
                r.relay_due = r.session.is_some();
            }),
            Err(panic) => inner.touch(&mut st, id, |r| {
                r.status = CrewRunStatus::Failed;
                r.note = Some(format!("the run panicked: {}", clip(&panic, 300)));
                r.current_action = None;
                r.relay_due = r.session.is_some();
            }),
        };
        events.extend(ev);
        if !keep {
            st.live.remove(id);
        }
    }
    for ev in events {
        inner.emit(ev);
    }
    if let (Some((run, dispatch)), Some(mind)) = (memory, inner.deps.mind.as_ref()) {
        let _ = mind.remember(result_memory(&run, &dispatch));
    }
}

/// The supervisor's side of every agent step.
pub(crate) struct Hooks {
    inner: Arc<Inner>,
    id: RunId,
    session: Option<SessionId>,
    ctl: Arc<Control>,
    /// Outputs of completed steps by key (stored ones at start, then each as it finishes).
    done: Mutex<HashMap<String, StepResult>>,
    /// The last checkpoint row number handed out.
    rows: Mutex<u32>,
    /// Row numbers reserved by steps in flight (by key), so live `Step` events and the checkpoint
    /// carry the number the transcript will show, also when steps run in parallel.
    live_rows: Mutex<HashMap<String, u32>>,
    /// Steps (by key) whose streamed text stopped mid-line, so a tool line starts a fresh one.
    mid_line: Mutex<HashSet<String>>,
    /// One budget approval at a time, however many steps run in parallel.
    budget_gate: Mutex<()>,
    /// Model calls admitted and not finished. They count against the steps budget, so parallel
    /// steps cannot together run past it before the approval.
    in_flight: Mutex<u32>,
    /// Price per role, for providers that report no cost.
    pricing: Mutex<HashMap<String, Option<Pricing>>>,
    /// The spin-up's model step has been reported (the first step's model resolved).
    model_spun: std::sync::atomic::AtomicBool,
}

/// The run's budget in a few characters for the spin-up line: `600k` tokens, else steps or dollars.
fn budget_echo(b: &marlowe_proto::CrewBudget) -> String {
    match (b.tokens, b.steps, b.usd) {
        (Some(t), _, _) if t >= 1_000_000 => format!("{:.1}M", t as f64 / 1e6),
        (Some(t), _, _) if t >= 1_000 => format!("{}k", t / 1_000),
        (Some(t), _, _) => t.to_string(),
        (None, Some(s), _) => format!("{s} steps"),
        (None, None, Some(u)) => format!("${u:.2}"),
        _ => "uncapped".into(),
    }
}

impl Hooks {
    /// The transcript row for the step `key`: reserved on its first event, kept until it finishes.
    fn row(&self, key: &str) -> u32 {
        let mut live = self.live_rows.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = live.get(key) {
            return *n;
        }
        let mut rows = self.rows.lock().unwrap_or_else(|e| e.into_inner());
        *rows += 1;
        live.insert(key.to_string(), *rows);
        *rows
    }

    /// One line of tool activity in the step's live transcript, on a line of its own.
    fn tool_line(&self, spec: &StepSpec, line: String) {
        let lead = if self.mid_line.lock().unwrap_or_else(|e| e.into_inner()).remove(&spec.key) { "\n" } else { "" };
        let step = self.row(&spec.key);
        // The event's action becomes the rail's current action: keep the tool summary in it.
        let action = self.run().ok().and_then(|r| r.current_action).unwrap_or_else(|| spec.label.clone());
        self.inner.emit(CrewEvent::Step {
            run: self.id.clone(),
            step,
            action,
            text_delta: Some(format!("{lead}{}\n", line.replace('\n', " "))),
        });
    }

    /// Cancel → `Cancelled`; shutdown or pause → `Stopped`.
    fn check(&self) -> Result<(), CrewError> {
        if self.ctl.stop.load(Ordering::SeqCst) || self.ctl.pause.load(Ordering::SeqCst) {
            return Err(CrewError::Stopped);
        }
        if self.ctl.cancel.is_cancelled() {
            return Err(CrewError::Cancelled);
        }
        Ok(())
    }

    fn run(&self) -> Result<Run, CrewError> {
        self.inner.run(&self.id)
    }

    fn cost(&self, role: &str, usage: &Usage) -> f64 {
        if let Some(c) = usage.cost_usd {
            return c;
        }
        let mut cache = self.pricing.lock().unwrap_or_else(|e| e.into_inner());
        let price = *cache.entry(role.to_string()).or_insert_with(|| {
            let roles = self.inner.roles.lock().unwrap_or_else(|e| e.into_inner()).clone();
            roles.get(role).and_then(|card| self.inner.priced_model(card, self.session.as_ref())).and_then(|m| m.pricing)
        });
        price.map_or(0.0, |p| p.cost(usage.input, usage.output))
    }

    /// While over budget: `NeedsYou` and an approval to extend; a refusal pauses the run.
    fn enforce_budget(&self) -> Result<(), CrewError> {
        let _gate = self.budget_gate.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            self.check()?;
            let run = self.run()?;
            match run.budget.exceeded(&run.spent) {
                None => return Ok(()),
                Some(dim) => self.ask_extension(&run, &run.spent, dim)?,
            }
        }
    }

    /// Admits one model call: the run's spend plus the calls already in flight must leave room
    /// for it, else the budget approval comes first. Holds the call's place until `after_call`.
    fn reserve(&self) -> Result<(), CrewError> {
        let _gate = self.budget_gate.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            self.check()?;
            let (run, projected) = {
                let mut n = self.in_flight.lock().unwrap_or_else(|e| e.into_inner());
                let run = self.run()?;
                let projected = CrewSpent { steps: run.spent.steps + *n, ..run.spent };
                if run.budget.exceeded(&projected).is_none() {
                    *n += 1;
                    return Ok(());
                }
                (run, projected)
            };
            let dim = run.budget.exceeded(&projected).unwrap_or("steps");
            self.ask_extension(&run, &projected, dim)?;
        }
    }

    /// Asks to extend the exceeded dimension `dim`. `Ok` once extended; a refusal pauses the run
    /// (a refusal that comes from a cancel ends it cancelled instead).
    fn ask_extension(&self, run: &Run, spent: &CrewSpent, dim: &'static str) -> Result<(), CrewError> {
        let (used, limit) = match dim {
            "steps" => (spent.steps.to_string(), run.budget.steps.unwrap_or(0).to_string()),
            "tokens" => (spent.tokens.to_string(), run.budget.tokens.unwrap_or(0).to_string()),
            _ => (format!("${:.2}", spent.usd), format!("${:.2}", run.budget.usd.unwrap_or(0.0))),
        };
        let pct = (self.inner.cfg.budget_extension * 100.0).round();
        let summary = format!("{} run reached its {dim} budget ({used} of {limit}); extend it by {pct}%?", run.role);
        self.inner.update(&self.id, |r| {
            r.status = CrewRunStatus::NeedsYou;
            r.note = Some(summary.clone());
        });
        let req = ApprovalReq {
            session: crew_session(&self.id),
            call_id: format!("budget-{}-{}", self.id.short(), spent.steps),
            tool: "budget".into(),
            summary: summary.clone(),
            detail: format!("run {}
goal: {}
budget: {:?}
spent: {:?}", self.id, run.goal, run.budget, spent),
        };
        let decision = self.inner.deps.approvals.ask(&self.id, self.session.as_ref(), req);
        // A stop (shutdown, a closed window) beats the refusal that woke us: the run was working,
        // not waiting on the user, so it stays `running` for the interrupted-run bookkeeping.
        if self.ctl.stop.load(Ordering::SeqCst) && !self.ctl.user_cancel.load(Ordering::SeqCst) {
            self.inner.update(&self.id, |r| {
                r.status = CrewRunStatus::Running;
                r.note = None;
            });
            return Err(CrewError::Stopped);
        }
        if self.ctl.user_cancel.load(Ordering::SeqCst) || self.ctl.cancel.is_cancelled() {
            return Err(CrewError::Cancelled);
        }
        if decision.is_allowed() {
            let fraction = self.inner.cfg.budget_extension;
            self.inner.update(&self.id, |r| {
                extend_budget(&mut r.budget, dim, fraction);
                r.status = CrewRunStatus::Running;
                r.note = None;
            });
            return Ok(());
        }
        self.inner.update(&self.id, |r| r.note = Some(format!("stopped at its {dim} budget; resume to ask again")));
        self.ctl.pause.store(true, Ordering::SeqCst);
        Err(CrewError::Stopped)
    }

    /// The step's model. While the GPU is busy and no cloud slot is configured, a local model
    /// waits here (the run shows "waiting for the GPU"), honouring cancel, pause and shutdown.
    fn resolve(&self, card: &RoleCard) -> Result<StepModel, CrewError> {
        let mut waiting = false;
        loop {
            let choice = self.inner.choose(card, self.session.as_ref())?;
            if !choice.waits_for_gpu {
                if !self.model_spun.swap(true, Ordering::SeqCst) {
                    let ev = PhaseEvent::end(PhaseKind::Spinup, 0).stage("model").detail(choice.model.model.id.as_str());
                    self.inner.emit(CrewEvent::Phase(PhaseEvent { run: Some(self.id.clone()), ..ev }));
                }
                // A crew slot's model that could not load: said once, kept with the run.
                if let Some(note) = choice.note {
                    let new = self.inner.run(&self.id).map_or(true, |r| r.model_note.as_deref() != Some(note.as_str()));
                    if new {
                        self.progress(&note);
                        self.inner.update(&self.id, |r| r.model_note = Some(note));
                    }
                }
                return Ok(choice.model);
            }
            if !waiting {
                self.progress(GPU_WAIT);
                waiting = true;
            }
            for _ in 0..10 {
                self.check()?;
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }

    /// One of `model`'s stream slots (`crew_slots` across every run).
    fn local_slot(&self, model: &ModelRef) -> Result<(), CrewError> {
        self.room_for_a_call(model)?;
        let limit = self.inner.deps.brains.context().crew_slots(model);
        let mut n = self.inner.local_streams.lock().unwrap_or_else(|e| e.into_inner());
        let mut said = false;
        loop {
            let held = n.entry(model.to_string()).or_default();
            if *held < limit {
                *held += 1;
                return Ok(());
            }
            if !said {
                said = true;
                // Waiting behind a runner that serves fewer than planned: say why, once.
                if let Some(note) = self.inner.deps.brains.context().slots_note(model) {
                    drop(n);
                    self.progress(&format!("waiting for a model slot: {note}"));
                    n = self.inner.local_streams.lock().unwrap_or_else(|e| e.into_inner());
                    continue;
                }
            }
            self.check()?;
            n = self
                .inner
                .local_free
                .wait_timeout(n, Duration::from_millis(200))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    /// Holds a new call on a local model while Windows is short of commit memory (the runner's
    /// cache grows with each new prompt, and at the limit every process on the machine, the daemon
    /// and the runner included, fails its next allocation). Fails the call with the reason when
    /// memory does not come back within `memory_wait`.
    fn room_for_a_call(&self, model: &ModelRef) -> Result<(), CrewError> {
        let cfg = &self.inner.cfg;
        let short = || (cfg.commit)().filter(|c| c.available < cfg.commit_floor);
        let Some(mut c) = short() else { return Ok(()) };
        let gb = |b: u64| b as f64 / (1u64 << 30) as f64;
        self.progress(&format!("waiting for memory: Windows has {:.1} GB of commit left", gb(c.available)));
        let end = Instant::now() + cfg.memory_wait;
        while Instant::now() < end {
            self.check()?;
            std::thread::sleep(Duration::from_millis(250).min(cfg.memory_wait));
            match short() {
                None => return Ok(()),
                Some(now) => c = now,
            }
        }
        Err(CrewError::Step {
            step: model.to_string(),
            message: format!(
                "Windows is out of memory: {:.1} GB of its {:.0} GB commit limit is left, under the {:.1} GB a model call \
                 needs to be safe (the local model's runner grows with each new prompt). Close programs, unload models or \
                 enlarge the page file, then resume",
                gb(c.available),
                gb(c.limit),
                gb(cfg.commit_floor)
            ),
        })
    }

    fn release_local(&self, model: &ModelRef) {
        let mut n = self.inner.local_streams.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(held) = n.get_mut(&model.to_string()) {
            *held = held.saturating_sub(1);
        }
        drop(n);
        self.inner.local_free.notify_one();
    }
}

impl StepHooks for Hooks {
    fn completed(&self, key: &str) -> Option<StepResult> {
        self.done.lock().unwrap_or_else(|e| e.into_inner()).get(key).cloned()
    }

    fn admit(&self, _spec: &StepSpec) -> Result<(), CrewError> {
        self.check()?;
        self.enforce_budget()
    }

    fn before_call(&self, spec: &StepSpec, local: Option<&ModelRef>) -> Result<(), CrewError> {
        self.reserve()?;
        if let Some(model) = local {
            if let Err(e) = self.local_slot(model) {
                let mut n = self.in_flight.lock().unwrap_or_else(|e| e.into_inner());
                *n = n.saturating_sub(1);
                return Err(e);
            }
            self.inner.local_holders.lock().unwrap_or_else(|e| e.into_inner()).push((self.id.clone(), spec.role.clone(), model.to_string()));
        }
        Ok(())
    }

    /// Frees the call's place and slot, and books what it spent.
    fn after_call(&self, spec: &StepSpec, local: Option<&ModelRef>, usage: Option<&Usage>) {
        if let Some(model) = local {
            let mut holders = self.inner.local_holders.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(i) = holders.iter().position(|(r, role, _)| *r == self.id && *role == spec.role) {
                holders.remove(i);
            }
            drop(holders);
            self.release_local(model);
        }
        let cost = usage.map(|u| self.cost(&spec.role, u));
        let mut n = self.in_flight.lock().unwrap_or_else(|e| e.into_inner());
        *n = n.saturating_sub(1);
        if let (Some(u), Some(cost)) = (usage, cost) {
            self.inner.update(&self.id, |r| {
                r.step += 1;
                r.spent.steps += 1;
                r.spent.tokens += u.total();
                r.spent.usd += cost;
            });
        }
    }

    fn remaining(&self) -> Budget {
        let Ok(run) = self.run() else { return Budget::default() };
        let (b, s) = (run.budget, run.spent);
        Budget {
            max_steps: b.steps.map_or(MAX_STEPS, |m| m.saturating_sub(s.steps).clamp(1, MAX_STEPS)),
            max_tokens: b.tokens.map(|m| m.saturating_sub(s.tokens).max(1)),
            max_cost_usd: b.usd.map(|m| (m - s.usd).max(0.0001)),
        }
    }

    fn tainted(&self) -> bool {
        self.run().is_ok_and(|r| r.tainted)
    }

    fn event(&self, spec: &StepSpec, ev: &AgentEvent) {
        // Spend is booked per model call in `after_call`.
        match ev {
            AgentEvent::TextDelta(t) => {
                if let Some(last) = t.chars().last() {
                    let mut mid = self.mid_line.lock().unwrap_or_else(|e| e.into_inner());
                    if last == '\n' {
                        mid.remove(&spec.key);
                    } else {
                        mid.insert(spec.key.clone());
                    }
                }
                let step = self.row(&spec.key);
                self.inner.emit(CrewEvent::Step {
                    run: self.id.clone(),
                    step,
                    action: spec.label.clone(),
                    text_delta: Some(t.clone()),
                });
            }
            AgentEvent::ToolStarted { summary, .. } => {
                let action = format!("{} · {}", spec.label, clip(summary, 80));
                self.inner.update(&self.id, |r| r.current_action = Some(action));
                self.tool_line(spec, format!("▸ {}", clip(summary, 160)));
            }
            AgentEvent::ToolFinished { ok, summary, .. } => {
                let mark = if *ok { "↳" } else { "✗" };
                self.tool_line(spec, format!("  {mark} {}", clip(summary, 120)));
            }
            AgentEvent::SteerApplied { text } => {
                let step = self.row(&spec.key);
                self.inner.emit(CrewEvent::Step {
                    run: self.id.clone(),
                    step,
                    action: format!("steer: {}", clip(text, 80)),
                    text_delta: None,
                });
            }
            _ => {}
        }
    }

    fn finished(&self, spec: &StepSpec, messages: &[Message], result: &StepResult) {
        let step = self.row(&spec.key);
        self.live_rows.lock().unwrap_or_else(|e| e.into_inner()).remove(&spec.key);
        self.mid_line.lock().unwrap_or_else(|e| e.into_inner()).remove(&spec.key);
        let calls: Vec<String> = messages.iter().flat_map(|m| m.tool_calls()).map(call_line).take(4).collect();
        let action = if calls.is_empty() { spec.label.clone() } else { format!("{} · {}", spec.label, calls.join(" · ")) };
        let rec = StepRecord {
            run: self.id.clone(),
            step,
            key: spec.key.clone(),
            role: spec.role.clone(),
            action: clip(&action, 160),
            messages: messages.to_vec(),
            output: Some(result.text.clone()),
            usage: result.usage,
            tainted: result.tainted,
            truncated: result.truncated,
            at_ms: crate::now_ms(),
        };
        let _ = self.inner.store().append_step(&rec);
        // The step row holds the whole conversation now; its per-call checkpoints are done.
        let _ = self.inner.store().clear_calls(&self.id, &spec.key);
        self.done.lock().unwrap_or_else(|e| e.into_inner()).insert(spec.key.clone(), result.clone());
        if result.tainted {
            self.inner.update(&self.id, |r| r.tainted = true);
        }
        self.inner.emit(CrewEvent::Step { run: self.id.clone(), step, action: rec.action, text_delta: None });
    }

    /// A step with no answer gets a transcript row with what it spent and how its last call
    /// stopped, and no output: a resumed run asks again.
    fn failed(&self, spec: &StepSpec, messages: &[Message], f: &StepFailure) {
        let step = self.row(&spec.key);
        self.live_rows.lock().unwrap_or_else(|e| e.into_inner()).remove(&spec.key);
        self.mid_line.lock().unwrap_or_else(|e| e.into_inner()).remove(&spec.key);
        let mut action = format!(
            "{} · {} ({} output tokens, {} of them reasoning",
            spec.label, f.why, f.usage.output, f.usage.reasoning
        );
        if let Some(cap) = f.max_output {
            action.push_str(&format!(", cap {cap}"));
        }
        action.push_str(if f.truncated { "; stopped at the output limit)" } else { ")" });
        let rec = StepRecord {
            run: self.id.clone(),
            step,
            key: spec.key.clone(),
            role: spec.role.clone(),
            action: clip(&action, 200),
            messages: messages.to_vec(),
            output: None,
            usage: f.usage,
            tainted: self.tainted(),
            truncated: f.truncated,
            at_ms: crate::now_ms(),
        };
        let _ = self.inner.store().append_step(&rec);
        self.inner.emit(CrewEvent::Step { run: self.id.clone(), step, action: rec.action, text_delta: None });
    }

    fn progress(&self, action: &str) {
        let action = clip(action, 120);
        self.inner.update(&self.id, |r| r.current_action = Some(action));
    }

    fn model_note(&self, note: &str) {
        self.progress(note);
        let note = clip(note, 300);
        self.inner.update(&self.id, |r| match r.model_note.as_deref() {
            Some(old) if old.contains(note.as_str()) => {}
            Some(old) => r.model_note = Some(format!("{old}; {note}")),
            None => r.model_note = Some(note),
        });
    }

    fn partial(&self, key: &str) -> PartialStep {
        self.inner.store().calls(&self.id, key).unwrap_or_default()
    }

    fn checkpoint_call(&self, spec: &StepSpec, checkpoint: &CallCheckpoint<'_>) {
        let _ = self.inner.store().append_call(&self.id, &spec.key, self.row(&spec.key), checkpoint);
    }
}

impl Hooks {
    /// A playbook data checkpoint: a transcript row with no messages, its output the data.
    fn save_data(&self, key: &str, label: &str, data: &str) {
        let step = self.row(key);
        self.live_rows.lock().unwrap_or_else(|e| e.into_inner()).remove(key);
        let rec = StepRecord {
            run: self.id.clone(),
            step,
            key: key.to_string(),
            role: "playbook".into(),
            action: clip(label, 160),
            messages: Vec::new(),
            output: Some(data.to_string()),
            usage: Usage::default(),
            tainted: false,
            truncated: false,
            at_ms: crate::now_ms(),
        };
        let _ = self.inner.store().append_step(&rec);
        let result = StepResult { text: data.to_string(), ..Default::default() };
        self.done.lock().unwrap_or_else(|e| e.into_inner()).insert(key.to_string(), result);
        self.inner.emit(CrewEvent::Step { run: self.id.clone(), step, action: rec.action, text_delta: None });
    }
}

/// Deep research v2's view of its run: plan approval, stage progress, data checkpoints, the
/// user's profile and memory, and the channels.
struct RunResearch {
    hooks: Arc<Hooks>,
    io: Arc<dyn ResearchIo>,
    workspace: Option<PathBuf>,
}

impl RunResearch {
    fn scope(&self) -> Option<MemoryScope> {
        self.workspace.as_ref().map(|w| MemoryScope::Project(w.display().to_string()))
    }
}

impl playbook::ResearchHost for RunResearch {
    fn run_id(&self) -> RunId {
        self.hooks.id.clone()
    }

    fn io(&self) -> &dyn ResearchIo {
        self.io.as_ref()
    }

    fn cancel(&self) -> &CancelToken {
        &self.hooks.ctl.cancel
    }

    fn check(&self) -> Result<(), CrewError> {
        self.hooks.check()
    }

    fn channel(&self, ev: &marlowe_research::ChannelEvent) {
        use marlowe_research::ChannelEvent as C;
        let phase = |c: marlowe_proto::ChannelId| {
            marlowe_proto::PhaseEvent { run: Some(self.hooks.id.clone()), ..marlowe_proto::PhaseEvent::start(marlowe_proto::PhaseKind::Channel) }.stage(c.label())
        };
        let ev = match ev {
            C::Online(c) => phase(*c),
            C::Retrying { channel, status, .. } => {
                let mut p = phase(*channel).detail(format!("{status} retrying"));
                p.state = marlowe_proto::PhaseState::Progress;
                p
            }
            C::Done { channel, n } => {
                let mut p = phase(*channel).with_done(*n as u64);
                p.state = marlowe_proto::PhaseState::End;
                p
            }
            C::Failed { channel, why } => {
                let mut p = phase(*channel).detail(format!("failed: {why}"));
                p.state = marlowe_proto::PhaseState::End;
                p
            }
        };
        self.hooks.inner.emit(CrewEvent::Phase(ev));
    }

    fn progress(&self, p: &ResearchProgress) {
        let mut action = format!("research {}/7 {}", p.stage.ordinal(), p.stage.as_str());
        if p.total > 0 {
            action.push_str(&format!(" {}/{}", p.done, p.total));
        }
        if let Some(d) = &p.detail {
            action.push_str(&format!(": {d}"));
        }
        self.hooks.progress(&action);
        self.hooks.inner.emit(CrewEvent::Research { run: self.hooks.id.clone(), progress: p.clone() });
    }

    /// The plan gate. An answer given while the run was parked is taken at once. Otherwise the
    /// plan is pending, the run is `needs_you`, and the thread ends (`Stopped` with `parked` set):
    /// a waiting plan holds no thread and no concurrency slot, and
    /// [`crate::Supervisor::approve_plan`] queues the run again with its answer.
    fn propose_plan(&self, plan: &ResearchPlan, quote: &CostQuote) -> Result<PlanDecision, CrewError> {
        let (inner, id) = (&self.hooks.inner, &self.hooks.id);
        if let Some(d) = inner.decided.lock().unwrap_or_else(|e| e.into_inner()).remove(id) {
            return Ok(d);
        }
        self.hooks.check()?;
        self.hooks.ctl.parked.store(true, Ordering::SeqCst);
        let pending = PendingPlan { plan: plan.clone(), quote: quote.clone() };
        inner.plans.lock().unwrap_or_else(|e| e.into_inner()).insert(id.clone(), pending);
        let note = format!(
            "research plan ready: {} sub-questions, about {} model tokens, {} fetches, {} min; approve, edit or reject it",
            plan.sub_questions.len(),
            quote.model_tokens,
            quote.fetches,
            quote.minutes
        );
        inner.update(id, |r| {
            r.status = CrewRunStatus::NeedsYou;
            r.note = Some(note);
            r.current_action = None;
        });
        inner.emit(CrewEvent::PlanProposed {
            run: id.clone(),
            session: self.hooks.session.clone(),
            plan: plan.clone(),
            quote: quote.clone(),
        });
        Err(CrewError::Stopped)
    }

    /// Approving the plan approves its quote: limits rise to cover it (+25% tokens), never fall,
    /// and an unlimited dimension stays unlimited. Spend beyond that asks again as usual.
    fn plan_approved(&self, _plan: &ResearchPlan, quote: &CostQuote, steps: u32) {
        let tokens = quote.model_tokens.saturating_add(quote.model_tokens / 4);
        let updated = self.hooks.inner.update(&self.hooks.id, |r| {
            if let Some(t) = r.budget.tokens.as_mut() {
                *t = (*t).max(tokens);
            }
            if let Some(s) = r.budget.steps.as_mut() {
                *s = (*s).max(steps);
            }
        });
        if let Some(r) = updated {
            let b = r.budget;
            self.hooks.progress(&format!(
                "plan approved; budget now {} tokens, {} steps",
                b.tokens.map_or("unlimited".into(), |t| t.to_string()),
                b.steps.map_or("unlimited".into(), |s| s.to_string())
            ));
        }
    }

    fn load(&self, key: &str) -> Option<String> {
        self.hooks.completed(key).map(|r| r.text)
    }

    fn save(&self, key: &str, label: &str, data: &str) {
        self.hooks.save_data(key, label, data);
    }

    fn profile(&self) -> Option<String> {
        let mind = self.hooks.inner.deps.mind.as_ref()?;
        let workspace = self.workspace.as_ref().map(|w| w.display().to_string());
        let p = mind.profile(ProfileQuery { workspace, budget_tokens: 0 }).ok()?;
        (!p.block.trim().is_empty()).then_some(p.block)
    }

    fn recall(&self, query: &str, limit: usize) -> Vec<Recalled> {
        let Some(mind) = self.hooks.inner.deps.mind.as_ref() else { return Vec::new() };
        let q = RecallQuery { text: query.to_string(), scope: self.scope(), limit };
        mind.recall(q)
            .unwrap_or_default()
            .into_iter()
            .map(|h| Recalled { id: h.id, text: h.text, provenance: h.provenance, trust: h.trust })
            .collect()
    }

    fn taint(&self) {
        if !self.hooks.tainted() {
            self.hooks.inner.update(&self.hooks.id, |r| r.tainted = true);
        }
    }

    fn tainted(&self) -> bool {
        self.hooks.tainted()
    }

    /// Worst case over lineage: the run's own taint latch counts whatever the playbook says, and
    /// the recalled memories the finding rests on are its parents in the store.
    fn remember(&self, finding: &Finding, tainted: bool) -> Result<String, String> {
        let mind = self.hooks.inner.deps.mind.as_ref().ok_or("no memory store is open")?;
        let turn = self.hooks.run().map(|r| r.step).unwrap_or(0);
        let tainted = tainted || self.hooks.tainted();
        let memory = MemoryIn {
            text: finding.text.clone(),
            kind: Kind::Result,
            scope: self.scope().unwrap_or(MemoryScope::Global),
            source: Provenance::Agent { session: format!("crew:{}", self.hooks.id), turn, tainted },
        };
        mind.remember_derived(memory, finding.derived_from.clone()).map(|r| r.id).map_err(|e| e.to_string())
    }
}

/// build_v2's view of its run: plan approval and questions through the run's approvals, data
/// checkpoints, child worktrees, memory.
struct RunBuild {
    hooks: Arc<Hooks>,
    created_ms: i64,
}

impl RunBuild {
    /// Asks on the run's behalf; a cancel while waiting ends the run.
    fn ask(&self, tool: &str, summary: String, detail: String) -> Result<bool, CrewError> {
        let (inner, id) = (&self.hooks.inner, &self.hooks.id);
        self.hooks.check()?;
        let req = ApprovalReq {
            session: crew_session(id),
            call_id: format!("{tool}-{}-{}", id.short(), crate::now_ms()),
            tool: tool.into(),
            summary,
            detail,
        };
        let decision = ask_for_run(inner, id, self.hooks.session.as_ref(), req);
        if self.hooks.ctl.user_cancel.load(Ordering::SeqCst) || self.hooks.ctl.cancel.is_cancelled() {
            return Err(CrewError::Cancelled);
        }
        Ok(decision.is_allowed())
    }
}

impl playbook::BuildHost for RunBuild {
    fn progress(&self, action: &str) {
        self.hooks.progress(action);
    }

    fn check(&self) -> Result<(), CrewError> {
        self.hooks.check()
    }

    fn propose_plan(&self, plan: &BuildPlan, quote: &BuildQuote) -> Result<bool, CrewError> {
        let summary = format!(
            "build plan: {} task{} in {} wave{}, about {} agent steps; approve it?",
            plan.tasks.len(),
            if plan.tasks.len() == 1 { "" } else { "s" },
            quote.waves,
            if quote.waves == 1 { "" } else { "s" },
            quote.steps
        );
        self.ask("build_plan", summary, playbook::build_v2::render_plan(plan, quote))
    }

    /// Approving the plan approves its quote: the steps and tokens limits rise to cover it
    /// (+25%), never fall, and an unlimited dimension stays unlimited.
    fn plan_approved(&self, _plan: &BuildPlan, quote: &BuildQuote) {
        let tokens = quote.model_tokens.saturating_add(quote.model_tokens / 4);
        let steps = quote.steps.saturating_mul(12);
        self.hooks.inner.update(&self.hooks.id, |r| {
            if let Some(t) = r.budget.tokens.as_mut() {
                *t = (*t).max(tokens);
            }
            if let Some(s) = r.budget.steps.as_mut() {
                *s = (*s).max(steps);
            }
        });
    }

    fn ask_user(&self, summary: &str, detail: &str) -> Result<bool, CrewError> {
        self.ask("build", summary.to_string(), detail.to_string())
    }

    fn dag(&self, report: &str) {
        let step = self.hooks.run().map(|r| r.step).unwrap_or(0);
        self.hooks.inner.emit(CrewEvent::Step { run: self.hooks.id.clone(), step, action: report.to_string(), text_delta: None });
    }

    fn load(&self, key: &str) -> Option<String> {
        self.hooks.completed(key).map(|r| r.text)
    }

    fn save(&self, key: &str, label: &str, data: &str) {
        self.hooks.save_data(key, label, data);
    }

    fn spawn_worktree(&self, parent: &Sandbox, task: &str, frozen: &[String]) -> Result<Sandbox, CrewError> {
        parent.create_child(&self.hooks.inner.home, &self.hooks.id, task, frozen)
    }

    fn artifacts_dir(&self) -> Option<PathBuf> {
        Some(self.hooks.inner.home.join("runs").join(self.hooks.id.as_str()))
    }

    fn elapsed(&self) -> Duration {
        Duration::from_millis(crate::now_ms().saturating_sub(self.created_ms).max(0) as u64)
    }

    /// The memories the plan rests on, trust-labelled. An untrusted one taints the run (the lead
    /// read it), and their ids are kept as the parents of the build's Result memory.
    fn recall(&self, query: &str) -> Vec<String> {
        let Some(mind) = self.hooks.inner.deps.mind.as_ref() else { return Vec::new() };
        let scope = self.hooks.run().ok().and_then(|r| r.workspace).map(|w| MemoryScope::Project(w.display().to_string()));
        let hits = mind.recall(RecallQuery { text: query.to_string(), scope, limit: 5 }).unwrap_or_default();
        if hits.iter().any(|h| h.trust == MemoryTrust::Untrusted) && !self.hooks.tainted() {
            self.hooks.inner.update(&self.hooks.id, |r| r.tainted = true);
        }
        let ids: Vec<&str> = hits.iter().map(|h| h.id.as_str()).collect();
        if let (false, Ok(json)) = (ids.is_empty(), serde_json::to_string(&ids)) {
            self.hooks.save_data(RECALLED_KEY, &format!("{} memories recalled for the plan", ids.len()), &json);
        }
        hits.into_iter().map(|h| format!("- ({:?} trust) {}", h.trust, clip(&h.text, 300))).collect()
    }

    /// The build's outcome as a Result memory, at the run's trust (worst case over what it read)
    /// and derived from the memories its plan recalled, so their trust caps it in the store.
    fn remember(&self, text: &str) {
        let Some(mind) = self.hooks.inner.deps.mind.as_ref() else { return };
        let Ok(run) = self.hooks.run() else { return };
        let parents: Vec<String> =
            self.hooks.completed(RECALLED_KEY).and_then(|r| serde_json::from_str(&r.text).ok()).unwrap_or_default();
        let memory = MemoryIn {
            text: format!("Crew build run {}: {text}", run.id.short()),
            kind: Kind::Result,
            scope: run.workspace.as_ref().map(|w| MemoryScope::Project(w.display().to_string())).unwrap_or(MemoryScope::Global),
            source: Provenance::Agent { session: format!("crew:{}", run.id), turn: run.step, tainted: run.tainted },
        };
        let _ = mind.remember_derived(memory, parents);
    }
}

/// The data checkpoint holding the ids of the memories a build's plan recalled.
const RECALLED_KEY: &str = "b2:recalled";

/// Approvals from a run's tools: the run shows `NeedsYou` while the user decides.
struct RunGate {
    inner: Arc<Inner>,
    id: RunId,
    session: Option<SessionId>,
}

impl ApprovalGate for RunGate {
    fn ask(&self, req: ApprovalReq) -> ApprovalDecision {
        ask_for_run(&self.inner, &self.id, self.session.as_ref(), req)
    }

    fn denial_reason(&self, call_id: &str) -> Option<String> {
        self.inner.deps.approvals.denial_reason(call_id)
    }
}

fn ask_for_run(inner: &Inner, id: &RunId, session: Option<&SessionId>, req: ApprovalReq) -> ApprovalDecision {
    let note = req.summary.clone();
    inner.update(id, |r| {
        r.status = CrewRunStatus::NeedsYou;
        r.note = Some(note);
    });
    let decision = inner.deps.approvals.ask(id, session, req);
    inner.update(id, |r| {
        if r.status == CrewRunStatus::NeedsYou {
            r.status = CrewRunStatus::Running;
            r.note = None;
        }
    });
    decision
}

/// The playbook's own shell (test commands): the sandbox's shell policy with the run's taint,
/// then [`sandbox_shell`].
fn guarded_shell(inner: Arc<Inner>, id: RunId, session: Option<SessionId>) -> playbook::ShellFn {
    Arc::new(move |sandbox: &Sandbox, command: &str, cancel: &CancelToken| -> Result<ShellResult, CrewError> {
        let tainted = inner.run(&id).is_ok_and(|r| r.tainted);
        // A hard deny (force push, `--no-verify`, shared git config) is refused, not asked.
        if let Verdict::Deny(why) = sandbox.shell_verdict(Dialect::Bash, command, false) {
            return Ok(ShellResult { ok: false, exit_code: None, output: format!("not run: refused ({why})") });
        }
        if let ShellVerdict::Ask(summary) = sandbox.classify_shell(command, tainted) {
            let call_id = format!("shell-{}-{}", id.short(), crate::now_ms());
            let req = ApprovalReq {
                session: crew_session(&id),
                call_id: call_id.clone(),
                tool: "shell".into(),
                summary: summary.clone(),
                detail: command.to_string(),
            };
            if !ask_for_run(&inner, &id, session.as_ref(), req).is_allowed() {
                let said = inner.deps.approvals.denial_reason(&call_id);
                let output = match said {
                    Some(r) => format!("not run: ({summary}) {}", marlowe_agent::run_tools::denied_text(Some(&r))),
                    None => format!("not run: the user declined ({summary})"),
                };
                return Ok(ShellResult { ok: false, exit_code: None, output });
            }
        }
        sandbox_shell(sandbox, command, cancel)
    })
}

/// The project engine's view of this run's model slots: employees take their slots from the same
/// pool as the run's agents and book their spend on the run (`Hooks::before_call` and
/// `after_call` do both, behind the commit-memory check and the GPU hold).
#[allow(dead_code)] // called by the project branch of `execute` (stage D)
pub(crate) fn project_gate(hooks: &Arc<Hooks>, models: Vec<ModelRef>) -> Arc<crate::project_gate::ProjectGate> {
    let brains = hooks.inner.deps.brains.clone();
    let per_provider = hooks.inner.cfg.per_provider;
    Arc::new(crate::project_gate::ProjectGate::new(
        hooks.clone(),
        models,
        Arc::new(move |m: &ModelRef| if m.provider.is_local() { brains.context().crew_slots(m) } else { per_provider }),
    ))
}
