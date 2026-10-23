//! The coordinator: the scheduler that runs a project's phases.
//!
//! One thread per running project owns the [`Engine::run`] loop. It never calls a model and
//! never blocks on one: tasks run on worker threads, each on its own lane (`model`, `fetch`,
//! `search`, `cpu`, `io`), and report back over a channel. The loop is *level-triggered*: every
//! round it reads a light index of the state, works out which input items are ready for which
//! step, queues the tasks it has not queued before, replays any that the journal already holds,
//! and launches the best queued task of each lane with room (later phases and later steps first,
//! so started work finishes; ties go round-robin across questions). Because a round starts from
//! the state and the journal and not from remembered events, a fresh start, a resume after a
//! crash and the step after a user's answer are the same code path.
//!
//! Phases are data ([`crate::spec`]). A phase is *active* once everything before it has finished,
//! or sooner when its steps stream from a step in a phase that is still running (DataValidation
//! starts extracting the moment the first source is analysed). A step is *closed* when its
//! upstream steps are closed and nothing of it is queued or running; a phase is done when its
//! steps are closed.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{unbounded, Receiver, RecvTimeoutError, Sender};
use marlowe_model::{CancelToken, Provider};
use serde_json::json;

use super::exec::{run_task, MergeTask, Shared, Task};
use super::functions::{Effect, GateCard, GateCtx, Toolbox};
use super::gate::ModelGate;
use super::index::{Index, ItemInfo, PhaseInfo};
use super::record::{answer_key, merge_key, task_key, Outcome, Record};
use super::runner::RunnerConfig;
use crate::actor::{ActorError, ProjectHandle};
use crate::mutation::{ItemUpdate, Mutation, MutationError, PhaseUpdate};
use crate::registry::Registry;
use crate::spec::{Edge, Lane, PhaseSpec, Scope, StepSpec};
use crate::state::{
    AgentStatus, Counts, Gate, GateAnswer, GateKind, GateStatus, ItemStatus, Metrics, NetGrant, Progress, ProjectStatus, StageStatus, StepKind,
    StepState,
};
use crate::store::{ProjectStore, StoreError};

/// What the budgets are. Reaching one stops new model calls and raises a `budget` checkin; the
/// user's "continue" adds one more quote each time.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Budgets {
    /// Most tokens (prompt plus output) across every employee.
    pub tokens: Option<u64>,
    /// Most minutes of running time, parked time excluded.
    pub minutes: Option<u64>,
}

/// How wide each non-model lane is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lanes {
    /// Page fetches at once.
    pub fetch: usize,
    /// Searches at once.
    pub search: usize,
    /// Pure computations at once.
    pub cpu: usize,
    /// Disk steps at once.
    pub io: usize,
}

impl Default for Lanes {
    fn default() -> Self {
        Lanes { fetch: 8, search: 4, cpu: 2, io: 2 }
    }
}

/// Knobs of the engine.
#[derive(Clone)]
pub struct EngineConfig {
    /// The model of `employee` steps.
    pub employee_model: String,
    /// The model of `lead` steps (the same by default: one model, one cache).
    pub lead_model: String,
    /// Lane widths.
    pub lanes: Lanes,
    /// More than this share of an important step's items failing raises a `failures` checkin.
    pub failure_share: f64,
    /// Token and minute budgets.
    pub budget: Budgets,
    /// The employee runner.
    pub runner: RunnerConfig,
    /// The most tokens the window leaves for one reply.
    pub window_room: Option<u32>,
    /// How often the loop looks up when nothing happens (cancel, budgets, forced checkins).
    pub poll: Duration,
    /// The clock; tests pass a fixed one.
    pub clock: fn() -> u64,
}

impl EngineConfig {
    /// A config with `model` for employees and the lead.
    pub fn new(model: &str) -> EngineConfig {
        EngineConfig {
            employee_model: model.into(),
            lead_model: model.into(),
            lanes: Lanes::default(),
            failure_share: 0.25,
            budget: Budgets::default(),
            runner: RunnerConfig::default(),
            window_room: None,
            poll: Duration::from_millis(25),
            clock: crate::ids::now_ms,
        }
    }
}

/// How a call to [`Engine::run`] ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunOutcome {
    /// Every phase is done; the final snapshot is written.
    Finished,
    /// A gate is open and nothing is running: no thread and no slot is held. Answer it with
    /// [`Engine::answer`] and call [`Engine::run`] again.
    Parked {
        /// The gate id.
        gate: String,
    },
    /// Cancelled: nothing is running, unfinished items are `cancelled`, the state is saved.
    Cancelled,
    /// Stopped for shutdown by [`Control::suspend`]: nothing is running, the state is saved as it
    /// stands (the status is still `running`), and the next start resumes from the journal.
    Suspended,
    /// The engine could not go on; the reason is harness-written.
    Failed {
        /// Why.
        reason: String,
    },
}

enum Msg {
    Done(Box<Done>),
    Wake,
}

struct Done {
    id: u64,
    rec: Record,
    cancelled: bool,
}

/// What a caller can do to a running engine from another thread.
#[derive(Clone)]
pub struct Control {
    tx: Sender<Msg>,
    cancel: CancelToken,
    checkin: Arc<AtomicBool>,
    suspend: Arc<AtomicBool>,
}

impl Control {
    /// Cancels the project: in-flight calls stop, unfinished items become `cancelled`.
    pub fn cancel(&self) {
        self.cancel.cancel();
        let _ = self.tx.send(Msg::Wake);
    }

    /// Stops the run for shutdown without cancelling the project: in-flight calls stop, their
    /// results are dropped, and the project stays `running` on disk so the next start resumes it.
    pub fn suspend(&self) {
        self.suspend.store(true, Ordering::SeqCst);
        self.cancel();
    }

    /// Asks for a checkin at the next safe point: nothing new starts, what is in flight finishes
    /// (none is killed), then the project parks on a checkin card.
    pub fn request_checkin(&self) {
        self.checkin.store(true, Ordering::SeqCst);
        let _ = self.tx.send(Msg::Wake);
    }

    /// True once cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }
}

/// Why the engine is stopping launches and heading for a gate.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Park {
    /// A gate step of a phase.
    Step { phase: String, step: String },
    /// A gate raised by the engine: `user_forced`, `budget`, `failures`.
    Extra { reason: String, phase: Option<String> },
    /// A gate that is open already.
    Open(String),
}

#[derive(Default)]
struct MergeRt {
    fired: usize,
    consumed: HashSet<String>,
    inflight: HashSet<String>,
    passes: u32,
}

#[derive(Default)]
struct StepRt {
    launched: HashSet<String>,
    counts: HashMap<String, Counts>,
    merge: HashMap<String, MergeRt>,
    closed: bool,
    running: u32,
}

impl StepRt {
    fn count(&mut self, sub: &str) -> &mut Counts {
        self.counts.entry(sub.to_string()).or_default()
    }

    fn busy(&self, sub: Option<&str>) -> bool {
        match sub {
            Some(s) => {
                let c = |k: &str| self.counts.get(k).map_or(0, |c| c.queued + c.running);
                c(s) + if s.is_empty() { 0 } else { c("") } > 0 || self.merge.get(s).is_some_and(|m| !m.inflight.is_empty())
            }
            None => self.counts.values().any(|c| c.queued + c.running > 0) || self.merge.values().any(|m| !m.inflight.is_empty()),
        }
    }

    fn total(&self) -> Counts {
        self.counts.values().fold(Counts::default(), |mut a, c| {
            a.queued += c.queued;
            a.running += c.running;
            a.done += c.done;
            a.failed += c.failed;
            a
        })
    }
}

struct Rt {
    by_key: HashMap<String, Record>,
    by_step: HashMap<(String, String), Vec<Record>>,
    steps: HashMap<(String, String), StepRt>,
    inited: HashSet<String>,
    queue: Vec<Task>,
    running: HashMap<u64, Task>,
    lane_running: HashMap<String, usize>,
    served: HashMap<(String, String), u64>,
    seq: u64,
    park: Option<Park>,
    metrics: Metrics,
    metrics_dirty: bool,
    /// The state changed since the index was read: a step must not close on the stale index.
    touched: bool,
    began: Instant,
    base_active_ms: u64,
}

/// A handle to an engine running on its own thread.
pub struct RunHandle {
    join: Option<JoinHandle<RunOutcome>>,
    /// Controls the engine.
    pub control: Control,
}

