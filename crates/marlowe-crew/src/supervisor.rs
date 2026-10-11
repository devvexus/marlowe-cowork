//! The supervisor: queue, per-provider concurrency, one thread per active run, controls.
//!
//! A run thread resolves its model from the role's slot chain (`crew:<role>`, then
//! `conductor`), creates its sandbox, then drives the playbook through an [`AgentRunner`] whose
//! hooks checkpoint every finished agent step, drain steers at step boundaries, honour
//! cancel/pause, and turn a budget overrun into `NeedsYou` with an approval to extend. A panic
//! in a run is caught (`catch_unwind`) and fails that run only. At start, runs left `queued` or
//! `running` by a previous daemon are picked up again and skip their completed steps.
//!
//! Concurrency: at most `per_provider` runs per cloud provider and `per_local_provider` per local
//! one, counted by the dispatched role. Every step resolves its own slot, so the same rules hold
//! per model call too: at most `per_local_provider` local streams at once across all runs, and
//! while the GPU is busy a local step moves to a cloud slot or waits ("waiting for the GPU").
//!
//! [`AgentRunner`]: crate::playbook::AgentRunner

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use marlowe_agent::{ApprovalReq, Steer};
use marlowe_brains::{Admission, Brains, Priority};
use marlowe_mind::{Kind, MemoryIn, Mind, Provenance};
use marlowe_model::{CancelToken, ModelRef};
use marlowe_proto::{
    ApprovalDecision, Confidence, CostQuote, CrewBudget, CrewRunInfo, CrewRunStatus, CrewStep, Dispatch, MemoryScope, PlanDecision,
    ResearchPlan, ResearchProgress, RoleInfo, RunId, SandboxKind, SessionId,
};

use crate::dispatch::{DispatchArgs, DispatchReceipt, Estimate};
use crate::playbook::research_v2::apply_decision;
use crate::playbook::{slug, ResearchIo, ResearchV2Config, StepModel, REPORT_FILE};
use crate::role::{RoleCard, Roles};
use crate::run::{clip, Run};
use crate::sandbox::MergeOutcome;
use crate::store::RunStore;
use crate::tools::CrewToolFactory;
use crate::CrewError;

/// Asks the user on a run's behalf. The daemon raises `ApprovalRequested` with `run` set and
/// `session` = the run's parent session.
pub trait RunApprovals: Send + Sync {
    /// Blocks the run's thread; returns `Deny` on timeout, cancel or shutdown.
    fn ask(&self, run: &RunId, session: Option<&SessionId>, req: ApprovalReq) -> ApprovalDecision;
}

/// Whether the Sentinel reports the GPU busy right now.
pub type GpuBusy = Arc<dyn Fn() -> bool + Send + Sync>;

/// Supervisor tunables. `Default` is what the daemon runs with.
#[derive(Debug, Clone)]
pub struct CrewConfig {
    /// Concurrent runs per cloud provider.
    pub per_provider: usize,
    /// Concurrent runs per local provider (Ollama, GGUF).
    pub per_local_provider: usize,
    /// Scouts in a deep_research run: at least 3, at most this.
    pub max_scouts: usize,
    /// Fix-and-retest iterations in a build run.
    pub max_test_iterations: u32,
    /// Raise applied to the exceeded dimension when the user approves a budget extension:
    /// the limit grows by this fraction of itself (0.5 = +50%).
    pub budget_extension: f64,
    /// Deep research v2: plan auto-approval (off by default), fetch concurrency, bounds.
    pub research: ResearchV2Config,
}

impl Default for CrewConfig {
    fn default() -> Self {
        Self {
            per_provider: 2,
            per_local_provider: 1,
            max_scouts: 6,
            max_test_iterations: 4,
            budget_extension: 0.5,
            research: ResearchV2Config::default(),
        }
    }
}

/// What the binary injects through `DaemonDeps`; the daemon adds models, approvals, memory and
/// the GPU probe when it starts the supervisor.
pub struct CrewSetup {
    pub tools: CrewToolFactory,
    pub config: CrewConfig,
    /// Research channels and the corpus fetcher ([`crate::playbook::research_v2::ChannelIo`]).
    /// `None` keeps deep research on v1.
    pub research: Option<Arc<dyn ResearchIo>>,
}

