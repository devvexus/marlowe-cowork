//! One run's thread: sandbox, the agent-backed step runner, and the hooks that make the run
//! durable and controllable (checkpoints, spend, budget approvals, pause/stop, live events).

use std::collections::HashMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossbeam_channel::RecvTimeoutError;
use marlowe_agent::{AgentEvent, ApprovalGate, ApprovalReq, Budget, MAX_STEPS};
use marlowe_mind::{Kind, MemoryIn, ProfileQuery, Provenance, RecallQuery};
use marlowe_model::{CancelToken, Message, Pricing, Usage};
use marlowe_proto::{
    ApprovalDecision, CostQuote, CrewRunStatus, CrewSpent, Dispatch, MemoryScope, PlanDecision, Playbook, ResearchPlan,
    ResearchProgress, RunId, SandboxKind, SessionId,
};

use crate::playbook::research_v2::{Finding, Recalled};
use crate::playbook::{
    self, sandbox_shell, AgentRunner, PlaybookConfig, ResearchIo, ShellResult, StepHooks, StepModel, StepResult, StepSpec,
};
use crate::role::RoleCard;
use crate::run::{call_line, clip, Run, StepRecord};
use crate::sandbox::{Sandbox, ShellVerdict};
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
pub(crate) fn run_thread(inner: Arc<Inner>, id: RunId, provider: &'static str) {
    let outcome = catch_unwind(AssertUnwindSafe(|| execute(&inner, &id)));
    let outcome = outcome.map_err(|p| {
        p.downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| p.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown panic".into())
    });
    finish(&inner, &id, provider, outcome);
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

    let sandbox = match run.sandbox.clone() {
        Some(sb) if sb.root.exists() => sb,
        _ => {
            let want = match run.playbook {
                Some(Playbook::Build) => SandboxKind::Worktree,
                Some(Playbook::DeepResearch) => SandboxKind::Scratch,
                None => card.sandbox,
            };
            inner.update(id, |r| r.current_action = Some("preparing the sandbox".into()));
            Sandbox::create(&inner.home, id, run.workspace.as_deref(), want)?
        }
    };
    inner.update(id, |r| r.sandbox = Some(sandbox.clone()));

    let done: HashMap<String, StepResult> = inner
        .store()
        .steps(id)?
        .into_iter()
        .filter_map(|s| s.output.clone().map(|text| (s.key, StepResult { text, usage: s.usage, tainted: s.tainted })))
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
        budget_gate: Mutex::new(()),
        in_flight: Mutex::new(0),
        pricing: Mutex::new(HashMap::new()),
    });

    let resolver = hooks.clone();
    let models: playbook::ModelResolver = Arc::new(move |card| resolver.resolve(card));
    let tools = (inner.deps.tools)(&sandbox, run.workspace.as_deref());
    let mut runner = AgentRunner::new(id.clone(), roles.clone(), models, tools, sandbox, run.workspace.clone());
    runner.approvals = Arc::new(RunGate { inner: inner.clone(), id: id.clone(), session: run.session.clone() });
    runner.cancel = ctl.cancel.clone();
    runner.inbox = inbox;
    runner.hooks = hooks.clone();
    runner.shell = guarded_shell(inner.clone(), id.clone(), run.session.clone());
    runner.per_provider = inner.cfg.per_provider;
    runner.per_local_provider = inner.cfg.per_local_provider;

    let cfg = PlaybookConfig {
        max_scouts: inner.cfg.max_scouts,
        max_test_iterations: inner.cfg.max_test_iterations,
        test_command: None,
        research: inner.cfg.research.clone(),
    };
    hooks.check()?;
    let research = match (&run.playbook, &inner.deps.research) {
        (Some(Playbook::DeepResearch), Some(io)) => Some(RunResearch {
            hooks: hooks.clone(),
            io: io.clone(),
            workspace: run.workspace.clone(),
        }),
        _ => None,
    };
    playbook::run_with(
        run.playbook,
        &run.role,
        &run.goal,
        &runner,
        &cfg,
        research.as_ref().map(|r| r as &dyn playbook::ResearchHost),
    )
}