impl RunHandle {
    /// True once the run has ended (finished, parked, cancelled or failed).
    pub fn is_finished(&self) -> bool {
        self.join.as_ref().map_or(true, JoinHandle::is_finished)
    }

    /// Waits for the run and returns how it ended.
    pub fn join(mut self) -> RunOutcome {
        match self.join.take().map(JoinHandle::join) {
            Some(Ok(o)) => o,
            _ => RunOutcome::Failed { reason: "the project's engine thread panicked".into() },
        }
    }
}

/// The phase engine for one project.
pub struct Engine {
    handle: ProjectHandle,
    store: ProjectStore,
    registry: Arc<Registry>,
    provider: Arc<dyn Provider>,
    gate: Arc<dyn ModelGate>,
    tools: Arc<Toolbox>,
    cfg: EngineConfig,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    cancel: CancelToken,
    checkin: Arc<AtomicBool>,
    suspend: Arc<AtomicBool>,
    specs: HashMap<String, Arc<StepSpec>>,
}

/// Answers the open gate `gate` of the project behind `handle` (the user's choice on the card).
/// The project's engine, parked, is then run again. Refused (and the card left open) when the
/// choice is not one of the gate's options or a required text is missing.
pub fn answer_gate(handle: &ProjectHandle, gate: &str, choice: &str, text: Option<&str>) -> Result<(), MutationError> {
    handle
        .mutate(Mutation::AnswerGate { id: gate.into(), choice: choice.into(), text: text.map(str::to_string) })
        .map(|_| ())
        .map_err(|e| match e {
            ActorError::Mutation(m) => m,
            other => MutationError::Other(other.to_string()),
        })
}

/// The part of the live numbers the engine owns, as a mutation: it must not overwrite what task
/// functions count (fetches) from their own threads.
fn run_metrics(m: &Metrics) -> Mutation {
    Mutation::SetRunMetrics { tokens: m.tokens, active_ms: m.active_ms, budget_raises: m.budget_raises }
}

fn fail<E: std::fmt::Display>(what: &str) -> impl FnOnce(E) -> String + '_ {
    move |e| format!("{what}: {e}")
}

impl Engine {
    /// An engine for the project behind `handle`. `store` is the engine's own connection to the
    /// same projects directory (the journal lives there); `registry` supplies the phase menu.
    pub fn new(
        handle: ProjectHandle,
        store: ProjectStore,
        registry: Arc<Registry>,
        provider: Arc<dyn Provider>,
        gate: Arc<dyn ModelGate>,
        tools: Arc<Toolbox>,
        cfg: EngineConfig,
    ) -> Engine {
        let (tx, rx) = unbounded();
        let specs = registry.menu().values().flat_map(|p| p.steps.iter()).map(|s| (s.id.clone(), Arc::new(s.clone()))).collect();
        Engine { handle, store, registry, provider, gate, tools, cfg, tx, rx, cancel: CancelToken::new(), checkin: Arc::new(AtomicBool::new(false)), suspend: Arc::new(AtomicBool::new(false)), specs }
    }

    /// The handle other threads use to cancel the run or ask for a checkin.
    pub fn control(&self) -> Control {
        Control { tx: self.tx.clone(), cancel: self.cancel.clone(), checkin: self.checkin.clone(), suspend: self.suspend.clone() }
    }

    /// Runs on a thread of its own and returns at once: the conversation never waits for a
    /// project.
    pub fn spawn(self) -> std::io::Result<RunHandle> {
        let control = self.control();
        let id = self.handle.id().to_string();
        let join = std::thread::Builder::new().name(format!("engine-{id}")).spawn(move || self.run())?;
        Ok(RunHandle { join: Some(join), control })
    }

    /// Answers the open gate `gate` and sets the project running again; call [`Engine::run`] (or
    /// [`Engine::spawn`]) next. A choice that is not one of the gate's options is refused.
    pub fn answer(&self, gate: &str, choice: &str, text: Option<&str>) -> Result<(), MutationError> {
        answer_gate(&self.handle, gate, choice, text)
    }

    // ---- the run -----------------------------------------------------------------------------

    /// Runs the project until it finishes, parks on a gate, is cancelled or fails. Blocks; use
    /// [`Engine::spawn`] to keep the caller free.
    pub fn run(&self) -> RunOutcome {
        match self.run_inner() {
            Ok(o) => o,
            Err(reason) => {
                let _ = self.handle.mutate(Mutation::SetStatus(ProjectStatus::Failed));
                let _ = self.handle.flush();
                RunOutcome::Failed { reason }
            }
        }
    }

    fn read(&self) -> Result<Index, String> {
        self.handle.read(|st| Index::build(st)).map_err(fail("the project state is gone"))
    }

    fn mutate(&self, m: Mutation) -> Result<(), String> {
        self.handle.mutate(m).map(|_| ()).map_err(fail("could not change the project"))
    }

    fn run_inner(&self) -> Result<RunOutcome, String> {
        let st = self.handle.snapshot().map_err(fail("the project state is gone"))?;
        match st.status {
            ProjectStatus::Finished => return Ok(RunOutcome::Finished),
            ProjectStatus::Cancelled => return Ok(RunOutcome::Cancelled),
            ProjectStatus::Failed => return Ok(RunOutcome::Failed { reason: "the project failed earlier and has not been reopened".into() }),
            _ => {}
        }
        // Every phase the project will run needs a declaration, every code step and gate a function.
        let mut menu = std::collections::BTreeMap::new();
        for p in &st.phases {
            match self.registry.phase_spec(&p.ty) {
                Some(spec) => {
                    menu.insert(spec.phase.clone(), spec.clone());
                }
                None => return Err(format!("phase `{}` has no declaration in the menu", p.ty)),
            }
        }
        let missing = self.tools.missing_for(&menu);
        if !missing.is_empty() {
            return Err(format!("the toolbox lacks: {}", missing.join("; ")));
        }

        // A run that died leaves `running` items and agents behind; nothing will finish them.
        self.recover(&st)?;
        let sh = Arc::new(Shared {
            handle: self.handle.clone(),
            provider: self.provider.clone(),
            gate: self.gate.clone(),
            tools: self.tools.clone(),
            registry: self.registry.clone(),
            cfg: self.cfg.clone(),
            cancel: self.cancel.clone(),
            agents: AtomicU32::new(st.agents.keys().filter_map(|k| k.strip_prefix("a_").and_then(|n| n.parse::<u32>().ok())).max().unwrap_or(0) + 1),
        });

        let mut rt = Rt {
            by_key: HashMap::new(),
            by_step: HashMap::new(),
            steps: HashMap::new(),
            inited: HashSet::new(),
            queue: Vec::new(),
            running: HashMap::new(),
            lane_running: HashMap::new(),
            served: HashMap::new(),
            seq: 0,
            park: None,
            metrics: st.metrics,
            metrics_dirty: false,
            touched: false,
            began: Instant::now(),
            base_active_ms: st.metrics.active_ms,
        };
        for p in &st.phases {
            if let Some(spec) = self.registry.phase_spec(&p.ty) {
                for s in &spec.steps {
                    for rec in self.read_journal(&p.id, &s.id)? {
                        rt.by_key.insert(rec.key.clone(), rec.clone());
                        rt.by_step.entry((p.id.clone(), s.id.clone())).or_default().push(rec);
                    }
                }
            }
        }
        for rec in self.read_journal("_gates", "answers")? {
            rt.by_key.insert(rec.key.clone(), rec);
        }

        if let Some(g) = st.open_gate() {
            rt.park = Some(Park::Open(g.id.clone()));
        } else if st.status != ProjectStatus::Running {
            self.mutate(Mutation::SetStatus(ProjectStatus::Running))?;
        }

        let outcome = self.drive(&sh, &mut rt);
        self.cancel_stragglers_if_needed(&mut rt);
        // Booked once, whatever the outcome: running time (parked time excluded) is what the
        // minutes budget counts.
        rt.metrics.active_ms = rt.base_active_ms + rt.began.elapsed().as_millis() as u64;
        let _ = self.handle.send(run_metrics(&rt.metrics));
        let _ = self.handle.flush();
        outcome
    }