/// Everything a supervisor needs.
pub struct CrewDeps {
    /// Slot resolution and providers.
    pub brains: Arc<Brains>,
    /// Builds each run's inner tool host.
    pub tools: CrewToolFactory,
    pub approvals: Arc<dyn RunApprovals>,
    /// Finished research runs write a Result memory; `None` skips it. Deep research v2 also
    /// recalls from it and reads the user's profile when it plans.
    pub mind: Option<Mind>,
    pub gpu_busy: GpuBusy,
    /// Research channels and the corpus fetcher. With them deep research runs v2; without them,
    /// v1 (scouts search with their own tools).
    pub research: Option<Arc<dyn ResearchIo>>,
}

/// What the supervisor tells the daemon. The daemon maps these onto the bus.
#[derive(Debug, Clone, PartialEq)]
pub enum CrewEvent {
    /// Created, or status/step/spend/action changed.
    Updated(CrewRunInfo),
    /// Streaming output of the current step.
    Step { run: RunId, step: u32, action: String, text_delta: Option<String> },
    /// Done with a result. The daemon posts it to `session`'s inbox and, if the session is idle,
    /// runs a short relay turn.
    Finished { run: RunId, session: Option<SessionId>, dispatch: Dispatch },
    /// A deep research run drafted its plan and waits for [`Supervisor::approve_plan`].
    PlanProposed { run: RunId, session: Option<SessionId>, plan: ResearchPlan, quote: CostQuote },
    /// A deep research run's stage moved.
    Research { run: RunId, progress: ResearchProgress },
}

/// Handle to the supervisor. Clones share one.
#[derive(Clone)]
pub struct Supervisor {
    inner: Arc<Inner>,
}

/// One run's live controls. Cancel is final; pause and stop end the run thread at its next step
/// boundary (stop = shutdown, the store keeps the run `running` so the next start resumes it).
/// `user_cancel` marks a cancel the user asked for, which outranks pause and stop when the run
/// thread records how it ended (shutdown also fires `cancel` to interrupt model calls).
pub(crate) struct Control {
    pub cancel: CancelToken,
    pub user_cancel: AtomicBool,
    pub pause: AtomicBool,
    pub stop: AtomicBool,
    /// The note a cancelled run ends with (a rejected plan says so).
    pub end_note: Mutex<Option<String>>,
}

/// A research plan waiting on the user.
pub(crate) struct PendingPlan {
    pub plan: ResearchPlan,
    pub quote: CostQuote,
    pub tx: Sender<PlanDecision>,
}

/// A run this process is responsible for: queued, running, paused or waiting on the user.
pub(crate) struct Live {
    pub run: Run,
    pub ctl: Arc<Control>,
    pub steer_tx: Sender<Steer>,
    pub steer_rx: Receiver<Steer>,
    /// A run thread owns it.
    pub running: bool,
}

#[derive(Default)]
pub(crate) struct State {
    pub live: HashMap<RunId, Live>,
    pub queue: VecDeque<RunId>,
    /// Runs in flight per provider (`ProviderKind::as_str`).
    pub active: HashMap<&'static str, usize>,
    pub threads: usize,
    pub stopping: bool,
}

pub(crate) struct Inner {
    pub home: PathBuf,
    pub cfg: CrewConfig,
    pub deps: CrewDeps,
    pub roles: Mutex<Roles>,
    pub sink: Sender<CrewEvent>,
    pub store: Mutex<RunStore>,
    pub state: Mutex<State>,
    /// Local model streams in flight across every run, at most `cfg.per_local_provider`.
    pub local_streams: Mutex<usize>,
    pub local_free: Condvar,
    /// Research plans waiting on the user, by run.
    pub plans: Mutex<HashMap<RunId, PendingPlan>>,
    wake: Sender<()>,
}

/// The model a card resolves to now, and whether it must wait for the GPU.
pub(crate) struct Choice {
    pub model: StepModel,
    pub waits_for_gpu: bool,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Inner {
    pub(crate) fn state(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }

    /// A deep research run whose plan was proposed and never approved: it stopped at the plan
    /// gate, and its next start proposes the checkpointed plan again.
    fn waits_on_plan(&self, run: &Run) -> bool {
        use crate::playbook::research_v2::checkpoint_key;
        use marlowe_proto::ResearchStage;
        if run.playbook != Some(crate::Playbook::DeepResearch) {
            return false;
        }
        let Ok(steps) = self.store().steps(&run.id) else { return false };
        let has = |part: &str| {
            let key = checkpoint_key(ResearchStage::Plan, Some(part));
            steps.iter().any(|s| s.key == key)
        };
        has("proposed") && !has("approved")
    }

    pub(crate) fn store(&self) -> MutexGuard<'_, RunStore> {
        lock(&self.store)
    }