/// Records how a run thread ended. A user's cancel wins over pause and shutdown; a stopped run
/// (shutdown) is left `running` in the store so the next start resumes it.
fn finish(inner: &Arc<Inner>, id: &RunId, provider: &'static str, outcome: Result<Result<Dispatch, CrewError>, String>) {
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
struct Hooks {
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
    /// One budget approval at a time, however many steps run in parallel.
    budget_gate: Mutex<()>,
    /// Model calls admitted and not finished. They count against the steps budget, so parallel
    /// steps cannot together run past it before the approval.
    in_flight: Mutex<u32>,
    /// Price per role, for providers that report no cost.
    pricing: Mutex<HashMap<String, Option<Pricing>>>,
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
            roles.get(role).and_then(|card| self.inner.choose(card).ok()).and_then(|c| c.model.model.pricing)
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
            let choice = self.inner.choose(card)?;
            if !choice.waits_for_gpu {
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

    /// One of the machine's local-model stream slots (`per_local_provider` across every run).
    fn local_slot(&self) -> Result<(), CrewError> {
        let limit = self.inner.cfg.per_local_provider.max(1);
        let mut n = self.inner.local_streams.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if *n < limit {
                *n += 1;
                return Ok(());
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

    fn release_local(&self) {
        let mut n = self.inner.local_streams.lock().unwrap_or_else(|e| e.into_inner());
        *n = n.saturating_sub(1);
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

    fn before_call(&self, _spec: &StepSpec, local: bool) -> Result<(), CrewError> {
        self.reserve()?;
        if local {
            if let Err(e) = self.local_slot() {
                let mut n = self.in_flight.lock().unwrap_or_else(|e| e.into_inner());
                *n = n.saturating_sub(1);
                return Err(e);
            }
        }
        Ok(())
    }

    /// Frees the call's place and slot, and books what it spent.
    fn after_call(&self, spec: &StepSpec, local: bool, usage: Option<&Usage>) {
        if local {
            self.release_local();
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
            at_ms: crate::now_ms(),
        };
        let _ = self.inner.store().append_step(&rec);
        self.done.lock().unwrap_or_else(|e| e.into_inner()).insert(spec.key.clone(), result.clone());
        if result.tainted {
            self.inner.update(&self.id, |r| r.tainted = true);
        }
        self.inner.emit(CrewEvent::Step { run: self.id.clone(), step, action: rec.action, text_delta: None });
    }

    fn progress(&self, action: &str) {
        let action = clip(action, 120);
        self.inner.update(&self.id, |r| r.current_action = Some(action));
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

    /// The run is `needs_you` until [`crate::Supervisor::approve_plan`] answers; cancel, pause
    /// and shutdown end the wait.
    fn propose_plan(&self, plan: &ResearchPlan, quote: &CostQuote) -> Result<PlanDecision, CrewError> {
        let (inner, id) = (&self.hooks.inner, &self.hooks.id);
        let (tx, rx) = crossbeam_channel::bounded(1);
        let pending = PendingPlan { plan: plan.clone(), quote: quote.clone(), tx };
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
        });
        inner.emit(CrewEvent::PlanProposed {
            run: id.clone(),
            session: self.hooks.session.clone(),
            plan: plan.clone(),
            quote: quote.clone(),
        });
        let decided = loop {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(d) => break Ok(d),
                Err(RecvTimeoutError::Timeout) => {
                    if let Err(e) = self.hooks.check() {
                        break Err(e);
                    }
                }
                Err(RecvTimeoutError::Disconnected) => break Err(CrewError::Stopped),
            }
        };
        inner.plans.lock().unwrap_or_else(|e| e.into_inner()).remove(id);
        inner.update(id, |r| {
            if r.status == CrewRunStatus::NeedsYou {
                r.status = CrewRunStatus::Running;
                r.note = None;
            }
        });
        decided
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
            .map(|h| Recalled { id: h.id, text: h.text, provenance: h.provenance })
            .collect()
    }

    fn taint(&self) {
        if !self.hooks.tainted() {
            self.hooks.inner.update(&self.hooks.id, |r| r.tainted = true);
        }
    }

    fn remember(&self, finding: &Finding, tainted: bool) -> Result<String, String> {
        let mind = self.hooks.inner.deps.mind.as_ref().ok_or("no memory store is open")?;
        let turn = self.hooks.run().map(|r| r.step).unwrap_or(0);
        let memory = MemoryIn {
            text: finding.text.clone(),
            kind: Kind::Result,
            scope: self.scope().unwrap_or(MemoryScope::Global),
            source: Provenance::Agent { session: format!("crew:{}", self.hooks.id), turn, tainted },
        };
        mind.remember(memory).map(|r| r.id).map_err(|e| e.to_string())
    }
}

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
        if let ShellVerdict::Ask(summary) = sandbox.classify_shell(command, tainted) {
            let req = ApprovalReq {
                session: crew_session(&id),
                call_id: format!("shell-{}-{}", id.short(), crate::now_ms()),
                tool: "shell".into(),
                summary: summary.clone(),
                detail: command.to_string(),
            };
            if !ask_for_run(&inner, &id, session.as_ref(), req).is_allowed() {
                return Ok(ShellResult { ok: false, exit_code: None, output: format!("not run: the user declined ({summary})") });
            }
        }
        sandbox_shell(sandbox, command, cancel)
    })
}