    fn cancel_stragglers_if_needed(&self, rt: &mut Rt) {
        // Workers still out when the loop ended (a failure): stop them so none outlives the run.
        if !rt.running.is_empty() {
            self.cancel.cancel();
            let deadline = Instant::now() + Duration::from_secs(10);
            while !rt.running.is_empty() && Instant::now() < deadline {
                match self.rx.recv_timeout(Duration::from_millis(50)) {
                    Ok(Msg::Done(d)) => {
                        rt.running.remove(&d.id);
                    }
                    Ok(Msg::Wake) | Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
        }
    }

    fn read_journal(&self, phase: &str, step: &str) -> Result<Vec<Record>, String> {
        let lines = self.store.journal_read(self.handle.id(), phase, step).map_err(|e: StoreError| format!("could not read the journal: {e}"))?;
        Ok(lines.into_iter().filter_map(|v| serde_json::from_value::<Record>(v).ok()).collect())
    }

    fn journal(&self, rec: &Record) -> Result<(), String> {
        let v = serde_json::to_value(rec).map_err(|e| e.to_string())?;
        self.store.journal_append(self.handle.id(), &rec.phase, &rec.step, &v).map_err(|e| format!("could not write the journal: {e}"))
    }

    fn recover(&self, st: &crate::state::ProjectState) -> Result<(), String> {
        for it in st.items.values().filter(|i| i.status == ItemStatus::Running) {
            self.mutate(Mutation::UpdateItem { id: it.id.clone(), update: ItemUpdate { status: Some(ItemStatus::Queued), ..Default::default() } })?;
        }
        for a in st.agents.values().filter(|a| a.status == AgentStatus::Running) {
            let mut a = a.clone();
            a.status = AgentStatus::Failed;
            a.ended_ms = Some((self.cfg.clock)());
            self.mutate(Mutation::PutAgent(Box::new(a)))?;
        }
        Ok(())
    }

    fn drive(&self, sh: &Arc<Shared>, rt: &mut Rt) -> Result<RunOutcome, String> {
        let mut idle_rounds = 0u32;
        loop {
            if self.cancel.is_cancelled() {
                return if self.suspend.load(Ordering::SeqCst) { self.finish_suspend(rt) } else { self.finish_cancel(rt) };
            }
            // Results first, so a finished task's successors are queued this round.
            while let Ok(m) = self.rx.try_recv() {
                self.handle_msg(rt, m)?;
            }
            if self.checkin.swap(false, Ordering::SeqCst) && rt.park.is_none() {
                rt.park = Some(Park::Extra { reason: "user_forced".into(), phase: None });
            }
            let idx = self.read()?;
            let mut progress = self.advance(rt, &idx)?;
            if let Some(o) = self.finished_or_stuck(rt, &idx)? {
                return Ok(o);
            }
            if rt.park.is_none() {
                self.launch(sh, rt)?;
            }
            self.sync_counts(rt, &idx)?;
            if rt.metrics_dirty {
                rt.metrics_dirty = false;
                rt.metrics.active_ms = rt.base_active_ms + rt.began.elapsed().as_millis() as u64;
                let _ = self.handle.send(run_metrics(&rt.metrics));
            }
            // Parked: nothing may run; the gate is raised (or found open) once the last worker is back.
            if let Some(p) = rt.park.clone() {
                if rt.running.is_empty() {
                    if let Some(o) = self.raise_and_park(rt, &idx, &p)? {
                        return Ok(o);
                    }
                    progress = true;
                }
            }
            if progress {
                idle_rounds = 0;
                continue;
            }
            if rt.running.is_empty() && rt.queue.is_empty() && rt.park.is_none() {
                idle_rounds += 1;
                if idle_rounds > 3 {
                    return Err(self.stuck_reason(rt, &idx));
                }
            }
            match self.rx.recv_timeout(self.cfg.poll) {
                Ok(m) => self.handle_msg(rt, m)?,
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Err("the engine's channel closed".into()),
            }
        }
    }

    fn stuck_reason(&self, rt: &Rt, idx: &Index) -> String {
        let waiting: Vec<String> = idx
            .phases
            .iter()
            .filter(|p| matches!(p.status, StageStatus::Pending | StageStatus::Running))
            .map(|p| {
                let open: Vec<String> = self
                    .registry
                    .phase_spec(&p.ty)
                    .map(|s| s.steps.iter().filter(|s| !rt.steps.get(&(p.id.clone(), s.id.clone())).is_some_and(|r| r.closed)).map(|s| s.id.clone()).collect())
                    .unwrap_or_default();
                format!("{} ({:?}: {})", p.id, p.status, open.join(", "))
            })
            .collect();
        format!("the project cannot advance; waiting phases: {}", waiting.join("; "))
    }

    // ---- phases and steps --------------------------------------------------------------------

    fn spec_of(&self, ty: &str) -> Option<&PhaseSpec> {
        self.registry.phase_spec(ty)
    }

    /// The index of the nearest phase at or before `from` that declares the step `id`.
    fn resolve(&self, idx: &Index, from: usize, id: &str) -> Option<usize> {
        (0..=from.min(idx.phases.len().saturating_sub(1))).rev().find(|&i| self.spec_of(&idx.phases[i].ty).is_some_and(|s| s.step(id).is_some()))
    }

    fn closed(&self, idx: &Index, rt: &Rt, ix: usize, id: &str) -> bool {
        let Some(p) = idx.phases.get(ix) else { return true };
        match p.status {
            StageStatus::Done | StageStatus::Skipped => true,
            StageStatus::Pending | StageStatus::Failed => false,
            StageStatus::Running => rt.steps.get(&(p.id.clone(), id.to_string())).is_some_and(|s| s.closed),
        }
    }

    /// True when every `after` step of `step` is closed (or its phase is not part of this plan).
    fn upstream_closed(&self, idx: &Index, rt: &Rt, ix: usize, step: &StepSpec) -> bool {
        step.after.iter().all(|a| match self.resolve(idx, ix, &a.step) {
            Some(u) => self.closed(idx, rt, u, &a.step),
            None => true,
        })
    }

    /// True while the step, or anything upstream of it, still has work for group `sub`.
    fn group_busy(&self, idx: &Index, rt: &Rt, ix: usize, step: &StepSpec, sub: &str) -> bool {
        let Some(p) = idx.phases.get(ix) else { return false };
        match p.status {
            StageStatus::Done | StageStatus::Skipped => return false,
            StageStatus::Pending | StageStatus::Failed => return true,
            StageStatus::Running => {}
        }
        if rt.steps.get(&(p.id.clone(), step.id.clone())).is_none_or(|s| !s.closed && s.busy(Some(sub))) {
            return true;
        }
        step.after.iter().any(|a| match self.resolve(idx, ix, &a.step).and_then(|u| self.specs.get(&a.step).map(|s| (u, s))) {
            Some((u, s)) => self.group_busy(idx, rt, u, s, sub),
            None => false,
        })
    }

    /// Queued and running tasks of every step upstream of `step` for group `sub`.
    fn upstream_pending(&self, idx: &Index, rt: &Rt, ix: usize, step: &StepSpec, sub: &str) -> u32 {
        let mut n = 0;
        for a in &step.after {
            if let (Some(u), Some(s)) = (self.resolve(idx, ix, &a.step), self.specs.get(&a.step)) {
                if let Some(srt) = idx.phases.get(u).and_then(|p| rt.steps.get(&(p.id.clone(), a.step.clone()))) {
                    let c = |k: &str| srt.counts.get(k).map_or(0, |c| c.queued + c.running);
                    n += c(sub) + if sub.is_empty() { 0 } else { c("") };
                }
                n += self.upstream_pending(idx, rt, u, s, sub);
            }
        }
        n
    }

    fn can_activate(&self, idx: &Index, ix: usize) -> bool {
        let Some(spec) = self.spec_of(&idx.phases[ix].ty) else { return false };
        let mut referenced: HashSet<usize> = HashSet::new();
        for s in &spec.steps {
            for a in &s.after {
                if let Some(u) = self.resolve(idx, ix, &a.step) {
                    if u != ix {
                        referenced.insert(u);
                    }
                }
            }
        }
        (0..ix).all(|j| match idx.phases[j].status {
            StageStatus::Done | StageStatus::Skipped => true,
            StageStatus::Running => referenced.contains(&j),
            _ => false,
        })
    }

    fn init_phase(&self, rt: &mut Rt, idx: &Index, ix: usize) -> Result<(), String> {
        let p = &idx.phases[ix];
        let Some(spec) = self.spec_of(&p.ty) else { return Err(format!("phase `{}` has no declaration", p.ty)) };
        if rt.inited.insert(p.id.clone()) {
            for s in &spec.steps {
                rt.steps.insert((p.id.clone(), s.id.clone()), StepRt::default());
            }
            if p.steps.is_empty() {
                let steps = spec
                    .steps
                    .iter()
                    .map(|s| StepState { id: s.id.clone(), kind: s.kind, form: s.form.clone(), status: StageStatus::Pending, counts: Counts::default() })
                    .collect();
                self.mutate(Mutation::UpdatePhase { id: p.id.clone(), update: PhaseUpdate { steps: Some(steps), ..Default::default() } })?;
            }
            // Merge passes the journal holds are replayed now, from the membership they recorded.
            for s in spec.steps.iter().filter(|s| s.kind == StepKind::Merge) {
                let recs = rt.by_step.get(&(p.id.clone(), s.id.clone())).cloned().unwrap_or_default();
                for rec in recs.iter().filter(|r| !r.members.is_empty()) {
                    self.apply_record(rt, rec, true)?;
                    let srt = rt.steps.get_mut(&(p.id.clone(), s.id.clone())).ok_or("step state")?;
                    let m = srt.merge.entry(rec.sub.clone()).or_default();
                    m.consumed.extend(rec.members.iter().cloned());
                    m.passes += 1;
                    m.fired = m.fired.max(rec.threshold.map_or(0, |t| t + 1));
                    let c = srt.count(&rec.sub);
                    match rec.outcome {
                        Outcome::Done => c.done += 1,
                        Outcome::Failed => c.failed += 1,
                    }
                }
            }
        }
        Ok(())
    }

    fn activate(&self, rt: &mut Rt, idx: &Index, ix: usize) -> Result<(), String> {
        let p = &idx.phases[ix];
        // The state this phase starts from, so a project can be reopened here.
        self.handle.archive(&p.id).map_err(fail("could not write the phase snapshot"))?;
        self.mutate(Mutation::UpdatePhase {
            id: p.id.clone(),
            update: PhaseUpdate { status: Some(StageStatus::Running), started_ms: Some((self.cfg.clock)()), ..Default::default() },
        })?;
        let _ = rt;
        Ok(())
    }

    /// One round of scheduling. True when anything changed that a fresh look at the state could
    /// act on.
    fn advance(&self, rt: &mut Rt, idx: &Index) -> Result<bool, String> {
        let mut progress = false;
        rt.touched = false;
        // Extra gates (forced, budget, failures) that were answered while the project was parked.
        progress |= self.apply_extra_answers(rt, idx)?;
        if progress {
            return Ok(true);
        }
        for ix in 0..idx.phases.len() {
            let status = idx.phases[ix].status;
            if status == StageStatus::Pending && rt.park.is_none() && self.can_activate(idx, ix) {
                self.activate(rt, idx, ix)?;
                return Ok(true);
            }
        }
        for ix in 0..idx.phases.len() {
            let p = &idx.phases[ix];
            if p.status != StageStatus::Running {
                continue;
            }
            self.init_phase(rt, idx, ix)?;
            let spec = self.spec_of(&p.ty).cloned().ok_or("phase spec")?;
            for step in &spec.steps {
                progress |= match step.kind {
                    StepKind::Fanout | StepKind::Code => self.process_items(rt, idx, ix, step)?,
                    StepKind::Merge => self.process_merge(rt, idx, ix, step)?,
                    StepKind::Gate => self.process_gate(rt, idx, ix, step)?,
                };
                if rt.park.is_some() && matches!(rt.park, Some(Park::Step { .. }) | Some(Park::Open(_))) {
                    // A gate was reached: later steps of this phase wait behind it anyway.
                }
            }
            if progress {
                return Ok(true);
            }
            // The phase is done when every step is closed.
            let all_closed = spec.steps.iter().all(|s| rt.steps.get(&(p.id.clone(), s.id.clone())).is_some_and(|r| r.closed));
            if all_closed && rt.park.is_none() {
                if self.failures_gate(rt, idx, ix, &spec)? {
                    return Ok(false);
                }
                self.complete_phase(rt, idx, ix, &spec)?;
                return Ok(true);
            }
        }
        Ok(progress)
    }

    fn complete_phase(&self, rt: &mut Rt, idx: &Index, ix: usize, spec: &PhaseSpec) -> Result<(), String> {
        let p = &idx.phases[ix];
        let steps = self.step_states(rt, p, spec, true);
        let (done, total) = steps.iter().fold((0, 0), |(d, t), s| (d + s.counts.done + s.counts.failed, t + s.counts.done + s.counts.failed + s.counts.queued + s.counts.running));
        self.mutate(Mutation::UpdatePhase {
            id: p.id.clone(),
            update: PhaseUpdate {
                status: Some(StageStatus::Done),
                ended_ms: Some((self.cfg.clock)()),
                progress: Some(Progress { done, total }),
                steps: Some(steps),
                ..Default::default()
            },
        })
    }

    /// Raises a `failures` checkin when an important step of the finished phase lost too many
    /// items. True when the project is now parking for it.
    fn failures_gate(&self, rt: &mut Rt, idx: &Index, ix: usize, spec: &PhaseSpec) -> Result<bool, String> {
        let p = &idx.phases[ix];
        if idx.gates.iter().any(|g| g.reason == "failures" && g.about == p.id) {
            return Ok(false);
        }
        for s in spec.steps.iter().filter(|s| s.important) {
            let c = rt.steps.get(&(p.id.clone(), s.id.clone())).map(StepRt::total).unwrap_or_default();
            let n = c.done + c.failed;
            if n > 0 && f64::from(c.failed) / f64::from(n) > self.cfg.failure_share {
                rt.park = Some(Park::Extra { reason: "failures".into(), phase: Some(p.id.clone()) });
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn step_states(&self, rt: &Rt, p: &PhaseInfo, spec: &PhaseSpec, closing: bool) -> Vec<StepState> {
        spec.steps
            .iter()
            .map(|s| {
                let srt = rt.steps.get(&(p.id.clone(), s.id.clone()));
                let counts = srt.map(StepRt::total).unwrap_or_default();
                let status = if srt.is_some_and(|r| r.closed) || (closing && counts.queued + counts.running == 0 && counts.done + counts.failed > 0) {
                    StageStatus::Done
                } else if counts.queued + counts.running + counts.done + counts.failed > 0 {
                    StageStatus::Running
                } else {
                    StageStatus::Pending
                };
                StepState { id: s.id.clone(), kind: s.kind, form: s.form.clone(), status, counts }
            })
            .collect()
    }

    fn sync_counts(&self, rt: &Rt, idx: &Index) -> Result<(), String> {
        for p in idx.phases.iter().filter(|p| p.status == StageStatus::Running) {
            let Some(spec) = self.spec_of(&p.ty) else { continue };
            if !rt.inited.contains(&p.id) {
                continue;
            }
            let steps = self.step_states(rt, p, spec, false);
            if steps != p.steps {
                let (done, total) = steps.iter().fold((0, 0), |(d, t), s| (d + s.counts.done + s.counts.failed, t + s.counts.done + s.counts.failed + s.counts.queued + s.counts.running));
                let _ = self.handle.send(Mutation::UpdatePhase {
                    id: p.id.clone(),
                    update: PhaseUpdate { steps: Some(steps), progress: Some(Progress { done, total }), ..Default::default() },
                });
            }
        }
        Ok(())
    }

    // ---- fan-out and code steps --------------------------------------------------------------

    fn process_items(&self, rt: &mut Rt, idx: &Index, ix: usize, step: &StepSpec) -> Result<bool, String> {
        let p = &idx.phases[ix];
        let skey = (p.id.clone(), step.id.clone());
        if rt.steps.get(&skey).is_none_or(|s| s.closed) {
            return Ok(false);
        }
        let upstream_ok = self.upstream_closed(idx, rt, ix, step);
        let mut inputs: Vec<(Option<String>, String)> = Vec::new();
        match step.over_kind() {
            Some(kind) => {
                let from: HashSet<&str> = step.after.iter().map(|a| a.step.as_str()).collect();
                for it in idx.items(kind) {
                    if it.status != ItemStatus::Done {
                        continue;
                    }
                    let fits = if from.is_empty() {
                        step.scope == Scope::All || it.round == p.round
                    } else {
                        from.contains(it.step.as_str()) && it.round == p.round
                    };
                    if !fits || rt.steps[&skey].launched.contains(&it.id) {
                        continue;
                    }
                    if !self.barriers_open(idx, rt, ix, step, &it.sub, upstream_ok) {
                        continue;
                    }
                    inputs.push((Some(it.id.clone()), it.sub.clone()));
                }
            }
            None => {
                if upstream_ok && !rt.steps[&skey].launched.contains("project") {
                    inputs.push((None, String::new()));
                }
            }
        }
        let mut progress = false;
        for (item, sub) in inputs {
            let key = task_key(&p.id, &step.id, item.as_deref(), step.form.as_deref().unwrap_or(""), step.func.as_deref().unwrap_or(""), p.round);
            let who = item.clone().unwrap_or_else(|| "project".into());
            rt.steps.get_mut(&skey).ok_or("step state")?.launched.insert(who);
            if let Some(rec) = rt.by_key.get(&key).cloned() {
                // Already done in an earlier life of the project: replay, never ask again.
                self.apply_record(rt, &rec, true)?;
                let c = rt.steps.get_mut(&skey).ok_or("step state")?.count(&sub);
                match rec.outcome {
                    Outcome::Done => c.done += 1,
                    Outcome::Failed => c.failed += 1,
                }
                progress = true;
                continue;
            }
            rt.seq += 1;
            let task = Task {
                id: rt.seq,
                phase_ix: ix,
                step_ix: self.spec_of(&p.ty).and_then(|s| s.steps.iter().position(|x| x.id == step.id)).unwrap_or(0),
                phase: p.id.clone(),
                round: p.round,
                step: self.specs.get(&step.id).cloned().unwrap_or_else(|| Arc::new(step.clone())),
                item,
                sub: sub.clone(),
                key,
                merge: None,
                seq: rt.seq,
            };
            rt.steps.get_mut(&skey).ok_or("step state")?.count(&sub).queued += 1;
            rt.queue.push(task);
            progress = true;
        }
        let srt = rt.steps.get_mut(&skey).ok_or("step state")?;
        // Closed only on a fresh look: a replay above may have put the very items this step waits for.
        if upstream_ok && !srt.busy(None) && !rt.touched {
            srt.closed = true;
            progress = true;
        }
        Ok(progress)
    }

    /// Whether the barrier edges of `step` let an input of group `sub` through.
    fn barriers_open(&self, idx: &Index, rt: &Rt, ix: usize, step: &StepSpec, sub: &str, upstream_all_closed: bool) -> bool {
        step.after.iter().all(|a| {
            if a.edge != Edge::Barrier {
                return true;
            }
            if a.per.as_deref() == Some("sub") {
                match (self.resolve(idx, ix, &a.step), self.specs.get(&a.step)) {
                    (Some(u), Some(s)) => !self.group_busy(idx, rt, u, s, sub),
                    _ => true,
                }
            } else {
                match self.resolve(idx, ix, &a.step) {
                    Some(u) => self.closed(idx, rt, u, &a.step),
                    None => upstream_all_closed || true,
                }
            }
        })
    }

    // ---- delta merge -------------------------------------------------------------------------

    fn process_merge(&self, rt: &mut Rt, idx: &Index, ix: usize, step: &StepSpec) -> Result<bool, String> {
        let p = &idx.phases[ix];
        let skey = (p.id.clone(), step.id.clone());
        if rt.steps.get(&skey).is_none_or(|s| s.closed) {
            return Ok(false);
        }
        let upstream_ok = self.upstream_closed(idx, rt, ix, step);
        let kind = step.over_kind().unwrap_or("");
        let watch = step.watch.clone().unwrap_or_default();
        let thresholds = step.thresholds();
        let min_new = step.min_new.unwrap_or(1) as usize;
        let from: HashSet<&str> = step.after.iter().map(|a| a.step.as_str()).collect();
        // The items on offer, by group.
        let mut by_sub: HashMap<String, Vec<&ItemInfo>> = HashMap::new();
        for it in idx.items(kind) {
            if it.status == ItemStatus::Done && it.round == p.round && (from.is_empty() || from.contains(it.step.as_str())) {
                by_sub.entry(it.sub.clone()).or_default().push(it);
            }
        }
        let watch_ix = self.resolve(idx, ix, &watch);
        let watch_spec = self.specs.get(&watch).cloned();
        let mut progress = false;
        let mut subs: Vec<String> = by_sub.keys().cloned().collect();
        subs.sort();
        for sub in subs {
            let items = &by_sub[&sub];
            let (new, pass, fired, inflight) = {
                let m = rt.steps.get_mut(&skey).ok_or("step state")?.merge.entry(sub.clone()).or_default();
                let new: Vec<String> = items.iter().filter(|i| !m.consumed.contains(&i.id) && !m.inflight.contains(&i.id)).map(|i| i.id.clone()).collect();
                (new, m.passes, m.fired, !m.inflight.is_empty())
            };
            if new.is_empty() || inflight {
                continue;
            }
            let settled = upstream_ok || {
                // Everything upstream of the merge is done for this group, and so is the watched step.
                let up_idle = step.after.iter().all(|a| match (self.resolve(idx, ix, &a.step), self.specs.get(&a.step)) {
                    (Some(u), Some(s)) => !self.group_busy(idx, rt, u, s, &sub),
                    _ => true,
                });
                let watch_idle = match (watch_ix, &watch_spec) {
                    (Some(u), Some(s)) => !self.group_busy(idx, rt, u, s, &sub),
                    _ => true,
                };
                up_idle && watch_idle
            };
            let threshold = if settled {
                Some(thresholds.len() - 1)
            } else {
                let share = self.watch_share(idx, rt, watch_ix, watch_spec.as_deref(), &sub);
                let crossed = thresholds.iter().rposition(|t| share + 1e-9 >= *t);
                crossed.filter(|c| *c >= fired && *c < thresholds.len() - 1 && new.len() >= min_new)
            };
            let Some(threshold) = threshold else { continue };
            let key = merge_key(&p.id, &step.id, &sub, &new);
            {
                let m = rt.steps.get_mut(&skey).ok_or("step state")?.merge.entry(sub.clone()).or_default();
                m.fired = m.fired.max(threshold + 1);
            }
            if let Some(rec) = rt.by_key.get(&key).cloned() {
                self.apply_record(rt, &rec, true)?;
                let srt = rt.steps.get_mut(&skey).ok_or("step state")?;
                let m = srt.merge.entry(sub.clone()).or_default();
                m.consumed.extend(rec.members.iter().cloned());
                m.passes += 1;
                let c = srt.count(&sub);
                match rec.outcome {
                    Outcome::Done => c.done += 1,
                    Outcome::Failed => c.failed += 1,
                }
                progress = true;
                continue;
            }
            rt.seq += 1;
            let task = Task {
                id: rt.seq,
                phase_ix: ix,
                step_ix: self.spec_of(&p.ty).and_then(|s| s.steps.iter().position(|x| x.id == step.id)).unwrap_or(0),
                phase: p.id.clone(),
                round: p.round,
                step: self.specs.get(&step.id).cloned().unwrap_or_else(|| Arc::new(step.clone())),
                item: None,
                sub: sub.clone(),
                key,
                merge: Some(MergeTask { members: new.clone(), threshold, pass }),
                seq: rt.seq,
            };
            let srt = rt.steps.get_mut(&skey).ok_or("step state")?;
            srt.merge.entry(sub.clone()).or_default().inflight.extend(new);
            srt.count(&sub).queued += 1;
            rt.queue.push(task);
            progress = true;
        }
        // Closed when upstream is closed, the watched step is closed, and nothing is left to merge.
        let watch_closed = watch_ix.map_or(true, |u| self.closed(idx, rt, u, &watch));
        let touched = rt.touched;
        let srt = rt.steps.get_mut(&skey).ok_or("step state")?;
        let pending_new = by_sub.iter().any(|(s, items)| {
            let m = srt.merge.get(s);
            items.iter().any(|i| !m.is_some_and(|m| m.consumed.contains(&i.id)))
        });
        if upstream_ok && watch_closed && !pending_new && !srt.busy(None) && !touched {
            srt.closed = true;
            progress = true;
        }
        Ok(progress)
    }

    /// The share of the watched step that is finished for a group: its finished tasks over the
    /// tasks it has plus the ones still queued or running upstream of it (an estimate: a page not
    /// yet fetched will become one more analysis).
    fn watch_share(&self, idx: &Index, rt: &Rt, watch_ix: Option<usize>, watch: Option<&StepSpec>, sub: &str) -> f64 {
        let (Some(w), Some(spec)) = (watch_ix, watch) else { return 1.0 };
        if self.group_busy(idx, rt, w, spec, sub) {
            let Some(srt) = idx.phases.get(w).and_then(|p| rt.steps.get(&(p.id.clone(), spec.id.clone()))) else { return 0.0 };
            let c = |k: &str| srt.counts.get(k).copied().unwrap_or_default();
            let (a, b) = (c(sub), if sub.is_empty() { Counts::default() } else { c("") });
            let finished = f64::from(a.done + a.failed + b.done + b.failed);
            let known = f64::from(a.queued + a.running + b.queued + b.running) + finished;
            let expected = known + f64::from(self.upstream_pending(idx, rt, w, spec, sub));
            if expected <= 0.0 {
                0.0
            } else {
                finished / expected
            }
        } else {
            1.0
        }
    }

    // ---- gates -------------------------------------------------------------------------------

    fn process_gate(&self, rt: &mut Rt, idx: &Index, ix: usize, step: &StepSpec) -> Result<bool, String> {
        let p = &idx.phases[ix];
        let skey = (p.id.clone(), step.id.clone());
        if rt.steps.get(&skey).is_none_or(|s| s.closed) || !self.upstream_closed(idx, rt, ix, step) {
            return Ok(false);
        }
        let gate = idx.gates.iter().find(|g| g.phase == p.id && g.step == step.id);
        match gate {
            None => {
                // A continuation's plan card needs no approval: the user just chose to continue.
                if step.gate == Some(GateKind::Plan) && p.round > 1 {
                    let ans = GateAnswer { choice: "approve".into(), text: None };
                    let now = (self.cfg.clock)();
                    let g = Gate {
                        id: "auto".into(),
                        kind: GateKind::Plan,
                        status: GateStatus::Answered,
                        payload: json!({"kind": "plan", "reason": "plan"}),
                        options: Vec::new(),
                        answer: Some(ans.clone()),
                        asked_ms: now,
                        answered_ms: Some(now),
                        phase: p.id.clone(),
                        step: step.id.clone(),
                    };
                    self.handle_answer(rt, idx, p, step, &g, &ans, "auto")?;
                    rt.steps.get_mut(&skey).ok_or("step state")?.closed = true;
                    return Ok(true);
                }
                if rt.park.is_none() {
                    rt.park = Some(Park::Step { phase: p.id.clone(), step: step.id.clone() });
                }
                Ok(false)
            }
            Some(g) if g.status == GateStatus::Open => {
                if rt.park.is_none() {
                    rt.park = Some(Park::Open(g.id.clone()));
                }
                Ok(false)
            }
            Some(g) => {
                let gid = g.id.clone();
                let key = answer_key(&p.id, &step.id, &gid);
                if let Some(rec) = rt.by_key.get(&key).cloned() {
                    self.apply_record(rt, &rec, true)?;
                } else {
                    let st_gate = self.handle.read({
                        let gid = gid.clone();
                        move |st| st.gates.iter().find(|g| g.id == gid).cloned()
                    });
                    let st_gate = st_gate.map_err(fail("the project state is gone"))?.ok_or("the gate vanished")?;
                    let ans = st_gate.answer.clone().ok_or("an answered gate has no answer")?;
                    self.handle_answer(rt, idx, p, step, &st_gate, &ans, &gid)?;
                }
                if self.project_status()? != ProjectStatus::Cancelled {
                    self.mutate(Mutation::SetStatus(ProjectStatus::Running))?;
                }
                rt.steps.get_mut(&skey).ok_or("step state")?.closed = true;
                Ok(true)
            }
        }
    }

    fn project_status(&self) -> Result<ProjectStatus, String> {
        self.handle.read(|st| st.status).map_err(fail("the project state is gone"))
    }

    /// Runs a gate's answer function, journals its effects and applies them.
    fn handle_answer(&self, rt: &mut Rt, idx: &Index, p: &PhaseInfo, step: &StepSpec, gate: &Gate, ans: &GateAnswer, gid: &str) -> Result<(), String> {
        let _ = idx;
        let key = answer_key(&p.id, &step.id, gid);
        if let Some(done) = rt.by_key.get(&key).cloned() {
            return self.apply_record(rt, &done, true);
        }
        let mut rec = Record::new(key, &p.id, &step.id, None, "");
        rec.gate = Some(gid.to_string());
        if ans.choice == "cancel" {
            // Cancelling is the engine's, not the card's: nothing else may run.
            self.cancel.cancel();
            self.journal(&rec)?;
            rt.by_key.insert(rec.key.clone(), rec);
            return Ok(());
        }
        let name = step.answer_fn.as_deref().unwrap_or("");
        match self.tools.get_answer(name) {
            None => {
                if !name.is_empty() {
                    return Err(format!("gate answer `{name}` is not registered"));
                }
            }
            Some(f) => {
                let (g2, a2, f2) = (gate.clone(), ans.clone(), f.clone());
                let effects = self.handle.read(move |st| f2(st, &g2, &a2)).map_err(fail("the project state is gone"))?;
                match effects {
                    Ok(e) => rec.effects = e,
                    Err(why) => {
                        rec.outcome = Outcome::Failed;
                        rec.error = Some(why.clone());
                        // A refused answer leaves the gate answered and the project going on without it.
                    }
                }
            }
        }
        rec.at_ms = (self.cfg.clock)();
        self.journal(&rec)?;
        self.apply_record(rt, &rec, false)?;
        rt.by_key.insert(rec.key.clone(), rec);
        Ok(())
    }

    /// Answers of gates the engine raised itself (forced, budget, failures) carry no step, so they
    /// are handled here, once, in the order they were answered.
    fn apply_extra_answers(&self, rt: &mut Rt, idx: &Index) -> Result<bool, String> {
        let mut any = false;
        for g in idx.gates.iter().filter(|g| g.status == GateStatus::Answered && g.step.is_empty()) {
            let key = answer_key("_gates", "answers", &g.id);
            if let Some(rec) = rt.by_key.get(&key).cloned() {
                if !rt.inited.contains(&key) {
                    rt.inited.insert(key);
                    self.apply_record(rt, &rec, true)?;
                    any = true;
                }
                continue;
            }
            let gid = g.id.clone();
            let st_gate = self.handle.read(move |st| st.gates.iter().find(|x| x.id == gid).cloned()).map_err(fail("the project state is gone"))?.ok_or("the gate vanished")?;
            let ans = st_gate.answer.clone().ok_or("an answered gate has no answer")?;
            let mut rec = Record::new(key.clone(), "_gates", "answers", None, "");
            rec.gate = Some(g.id.clone());
            if ans.choice == "cancel" {
                self.cancel.cancel();
            } else if let Some(f) = self.tools.get_answer("checkin_answer") {
                let (g2, a2, f2) = (st_gate.clone(), ans.clone(), f.clone());
                match self.handle.read(move |st| f2(st, &g2, &a2)).map_err(fail("the project state is gone"))? {
                    Ok(e) => rec.effects = e,
                    Err(why) => {
                        rec.outcome = Outcome::Failed;
                        rec.error = Some(why);
                    }
                }
            }
            rec.at_ms = (self.cfg.clock)();
            self.journal(&rec)?;
            self.apply_record(rt, &rec, false)?;
            rt.by_key.insert(key.clone(), rec);
            rt.inited.insert(key);
            if self.project_status()? != ProjectStatus::Cancelled {
                self.mutate(Mutation::SetStatus(ProjectStatus::Running))?;
            }
            any = true;
        }
        Ok(any)
    }

    /// With every worker back: open the gate the project is heading for and report the park.
    fn raise_and_park(&self, rt: &mut Rt, idx: &Index, park: &Park) -> Result<Option<RunOutcome>, String> {
        match park {
            Park::Open(id) => {
                self.mutate_status_for_open(id)?;
                self.handle.flush().map_err(fail("could not save"))?;
                Ok(Some(RunOutcome::Parked { gate: id.clone() }))
            }
            Park::Step { phase, step } => {
                let Some(p) = idx.phases.iter().find(|p| p.id == *phase) else { return Err("the gate's phase vanished".into()) };
                let spec = self.specs.get(step).cloned().ok_or("the gate's step vanished")?;
                let reason = if spec.gate == Some(GateKind::Plan) { "plan" } else { "lead_decision" };
                let gate = self.push_gate(idx, spec.gate.unwrap_or(GateKind::Checkin), &p.id, &spec.id, reason, spec.payload_fn.as_deref().unwrap_or(""), &spec)?;
                rt.park = Some(Park::Open(gate.clone()));
                Ok(Some(RunOutcome::Parked { gate }))
            }
            Park::Extra { reason, phase } => {
                let about = phase.clone().or_else(|| idx.phases.iter().rev().find(|p| p.status == StageStatus::Running).map(|p| p.id.clone())).unwrap_or_default();
                let spec: Arc<StepSpec> = Arc::new(
                    serde_json::from_value(json!({"id": "checkin", "kind": "gate", "gate": "checkin", "payload_fn": "checkin_card", "answer_fn": "checkin_answer"}))
                        .map_err(|e| e.to_string())?,
                );
                let gate = self.push_gate_extra(idx, &about, reason, &spec)?;
                rt.park = Some(Park::Open(gate.clone()));
                Ok(Some(RunOutcome::Parked { gate }))
            }
        }
    }

    fn mutate_status_for_open(&self, id: &str) -> Result<(), String> {
        let id = id.to_string();
        let kind = self.handle.read(move |st| st.gates.iter().find(|g| g.id == id).map(|g| g.kind)).map_err(fail("the project state is gone"))?;
        let status = if kind == Some(GateKind::Plan) { ProjectStatus::AwaitingPlan } else { ProjectStatus::AwaitingCheckin };
        if self.project_status()? != status {
            self.mutate(Mutation::SetStatus(status))?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn push_gate(&self, idx: &Index, kind: GateKind, phase: &str, step: &str, reason: &str, payload_fn: &str, spec: &Arc<StepSpec>) -> Result<String, String> {
        let f = self.tools.get_payload(payload_fn).cloned().ok_or_else(|| format!("gate card `{payload_fn}` is not registered"))?;
        let (phase_id, spec2, reason2) = (phase.to_string(), spec.clone(), reason.to_string());
        let card: Result<GateCard, String> = self
            .handle
            .read(move |st| {
                let p = st.phases.iter().find(|p| p.id == phase_id).ok_or("the gate's phase vanished")?;
                Ok(f(&GateCtx { state: st, phase: p, step: &spec2, reason: &reason2 }))
            })
            .map_err(fail("the project state is gone"))?;
        let mut card = card?;
        if let Some(o) = card.payload.as_object_mut() {
            o.entry("reason").or_insert(json!(reason));
            if step.is_empty() {
                // A gate the engine raised belongs to no step; the phase it is about is recorded
                // so a failures checkin is raised once per phase.
                o.insert("phase".into(), json!(phase));
            }
        }
        let gate = Gate {
            id: format!("g_{}", idx.max_gate + 1),
            kind,
            status: GateStatus::Open,
            payload: card.payload,
            options: card.options,
            answer: None,
            asked_ms: (self.cfg.clock)(),
            answered_ms: None,
            phase: phase.to_string(),
            step: step.to_string(),
        };
        let id = gate.id.clone();
        let status = if kind == GateKind::Plan { ProjectStatus::AwaitingPlan } else { ProjectStatus::AwaitingCheckin };
        self.mutate(Mutation::SetStatus(status))?;
        self.mutate(Mutation::PushGate(Box::new(gate)))?;
        self.handle.flush().map_err(fail("could not save"))?;
        Ok(id)
    }

    fn push_gate_extra(&self, idx: &Index, about: &str, reason: &str, spec: &Arc<StepSpec>) -> Result<String, String> {
        // The card is built against the phase that was running; the gate belongs to no step.
        let id = self.push_gate(idx, GateKind::Checkin, about, "", reason, "checkin_card", spec)?;
        // Record which phase it is about, so a failures gate is raised once per phase.
        let _ = about;
        Ok(id)
    }

    // ---- results -----------------------------------------------------------------------------

    fn handle_msg(&self, rt: &mut Rt, m: Msg) -> Result<(), String> {
        let Msg::Done(d) = m else { return Ok(()) };
        let Some(task) = rt.running.remove(&d.id) else { return Ok(()) };
        let lane = lane_key(&task, &self.cfg, &self.gate);
        if let Some(n) = rt.lane_running.get_mut(&lane) {
            *n = n.saturating_sub(1);
        }
        let skey = (task.phase.clone(), task.step.id.clone());
        if let Some(s) = rt.steps.get_mut(&skey) {
            s.running = s.running.saturating_sub(1);
            let c = s.count(&task.sub);
            c.running = c.running.saturating_sub(1);
        }
        if d.cancelled {
            return Ok(());
        }
        let mut rec = d.rec;
        rec.at_ms = (self.cfg.clock)();
        self.journal(&rec)?;
        self.apply_record(rt, &rec, false)?;
        let srt = rt.steps.get_mut(&skey).ok_or("step state")?;
        match rec.outcome {
            Outcome::Done => srt.count(&task.sub).done += 1,
            Outcome::Failed => srt.count(&task.sub).failed += 1,
        }
        if let Some(m) = &task.merge {
            let mrt = srt.merge.entry(task.sub.clone()).or_default();
            for id in &m.members {
                mrt.inflight.remove(id);
                mrt.consumed.insert(id.clone());
            }
            mrt.passes += 1;
        }
        rt.by_key.insert(rec.key.clone(), rec);
        Ok(())
    }

    /// Applies a record to the state: the agent, the items, the effects, the phase output. The
    /// same code serves a live result and a replay from the journal, and every part of it is
    /// idempotent.
    fn apply_record(&self, rt: &mut Rt, rec: &Record, replay: bool) -> Result<(), String> {
        rt.touched = true;
        if let Some(a) = &rec.agent {
            self.mutate(Mutation::PutAgent(Box::new(a.clone())))?;
        }
        for it in &rec.items {
            match self.handle.mutate(Mutation::PutItem(Box::new(it.clone()))) {
                Ok(_) => {}
                Err(ActorError::Mutation(MutationError::Invalid { errors, .. })) => {
                    // The function's data broke its form: the item is failed, not stored as done.
                    let mut bad = it.clone();
                    bad.status = ItemStatus::Failed;
                    bad.error = Some(format!("the result does not match its form: {}", errors.iter().take(2).cloned().collect::<Vec<_>>().join("; ")));
                    self.mutate(Mutation::PutItem(Box::new(bad)))?;
                }
                Err(e) => return Err(format!("could not store item `{}`: {e}", it.id)),
            }
        }
        for e in &rec.effects {
            self.apply_effect(rt, e)?;
        }
        if let Some(out) = &rec.output {
            self.mutate(Mutation::UpdatePhase { id: rec.phase.clone(), update: PhaseUpdate { output: Some(out.clone()), ..Default::default() } })?;
        }
        if rec.tainted {
            self.mutate(Mutation::SetTainted(true))?;
        }
        if !replay && (rec.tokens_in > 0 || rec.tokens_out > 0) {
            rt.metrics.tokens.input += rec.tokens_in;
            rt.metrics.tokens.out += rec.tokens_out;
            rt.metrics_dirty = true;
        }
        Ok(())
    }

    fn apply_effect(&self, rt: &mut Rt, e: &Effect) -> Result<(), String> {
        match e {
            Effect::InsertPhase { before, phase } => {
                let id = phase.id.clone();
                let exists = self.handle.read(move |st| st.phases.iter().any(|p| p.id == id)).map_err(fail("the project state is gone"))?;
                if !exists {
                    self.mutate(Mutation::InsertPhase { before: before.clone(), phase: phase.clone() })?;
                }
            }
            Effect::SetPlan { plan } => self.mutate(Mutation::SetPlan(plan.clone()))?,
            Effect::PlanRecord { record } => self.mutate(Mutation::PushPlanRecord(record.clone()))?,
            Effect::NetGrant { quota } => {
                let quota = *quota;
                let mut g = self.handle.read(|st| st.grants.clone()).map_err(fail("the project state is gone"))?;
                let net = g.net.get_or_insert(NetGrant { scope: "run".into(), public_https_get: true, robots: true, quota: 0, used: 0 });
                net.quota = net.quota.max(quota);
                net.public_https_get = true;
                net.robots = true;
                self.mutate(Mutation::SetGrants(g))?;
            }
            Effect::PhaseStatus { id, status } => {
                self.mutate(Mutation::UpdatePhase { id: id.clone(), update: PhaseUpdate { status: Some(*status), ..Default::default() } })?
            }
            Effect::JumpToReport => {
                let now = (self.cfg.clock)();
                let skip: Vec<String> = self
                    .handle
                    .read(|st| {
                        let target = st
                            .phases
                            .iter()
                            .position(|p| p.ty == "Report" && p.status != StageStatus::Done)
                            .or_else(|| st.phases.iter().position(|p| p.ty == "Finish" && p.status != StageStatus::Done))
                            .unwrap_or(st.phases.len());
                        st.phases[..target].iter().filter(|p| !matches!(p.status, StageStatus::Done | StageStatus::Skipped)).map(|p| p.id.clone()).collect()
                    })
                    .map_err(fail("the project state is gone"))?;
                for id in skip {
                    self.mutate(Mutation::UpdatePhase { id, update: PhaseUpdate { status: Some(StageStatus::Skipped), ended_ms: Some(now), ..Default::default() } })?;
                }
            }
            Effect::RaiseBudget { to } => {
                rt.metrics.budget_raises = rt.metrics.budget_raises.max(*to);
                rt.metrics_dirty = true;
                self.mutate(run_metrics(&rt.metrics))?;
            }
        }
        Ok(())
    }

    // ---- launching ---------------------------------------------------------------------------

    fn over_budget(&self, rt: &Rt) -> bool {
        let scale = 1 + u64::from(rt.metrics.budget_raises);
        let tokens = rt.metrics.tokens.input + rt.metrics.tokens.out;
        if self.cfg.budget.tokens.is_some_and(|cap| tokens >= cap.saturating_mul(scale)) {
            return true;
        }
        let minutes = (rt.base_active_ms + rt.began.elapsed().as_millis() as u64) / 60_000;
        self.cfg.budget.minutes.is_some_and(|cap| minutes >= cap.saturating_mul(scale))
    }

    fn launch(&self, sh: &Arc<Shared>, rt: &mut Rt) -> Result<(), String> {
        loop {
            // The best runnable task: later phase, later step, least recently served group, oldest.
            let mut best: Option<usize> = None;
            for (i, t) in rt.queue.iter().enumerate() {
                let lane = lane_key(t, &self.cfg, &self.gate);
                let cap = lane_cap(t, &self.cfg, &self.gate);
                if rt.lane_running.get(&lane).copied().unwrap_or(0) >= cap {
                    continue;
                }
                if let Some(c) = t.step.concurrency {
                    if rt.steps.get(&(t.phase.clone(), t.step.id.clone())).is_some_and(|s| s.running >= c) {
                        continue;
                    }
                }
                let rank = |t: &Task, served: &HashMap<(String, String), u64>| (std::cmp::Reverse(t.phase_ix), std::cmp::Reverse(t.step_ix), served.get(&(t.step.id.clone(), t.sub.clone())).copied().unwrap_or(0), t.seq);
                if best.is_none_or(|b| rank(t, &rt.served) < rank(&rt.queue[b], &rt.served)) {
                    best = Some(i);
                }
            }
            let Some(i) = best else { return Ok(()) };
            if rt.queue[i].step.lane() == Lane::Model && self.over_budget(rt) {
                rt.park = Some(Park::Extra { reason: "budget".into(), phase: None });
                return Ok(());
            }
            let task = rt.queue.remove(i);
            let lane = lane_key(&task, &self.cfg, &self.gate);
            *rt.lane_running.entry(lane).or_default() += 1;
            *rt.served.entry((task.step.id.clone(), task.sub.clone())).or_default() += 1;
            let skey = (task.phase.clone(), task.step.id.clone());
            if let Some(s) = rt.steps.get_mut(&skey) {
                s.running += 1;
                let c = s.count(&task.sub);
                c.queued = c.queued.saturating_sub(1);
                c.running += 1;
            }
            let (sh2, tx, t2) = (sh.clone(), self.tx.clone(), task.clone());
            let name = format!("task-{}-{}", task.step.id, task.id);
            let spawned = std::thread::Builder::new().name(name).spawn(move || {
                // A function that panics fails its task; it must never leave the engine waiting.
                let (rec, cancelled) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_task(&sh2, &t2))).unwrap_or_else(|_| {
                    let mut rec = Record::new(t2.key.clone(), &t2.phase, &t2.step.id, t2.item.clone(), &t2.sub);
                    rec.outcome = Outcome::Failed;
                    rec.error = Some(format!("step `{}` stopped unexpectedly", t2.step.id));
                    (rec, false)
                });
                let _ = tx.send(Msg::Done(Box::new(Done { id: t2.id, rec, cancelled })));
            });
            if let Err(e) = spawned {
                return Err(format!("could not start a worker thread: {e}"));
            }
            rt.running.insert(task.id, task);
        }
    }

    // ---- ending ------------------------------------------------------------------------------

    fn finished_or_stuck(&self, rt: &mut Rt, idx: &Index) -> Result<Option<RunOutcome>, String> {
        if idx.phases.iter().all(|p| matches!(p.status, StageStatus::Done | StageStatus::Skipped)) && rt.running.is_empty() {
            let have_final = self.store.snapshots(self.handle.id()).map_err(|e| e.to_string())?.iter().any(|s| s.label == "final");
            if !have_final {
                self.handle.archive("final").map_err(fail("could not write the final snapshot"))?;
            }
            self.mutate(Mutation::SetStatus(ProjectStatus::Finished))?;
            self.handle.flush().map_err(fail("could not save"))?;
            return Ok(Some(RunOutcome::Finished));
        }
        Ok(None)
    }

    fn finish_suspend(&self, rt: &mut Rt) -> Result<RunOutcome, String> {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !rt.running.is_empty() && Instant::now() < deadline {
            match self.rx.recv_timeout(Duration::from_millis(50)) {
                Ok(Msg::Done(d)) => {
                    rt.running.remove(&d.id);
                }
                Ok(Msg::Wake) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        self.handle.flush().map_err(fail("could not save"))?;
        Ok(RunOutcome::Suspended)
    }

    fn finish_cancel(&self, rt: &mut Rt) -> Result<RunOutcome, String> {
        // Workers see the token and come back at once; their results are dropped.
        let deadline = Instant::now() + Duration::from_secs(15);
        while !rt.running.is_empty() && Instant::now() < deadline {
            match self.rx.recv_timeout(Duration::from_millis(50)) {
                Ok(Msg::Done(d)) => {
                    rt.running.remove(&d.id);
                }
                Ok(Msg::Wake) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        let now = (self.cfg.clock)();
        let st = self.handle.snapshot().map_err(fail("the project state is gone"))?;
        for it in st.items.values().filter(|i| matches!(i.status, ItemStatus::Queued | ItemStatus::Running)) {
            self.mutate(Mutation::UpdateItem { id: it.id.clone(), update: ItemUpdate { status: Some(ItemStatus::Cancelled), ..Default::default() } })?;
        }
        for a in st.agents.values().filter(|a| a.status == AgentStatus::Running) {
            let mut a = a.clone();
            a.status = AgentStatus::Failed;
            a.ended_ms = Some(now);
            self.mutate(Mutation::PutAgent(Box::new(a)))?;
        }
        for p in st.phases.iter().filter(|p| matches!(p.status, StageStatus::Pending | StageStatus::Running)) {
            self.mutate(Mutation::UpdatePhase { id: p.id.clone(), update: PhaseUpdate { status: Some(StageStatus::Skipped), ended_ms: Some(now), ..Default::default() } })?;
        }
        self.mutate(Mutation::SetStatus(ProjectStatus::Cancelled))?;
        self.handle.flush().map_err(fail("could not save"))?;
        Ok(RunOutcome::Cancelled)
    }
}

/// The key of the lane a task runs on: `model:<model>` for a model step (so each model has its own
/// width), else the lane's name.
fn lane_key(t: &Task, cfg: &EngineConfig, _gate: &Arc<dyn ModelGate>) -> String {
    match t.step.lane() {
        Lane::Model => format!("model:{}", model_of(t, cfg)),
        l => l.as_str().to_string(),
    }
}

fn model_of<'a>(t: &Task, cfg: &'a EngineConfig) -> &'a str {
    match t.step.class {
        crate::spec::ModelClass::Employee => &cfg.employee_model,
        crate::spec::ModelClass::Lead => &cfg.lead_model,
    }
}

fn lane_cap(t: &Task, cfg: &EngineConfig, gate: &Arc<dyn ModelGate>) -> usize {
    match t.step.lane() {
        Lane::Model => gate.width(model_of(t, cfg)).max(1),
        Lane::Fetch => cfg.lanes.fetch.max(1),
        Lane::Search => cfg.lanes.search.max(1),
        Lane::Cpu => cfg.lanes.cpu.max(1),
        Lane::Io => cfg.lanes.io.max(1),
    }
}