    pub(crate) fn emit(&self, ev: CrewEvent) {
        let _ = self.sink.send(ev);
    }

    pub(crate) fn wake(&self) {
        let _ = self.wake.send(());
    }

    /// Changes a live run under `st`, persists it, and returns the event to send once `st` is
    /// released. `None` when the run is not live.
    pub(crate) fn touch(&self, st: &mut State, id: &RunId, f: impl FnOnce(&mut Run)) -> Option<CrewEvent> {
        let live = st.live.get_mut(id)?;
        f(&mut live.run);
        live.run.updated_ms = crate::now_ms();
        let _ = self.store().save(&live.run);
        Some(CrewEvent::Updated(live.run.info()))
    }

    /// [`Inner::touch`] with its own lock; returns the run after the change.
    pub(crate) fn update(&self, id: &RunId, f: impl FnOnce(&mut Run)) -> Option<Run> {
        let (ev, run) = {
            let mut st = self.state();
            let ev = self.touch(&mut st, id, f);
            (ev, st.live.get(id).map(|l| l.run.clone()))
        };
        if let Some(ev) = ev {
            self.emit(ev);
        }
        run
    }

    /// The live copy, else the stored one.
    pub(crate) fn run(&self, id: &RunId) -> Result<Run, CrewError> {
        if let Some(l) = self.state().live.get(id) {
            return Ok(l.run.clone());
        }
        self.store().get(id)?.ok_or_else(|| CrewError::UnknownRun(id.clone()))
    }

    /// Persists a run this process is not running (merge, discard, boot fixes) and announces it.
    fn save_detached(&self, mut run: Run) -> Result<Run, CrewError> {
        run.updated_ms = crate::now_ms();
        self.store().save(&run)?;
        self.emit(CrewEvent::Updated(run.info()));
        Ok(run)
    }

    /// The model for `card`: the first explicitly assigned slot of its chain, else the
    /// conductor's model. A local model while the GPU is busy moves to the first slot assigned a
    /// cloud model, or reports that it must wait. A local model that is not loaded loads only when
    /// the planner says it fits beside what is ([`Priority::Crew`]); otherwise the step is refused
    /// with the cost. The agent's window is the context gate's (`allowed_ctx`), never above it.
    pub(crate) fn choose(&self, card: &RoleCard) -> Result<Choice, CrewError> {
        let brains = &self.deps.brains;
        let slots = brains.slots().unwrap_or_default();
        let explicit = card
            .slot_chain()
            .into_iter()
            .find_map(|slot| slots.iter().find(|a| a.slot == slot && !a.inherited).and_then(|a| a.model.clone()));
        let mut model: ModelRef = match explicit {
            Some(m) => m,
            None => brains.resolve(&card.slot)?.0,
        };
        let local = brains.model_info(&model)?.local;
        let mut waits_for_gpu = false;
        if (local || model.provider.is_local()) && (self.deps.gpu_busy)() {
            let cloud = slots
                .iter()
                .filter_map(|a| a.model.clone())
                .find(|m| !m.provider.is_local() && m.provider.is_available_now());
            match cloud {
                Some(m) => model = m,
                None => waits_for_gpu = true,
            }
        }
        if !waits_for_gpu {
            if let Admission::Refused(why) = brains.admit(&model, Priority::Crew) {
                return Err(marlowe_brains::BrainsError::Invalid(format!("not loading {model}: {why}")).into());
            }
        }
        let (provider, info, _) = brains.model_parts(&model, marlowe_model::Effort::Medium)?;
        Ok(Choice { model: StepModel { provider, model: info }, waits_for_gpu })
    }

    /// Starts every queued run that has a provider slot free.
    fn schedule(self: &Arc<Self>) {
        let mut events = Vec::new();
        {
            let mut st = self.state();
            if st.stopping {
                return;
            }
            let roles = lock(&self.roles).clone();
            let queued: Vec<RunId> = st.queue.iter().cloned().collect();
            for id in queued {
                let Some(live) = st.live.get(&id) else {
                    st.queue.retain(|q| *q != id);
                    continue;
                };
                if live.run.status != CrewRunStatus::Queued || live.running {
                    st.queue.retain(|q| *q != id);
                    continue;
                }
                let choice = roles
                    .get(&live.run.role)
                    .ok_or_else(|| CrewError::UnknownRole(live.run.role.clone()))
                    .and_then(|card| self.choose(card));
                let choice = match choice {
                    Ok(c) => c,
                    Err(e) => {
                        st.queue.retain(|q| *q != id);
                        events.extend(self.touch(&mut st, &id, |r| {
                            r.status = CrewRunStatus::Failed;
                            r.note = Some(format!("no model for this run: {e}"));
                            r.current_action = None;
                            r.relay_due = r.session.is_some();
                        }));
                        st.live.remove(&id);
                        continue;
                    }
                };
                if choice.waits_for_gpu {
                    let waiting = "waiting for the GPU";
                    if live.run.current_action.as_deref() != Some(waiting) {
                        events.extend(self.touch(&mut st, &id, |r| r.current_action = Some(waiting.into())));
                    }
                    continue;
                }
                let kind = choice.model.model.provider;
                let local = choice.model.model.local || kind.is_local();
                // A local model runs no more agents at once than its policy's slots.
                let limit = if local {
                    let slots = self.deps.brains.context().slots(&choice.model.model.model_ref()).unwrap_or(1);
                    self.cfg.per_local_provider.min(usize::from(slots))
                } else {
                    self.cfg.per_provider
                }
                .max(1);
                let busy = st.active.get(kind.as_str()).copied().unwrap_or(0);
                if busy >= limit {
                    continue;
                }
                *st.active.entry(kind.as_str()).or_default() += 1;
                st.threads += 1;
                st.queue.retain(|q| *q != id);
                if let Some(l) = st.live.get_mut(&id) {
                    l.running = true;
                }
                events.extend(self.touch(&mut st, &id, |r| {
                    r.status = CrewRunStatus::Running;
                    r.current_action = Some("starting".into());
                    r.note = None;
                }));
                let inner = Arc::clone(self);
                let run_id = id.clone();
                let spawned = std::thread::Builder::new()
                    .name(format!("crew-{}", id.short()))
                    .spawn(move || crate::runner::run_thread(inner, run_id, kind.as_str()));
                if let Err(e) = spawned {
                    *st.active.entry(kind.as_str()).or_default() -= 1;
                    st.threads -= 1;
                    events.extend(self.touch(&mut st, &id, |r| {
                        r.status = CrewRunStatus::Failed;
                        r.note = Some(format!("could not start the run thread: {e}"));
                        r.relay_due = r.session.is_some();
                    }));
                    st.live.remove(&id);
                }
            }
        }
        for ev in events {
            self.emit(ev);
        }
    }

    /// Adds a run to the live table and the queue.
    fn enqueue(&self, st: &mut State, run: Run) {
        let id = run.id.clone();
        let ctl = match st.live.remove(&id) {
            Some(old) => (old.ctl, old.steer_tx, old.steer_rx),
            None => {
                let (tx, rx) = crossbeam_channel::unbounded();
                (Arc::new(Control::new()), tx, rx)
            }
        };
        st.live.insert(id.clone(), Live { run, ctl: ctl.0, steer_tx: ctl.1, steer_rx: ctl.2, running: false });
        if !st.queue.contains(&id) {
            st.queue.push_back(id);
        }
    }
}

impl Control {
    fn new() -> Control {
        Control {
            cancel: CancelToken::new(),
            user_cancel: AtomicBool::new(false),
            pause: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            end_note: Mutex::new(None),
        }
    }
}

/// The scheduler: wakes on dispatch, on run end and every second (GPU-busy waits).
fn scheduler(inner: Weak<Inner>, wake: Receiver<()>) {
    loop {
        match wake.recv_timeout(Duration::from_secs(1)) {
            Ok(()) | Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
        }
        while wake.try_recv().is_ok() {}
        let Some(inner) = inner.upgrade() else { return };
        if inner.state().stopping {
            return;
        }
        inner.schedule();
    }
}

impl Supervisor {
    /// Opens the run tables in `<home>\cowork.db`, loads role cards, resumes interrupted runs,
    /// and starts the scheduler thread.
    pub fn start(home: &Path, deps: CrewDeps, cfg: CrewConfig, sink: Sender<CrewEvent>) -> Result<Supervisor, CrewError> {
        let store = RunStore::open(&home.join("cowork.db"))?;
        let (wake_tx, wake_rx) = crossbeam_channel::unbounded();
        let inner = Arc::new(Inner {
            home: home.to_path_buf(),
            cfg,
            deps,
            roles: Mutex::new(Roles::load(home)),
            sink,
            store: Mutex::new(store),
            state: Mutex::new(State::default()),
            local_streams: Mutex::new(0),
            local_free: Condvar::new(),
            plans: Mutex::new(HashMap::new()),
            wake: wake_tx,
        });

        // Runs a previous daemon left mid-flight (a crash: a clean stop leaves them running). One
        // waiting on an approval lost it: it pauses, and resuming asks again. One waiting on its
        // research plan is queued instead: the proposal is checkpointed, so asking again costs no
        // model call, and the plan is back in front of the user without a resume.
        let mut resumable = inner.store().resumable()?;
        let stranded: Vec<Run> = inner
            .store()
            .list(u32::MAX)?
            .into_iter()
            .filter(|r| r.status == CrewRunStatus::NeedsYou && r.result.is_none())
            .collect();
        for mut run in stranded {
            if inner.waits_on_plan(&run) {
                run.note = Some("the daemon restarted; asking for the research plan again".into());
                run.status = CrewRunStatus::Queued;
                resumable.push(inner.save_detached(run)?);
                continue;
            }
            run.status = CrewRunStatus::Paused;
            run.note = Some("the daemon restarted while this run waited on you; resume it to ask again".into());
            inner.save_detached(run)?;
        }
        let mut events = Vec::new();
        {
            let mut st = inner.state();
            for mut run in resumable {
                if run.status == CrewRunStatus::Running {
                    run.note = Some("resuming after a restart".into());
                }
                run.status = CrewRunStatus::Queued;
                run.current_action = None;
                let id = run.id.clone();
                inner.enqueue(&mut st, run);
                events.extend(inner.touch(&mut st, &id, |_| {}));
            }
        }
        for ev in events {
            inner.emit(ev);
        }

        let weak = Arc::downgrade(&inner);
        std::thread::Builder::new().name("crew-scheduler".into()).spawn(move || scheduler(weak, wake_rx))?;
        inner.wake();
        Ok(Supervisor { inner })
    }

    /// Queues a run under `session` and returns at once. `args.workspace` must already be
    /// resolved (argument, else the session's workspace).
    pub fn dispatch(&self, session: Option<SessionId>, args: DispatchArgs) -> Result<DispatchReceipt, CrewError> {
        let card = lock(&self.inner.roles).get(&args.role).cloned().ok_or_else(|| CrewError::UnknownRole(args.role.clone()))?;
        let budget = args.budget.unwrap_or_default();
        let workspace = args.workspace.as_deref().map(str::trim).filter(|w| !w.is_empty()).map(PathBuf::from);
        let mut run = Run::new(session, &card, &args.goal, args.playbook, workspace, budget);
        run.tainted = args.tainted;
        let pricing = self.inner.choose(&card).ok().and_then(|c| c.model.model.pricing);
        let receipt = DispatchReceipt {
            run_id: run.id.clone(),
            role: card.name.clone(),
            playbook: args.playbook,
            estimate: Estimate::for_run(&card, args.playbook, &budget, pricing.as_ref()),
        };
        self.inner.store().save(&run)?;
        let ev = CrewEvent::Updated(run.info());
        {
            let mut st = self.inner.state();
            if st.stopping {
                return Err(CrewError::Stopped);
            }
            self.inner.enqueue(&mut st, run);
        }
        self.inner.emit(ev);
        self.inner.wake();
        Ok(receipt)
    }

    /// The role cards in force.
    /// Runs with a live thread now (running, or waiting on the user mid-run).
    pub fn running(&self) -> usize {
        self.inner.state().live.values().filter(|l| l.running).count()
    }

    pub fn roles(&self) -> Vec<RoleInfo> {
        lock(&self.inner.roles).infos()
    }

    /// Re-reads `<home>\roles`; runs already started keep the card they started with.
    pub fn reload_roles(&self) -> Vec<RoleInfo> {
        let fresh = Roles::load(&self.inner.home);
        let mut roles = lock(&self.inner.roles);
        *roles = fresh;
        roles.infos()
    }

    /// Newest first.
    pub fn list(&self, limit: u32) -> Result<Vec<CrewRunInfo>, CrewError> {
        Ok(self.inner.store().list(limit)?.iter().map(Run::info).collect())
    }

    pub fn get(&self, run: &RunId) -> Result<Option<CrewRunInfo>, CrewError> {
        match self.inner.run(run) {
            Ok(r) => Ok(Some(r.info())),
            Err(CrewError::UnknownRun(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// The run and its stored steps, for the Run view.
    pub fn transcript(&self, run: &RunId) -> Result<(CrewRunInfo, Vec<CrewStep>), CrewError> {
        let info = self.inner.run(run)?.info();
        let steps = self.inner.store().steps(run)?.iter().map(|s| s.view()).collect();
        Ok((info, steps))
    }

    /// Appends guidance to the run's inbox; picked up at its next step.
    pub fn steer(&self, run: &RunId, text: &str) -> Result<(), CrewError> {
        let st = self.inner.state();
        let Some(live) = st.live.get(run) else {
            drop(st);
            let status = self.inner.run(run)?.status;
            return Err(bad_state(run, status, "it is not active, so there is nothing to steer"));
        };
        let steer = Steer::admit(text).map_err(|e| bad_state(run, live.run.status, &e.to_string()))?;
        live.steer_tx.send(steer).map_err(|_| CrewError::Stopped)
    }

    /// Cancels the run's token; the run ends `cancelled` and keeps its sandbox until discarded.
    pub fn cancel(&self, run: &RunId) -> Result<(), CrewError> {
        let ev = {
            let mut st = self.inner.state();
            match st.live.get(run) {
                Some(live) if live.running => {
                    live.ctl.user_cancel.store(true, Ordering::SeqCst);
                    live.ctl.cancel.cancel();
                    None
                }
                Some(_) => {
                    st.queue.retain(|q| q != run);
                    let ev = self.inner.touch(&mut st, run, |r| {
                        r.status = CrewRunStatus::Cancelled;
                        r.current_action = None;
                    });
                    st.live.remove(run);
                    ev
                }
                None => {
                    drop(st);
                    let r = self.inner.run(run)?;
                    if r.status.is_terminal() || r.result.is_some() {
                        return Err(bad_state(run, r.status, "it already ended; discard it to remove its sandbox"));
                    }
                    let mut r = r;
                    r.status = CrewRunStatus::Cancelled;
                    self.inner.save_detached(r)?;
                    None
                }
            }
        };
        if let Some(ev) = ev {
            self.inner.emit(ev);
        }
        Ok(())
    }

    /// Records that the run's result (or failure) reached its parent session's history, so a
    /// restart does not relay it again.
    pub fn mark_relayed(&self, run: &RunId) -> Result<(), CrewError> {
        {
            let mut st = self.inner.state();
            if let Some(live) = st.live.get_mut(run) {
                live.run.relay_due = false;
                return self.inner.store().save(&live.run);
            }
        }
        let store = self.inner.store();
        match store.get(run)? {
            Some(mut r) if r.relay_due => {
                r.relay_due = false;
                store.save(&r)
            }
            _ => Ok(()),
        }
    }

    /// Runs whose result (or failure) has not reached their parent session yet, oldest first.
    pub fn relays_due(&self) -> Result<Vec<CrewRunInfo>, CrewError> {
        let mut due: Vec<Run> = self.inner.store().list(u32::MAX)?.into_iter().filter(|r| r.relay_due).collect();
        due.sort_by_key(|r| r.updated_ms);
        Ok(due.iter().map(Run::info).collect())
    }

    /// Answers a research run's proposed plan. An edit must validate (the run keeps waiting when
    /// it does not); a rejection ends the run cancelled.
    pub fn approve_plan(&self, run: &RunId, decision: PlanDecision) -> Result<(), CrewError> {
        let mut plans = lock(&self.inner.plans);
        let Some(pending) = plans.get(run) else {
            drop(plans);
            let status = self.inner.run(run)?.status;
            return Err(bad_state(run, status, "it has no plan awaiting approval"));
        };
        if let PlanDecision::Edit { .. } = &decision {
            if let Err(e) = apply_decision(pending.plan.clone(), decision.clone()) {
                return Err(bad_state(run, CrewRunStatus::NeedsYou, &format!("the edited plan is not runnable: {e}")));
            }
        }
        if let PlanDecision::Reject { reason } = &decision {
            if let Some(live) = self.inner.state().live.get(run) {
                let note = match reason.as_deref().map(str::trim).filter(|r| !r.is_empty()) {
                    Some(r) => format!("plan rejected: {r}"),
                    None => "plan rejected".to_string(),
                };
                *lock(&live.ctl.end_note) = Some(note);
                live.ctl.user_cancel.store(true, Ordering::SeqCst);
            }
        }
        let pending = plans.remove(run).ok_or_else(|| bad_state(run, CrewRunStatus::NeedsYou, "the plan was already answered"))?;
        pending.tx.send(decision).map_err(|_| CrewError::Stopped)
    }

    /// The plan a research run is waiting on, with its quote.
    pub fn pending_plan(&self, run: &RunId) -> Option<(ResearchPlan, CostQuote)> {
        lock(&self.inner.plans).get(run).map(|p| (p.plan.clone(), p.quote.clone()))
    }

    /// Stops the run at its next step boundary, freeing its concurrency slot.
    pub fn pause(&self, run: &RunId) -> Result<(), CrewError> {
        let ev = {
            let mut st = self.inner.state();
            let Some(live) = st.live.get(run) else {
                drop(st);
                let status = self.inner.run(run)?.status;
                return Err(bad_state(run, status, "only a queued or running run can pause"));
            };
            if live.running {
                live.ctl.pause.store(true, Ordering::SeqCst);
                None
            } else if live.run.status == CrewRunStatus::Queued {
                st.queue.retain(|q| q != run);
                self.inner.touch(&mut st, run, |r| {
                    r.status = CrewRunStatus::Paused;
                    r.current_action = None;
                })
            } else {
                return Err(bad_state(run, live.run.status, "only a queued or running run can pause"));
            }
        };
        if let Some(ev) = ev {
            self.inner.emit(ev);
        }
        Ok(())
    }

    /// Requeues a paused run; it continues from its last completed step.
    pub fn resume(&self, run: &RunId) -> Result<(), CrewError> {
        let stored = self.inner.run(run)?;
        let ev = {
            let mut st = self.inner.state();
            if st.live.get(run).is_some_and(|l| l.running) || stored.status != CrewRunStatus::Paused {
                return Err(bad_state(run, stored.status, "only a paused run can resume"));
            }
            if let Some(live) = st.live.get(run) {
                live.ctl.pause.store(false, Ordering::SeqCst);
            }
            self.inner.enqueue(&mut st, stored);
            self.inner.touch(&mut st, run, |r| {
                r.status = CrewRunStatus::Queued;
                r.note = None;
            })
        };
        if let Some(ev) = ev {
            self.inner.emit(ev);
        }
        self.inner.wake();
        Ok(())
    }

    /// Call only after the user approved. Build: merges `crew/<run-id>` into the origin's current
    /// branch and removes the worktree. Research: copies `report.md` to
    /// `<workspace>\research\<slug>.md`. Never pushes.
    pub fn merge(&self, run: &RunId) -> Result<MergeOutcome, CrewError> {
        let mut r = self.idle_run(run)?;
        if r.result.is_none() {
            return Err(bad_state(run, r.status, "it has no result to merge yet"));
        }
        let sandbox = r.sandbox.clone().ok_or_else(|| bad_state(run, r.status, "it has no sandbox"))?;
        let outcome = if sandbox.kind == SandboxKind::Worktree {
            let outcome = sandbox.merge()?;
            sandbox.discard()?;
            outcome
        } else {
            let report = sandbox.root.join(REPORT_FILE);
            if !report.is_file() {
                return Err(bad_state(run, r.status, "it wrote no report and has no worktree"));
            }
            let ws = r.workspace.clone().ok_or_else(|| bad_state(run, r.status, "it has no workspace to save into"))?;
            let dir = ws.join("research");
            std::fs::create_dir_all(&dir)?;
            let stem = slug(&r.title);
            let mut dest = dir.join(format!("{stem}.md"));
            let mut n = 2;
            while dest.exists() {
                dest = dir.join(format!("{stem}-{n}.md"));
                n += 1;
            }
            std::fs::copy(&report, &dest)?;
            MergeOutcome::ReportCopied { path: dest }
        };
        r.status = CrewRunStatus::Done;
        r.note = Some(match &outcome {
            MergeOutcome::FastForward { head } => format!("merged: fast-forward to {}", short_sha(head)),
            MergeOutcome::MergeCommit { commit } => format!("merged: merge commit {}", short_sha(commit)),
            MergeOutcome::NothingToMerge => "nothing to merge; the worktree is removed".into(),
            MergeOutcome::ReportCopied { path } => format!("report saved to {}", path.display()),
        });
        self.inner.save_detached(r)?;
        Ok(outcome)
    }

    /// Removes the run's sandbox (worktree and branch, or scratch dir) without merging.
    pub fn discard(&self, run: &RunId) -> Result<(), CrewError> {
        let mut r = self.idle_run(run)?;
        if let Some(sb) = &r.sandbox {
            sb.discard()?;
        }
        {
            let mut st = self.inner.state();
            st.queue.retain(|q| q != run);
            st.live.remove(run);
        }
        if r.result.is_some() {
            r.status = CrewRunStatus::Done;
        } else if !r.status.is_terminal() {
            r.status = CrewRunStatus::Cancelled;
        }
        r.note = Some("discarded".into());
        r.current_action = None;
        self.inner.save_detached(r)?;
        Ok(())
    }

    /// Stops every run thread at its next step boundary (in-flight model calls are interrupted)
    /// and waits up to ten seconds for them. Runs stay `running` in the store so the next start
    /// resumes them.
    pub fn shutdown(&self) {
        {
            let mut st = self.inner.state();
            st.stopping = true;
            for live in st.live.values().filter(|l| l.running) {
                live.ctl.stop.store(true, Ordering::SeqCst);
                live.ctl.cancel.cancel();
            }
        }
        self.inner.wake();
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.inner.state().threads > 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// A run no thread is working on, for merge and discard.
    fn idle_run(&self, run: &RunId) -> Result<Run, CrewError> {
        let r = self.inner.run(run)?;
        if self.inner.state().live.get(run).is_some_and(|l| l.running) {
            return Err(bad_state(run, r.status, "it is still running; cancel or wait for it first"));
        }
        Ok(r)
    }
}

fn bad_state(run: &RunId, status: CrewRunStatus, why: &str) -> CrewError {
    CrewError::BadState { run: run.clone(), status: status.as_str(), why: why.to_string() }
}

fn short_sha(sha: &str) -> &str {
    &sha[..sha.len().min(10)]
}

/// Raises the exceeded dimension of `budget` by `fraction` of itself (at least one unit).
pub(crate) fn extend_budget(budget: &mut CrewBudget, dim: &str, fraction: f64) {
    let f = fraction.max(0.1);
    match dim {
        "steps" => budget.steps = budget.steps.map(|s| s + ((s as f64 * f).ceil() as u32).max(1)),
        "tokens" => budget.tokens = budget.tokens.map(|t| t + ((t as f64 * f).ceil() as u64).max(1)),
        "usd" => budget.usd = budget.usd.map(|u| ((u + (u * f).max(0.01)) * 100.0).round() / 100.0),
        _ => {}
    }
}

/// The Result memory a finished run writes: the summary and its citations, scoped to the run's
/// workspace, with provenance = the run (`Agent { session: "crew:<run-id>", tainted }`), so a run
/// that read the web is remembered at untrusted trust.
pub fn result_memory(run: &Run, dispatch: &Dispatch) -> MemoryIn {
    let mut text = format!("Crew {} run \"{}\" ({}): {}", run.role, run.title, run.id.short(), clip(&dispatch.summary, 1_200));
    if !dispatch.citations.is_empty() {
        let sources: Vec<String> =
            dispatch.citations.iter().take(8).map(|c| format!("[{}]({})", c.title, c.url_or_path)).collect();
        text.push_str(&format!("\nSources: {}", sources.join("; ")));
    }
    let confidence = match dispatch.confidence {
        Confidence::Low => "low",
        Confidence::Medium => "medium",
        Confidence::High => "high",
    };
    text.push_str(&format!("\nConfidence: {confidence}."));
    MemoryIn {
        text,
        kind: Kind::Result,
        scope: match &run.workspace {
            Some(ws) => MemoryScope::Project(ws.display().to_string()),
            None => MemoryScope::Global,
        },
        source: Provenance::Agent { session: format!("crew:{}", run.id), turn: run.step, tainted: run.tainted },
    }
}
