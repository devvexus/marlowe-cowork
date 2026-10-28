//! What a project shows of the machine and of its models while it runs: the meters move with a
//! probe and fall back to zero, an employee held back for the GPU is a waiting agent with its
//! reason, and a lead on its own model is called through its own provider and slots. Fake models
//! and gates only: nothing here needs a GPU.

mod common;

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use marlowe_model::CancelToken;
use marlowe_project::engine::{ByModel, GateError, Machine, MachineProbe, ModelGate, Permit, SlotPool};
use marlowe_project::*;

/// Answers a fixed machine and counts how often it was asked.
struct Steady {
    reads: AtomicUsize,
    machine: Machine,
}

impl MachineProbe for Steady {
    fn sample(&self) -> Machine {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.machine
    }
}

fn approve_and_run(p: &Project, model: Arc<Fake>, gate: Arc<dyn ModelGate>, cfg: EngineConfig) -> engine::RunHandle {
    let e = p.engine(model.clone(), gate.clone(), research_tools(), cfg.clone());
    assert!(matches!(join_within(e.spawn().unwrap(), 20), RunOutcome::Parked { .. }));
    let e = p.engine(model, gate, research_tools(), cfg);
    e.answer("g_1", "approve", None).unwrap();
    e.spawn().unwrap()
}

#[test]
fn the_meters_follow_the_probe_while_the_project_runs_write_only_changes_and_return_to_zero() {
    let p = Project::new("meters", Registry::builtin(), &research_plan(2));
    // A slow model, so the run lasts long enough for several readings.
    let model = Fake::with(|_, _, _| {
        std::thread::sleep(Duration::from_millis(25));
        Say::Schema
    });
    let probe = Arc::new(Steady { reads: AtomicUsize::new(0), machine: Machine { cpu_pct: 37.4, gpu_pct: 81.6, vram_mb: 9_210 } });
    let mut cfg = config("mini");
    cfg.probe = Some(probe.clone());
    cfg.probe_every = Duration::from_millis(10);
    let h = approve_and_run(&p, model, Arc::new(SlotPool::new(2)), cfg);

    // Mid-run the numbers are the probe's, rounded.
    wait_until(60, "the meters to move", || p.state().metrics.vram_mb == 9_210);
    let m = p.state().metrics;
    assert_eq!((m.cpu_pct, m.gpu_pct, m.vram_mb), (37.0, 82.0, 9_210));
    assert_eq!(join_within(h, 120), RunOutcome::Finished);

    // When the engine has stopped the project uses nothing.
    let m = p.state().metrics;
    assert_eq!((m.cpu_pct, m.gpu_pct, m.vram_mb), (0.0, 0.0, 0));
    assert!(probe.reads.load(Ordering::SeqCst) > 3, "the probe was read repeatedly");

    // A constant reading is written once on the way up and once on the way down, not at every read.
    let mut writes = 0;
    while let Ok(ev) = p.events.try_recv() {
        if let ProjectEvent::Updated { patches, .. } = ev {
            writes += patches.iter().filter(|x| x.path == "/metrics/vram_mb").count();
        }
    }
    assert_eq!(writes, 2, "patches for /metrics/vram_mb: up, down");
}

#[test]
fn without_a_probe_the_meters_stay_at_zero() {
    let p = Project::new("nometers", Registry::builtin(), &research_plan(1));
    let h = approve_and_run(&p, Fake::schema(), Arc::new(SlotPool::new(2)), config("mini"));
    assert_eq!(join_within(h, 120), RunOutcome::Finished);
    let m = p.state().metrics;
    assert_eq!((m.cpu_pct, m.gpu_pct, m.vram_mb), (0.0, 0.0, 0));
}

/// A gate that holds every employee "for the GPU" until it is let go, and says so.
struct HeldForGpu {
    open: Arc<AtomicU64>,
    pool: SlotPool,
}

impl ModelGate for HeldForGpu {
    fn enter(&self, model: &str, cancel: &CancelToken) -> Result<Permit, GateError> {
        self.enter_noting(model, cancel, &mut |_| {})
    }

    fn enter_noting(&self, model: &str, cancel: &CancelToken, note: &mut dyn FnMut(&str)) -> Result<Permit, GateError> {
        while self.open.load(Ordering::SeqCst) == 0 {
            if cancel.is_cancelled() {
                return Err(GateError::Cancelled);
            }
            note("waiting for the GPU");
            std::thread::sleep(Duration::from_millis(10));
        }
        self.pool.enter(model, cancel)
    }

    fn width(&self, model: &str) -> usize {
        self.pool.width(model)
    }
}

#[test]
fn an_employee_held_for_the_gpu_is_a_waiting_agent_with_its_reason_until_its_call_starts() {
    let p = Project::new("held", Registry::builtin(), &research_plan(1));
    let open = Arc::new(AtomicU64::new(0));
    let model = Fake::schema();
    let h = approve_and_run(&p, model.clone(), Arc::new(HeldForGpu { open: open.clone(), pool: SlotPool::new(2) }), config("mini"));

    wait_until(60, "an employee to wait for the GPU", || p.state().agents.values().any(|a| a.status == AgentStatus::Waiting));
    let st = p.state();
    let waiting: Vec<&Agent> = st.agents.values().filter(|a| a.status == AgentStatus::Waiting).collect();
    assert!(waiting.iter().all(|a| a.note.as_deref() == Some("waiting for the GPU") && a.ended_ms.is_none()), "{waiting:?}");
    assert_eq!(model.count(), 0, "no model was called while the GPU was held");
    // The screen can say so: the overview counts them as waiting.
    let view = overview(&[&st], &p.registry, fixed_clock());
    let agents = view.rollups.iter().find(|r| r.title == "Agents").expect("an agents rollup");
    assert!(agents.groups.iter().any(|g| g.value == "waiting"), "{:?}", agents.groups.iter().map(|g| &g.value).collect::<Vec<_>>());

    // The GPU is free again: the same employees call the model and end done, with no note left.
    open.store(1, Ordering::SeqCst);
    assert_eq!(join_within(h, 120), RunOutcome::Finished);
    let st = p.state();
    assert!(model.count() > 0);
    assert!(st.agents.values().all(|a| a.status == AgentStatus::Done && a.note.is_none()), "{:?}", st.agents.values().map(|a| (&a.id, a.status, &a.note)).collect::<Vec<_>>());
}

#[test]
fn a_waiting_agent_is_failed_not_left_waiting_when_the_run_is_cancelled() {
    let p = Project::new("held-cancel", Registry::builtin(), &research_plan(1));
    let open = Arc::new(AtomicU64::new(0));
    let cfg = config("mini");
    let e = p.engine(Fake::schema(), Arc::new(HeldForGpu { open, pool: SlotPool::new(2) }), research_tools(), cfg.clone());
    assert!(matches!(join_within(e.spawn().unwrap(), 20), RunOutcome::Parked { .. }));
    let e = p.engine(Fake::schema(), Arc::new(HeldForGpu { open: Arc::new(AtomicU64::new(0)), pool: SlotPool::new(2) }), research_tools(), cfg);
    e.answer("g_1", "approve", None).unwrap();
    let control = e.control();
    let h = e.spawn().unwrap();
    wait_until(60, "an employee to wait", || p.state().agents.values().any(|a| a.status == AgentStatus::Waiting));
    control.cancel();
    assert_eq!(join_within(h, 60), RunOutcome::Cancelled);
    let st = p.state();
    assert!(st.agents.values().all(|a| !matches!(a.status, AgentStatus::Waiting | AgentStatus::Running) && a.note.is_none()), "{:?}", st.agents.values().map(|a| (&a.id, a.status)).collect::<Vec<_>>());
}

/// A pool that remembers which models asked for a slot.
struct Counting(SlotPool, Mutex<Vec<String>>);

impl ModelGate for Counting {
    fn enter(&self, model: &str, cancel: &CancelToken) -> Result<Permit, GateError> {
        self.1.lock().unwrap().push(model.to_string());
        self.0.enter(model, cancel)
    }

    fn width(&self, model: &str) -> usize {
        self.0.width(model)
    }
}

#[test]
fn a_lead_on_its_own_model_is_called_through_its_own_provider_and_slots() {
    let p = Project::new("two-models", Registry::builtin(), &research_plan(2));
    let (employees, lead) = (Fake::schema(), Fake::schema());
    let provider = Arc::new(ByModel::new(employees.clone()).route("dawn", lead.clone()));
    let gate = Arc::new(Counting(SlotPool::per_model(|m| if m == "dawn" { 1 } else { 3 }), Mutex::new(Vec::new())));
    let mut cfg = config("mini");
    cfg.lead_model = "dawn".into();

    let e = p.engine(provider.clone(), gate.clone(), research_tools(), cfg.clone());
    assert!(matches!(join_within(e.spawn().unwrap(), 20), RunOutcome::Parked { .. }));
    let e = p.engine(provider, gate.clone(), research_tools(), cfg);
    e.answer("g_1", "approve", None).unwrap();
    assert_eq!(join_within(e.spawn().unwrap(), 120), RunOutcome::Finished);

    assert!(lead.count() > 0, "the thinking phase's lead steps ran");
    assert!(lead.requests().iter().all(|r| r.model == "dawn"));
    assert!(employees.count() > 0 && employees.requests().iter().all(|r| r.model == "mini"), "employees stayed on theirs");
    let asked = gate.1.lock().unwrap().clone();
    assert!(asked.iter().any(|m| m == "dawn") && asked.iter().any(|m| m == "mini"), "{asked:?}");
    assert_eq!(gate.0.held("dawn") + gate.0.held("mini"), 0);
    // Each agent records the model it ran on.
    let st = p.state();
    assert!(st.agents.values().any(|a| a.model == "dawn") && st.agents.values().any(|a| a.model == "mini"));
}

#[test]
fn numbers_a_dead_run_left_in_the_state_are_cleared_by_the_next_run() {
    let p = Project::new("stale", Registry::builtin(), &research_plan(1));
    p.handle.mutate(Mutation::SetMachine { cpu_pct: 55.0, gpu_pct: 90.0, vram_mb: 12_000 }).unwrap();
    let mut cfg = config("mini");
    cfg.probe = Some(Arc::new(Steady { reads: AtomicUsize::new(0), machine: Machine::default() }));
    // The run parks on the plan card before its first reading is due, and leaves the meters at zero.
    cfg.probe_every = Duration::from_secs(60);
    let e = p.engine(Fake::schema(), Arc::new(SlotPool::new(2)), research_tools(), cfg);
    assert!(matches!(join_within(e.spawn().unwrap(), 20), RunOutcome::Parked { .. }));
    let m = p.state().metrics;
    assert_eq!((m.cpu_pct, m.gpu_pct, m.vram_mb), (0.0, 0.0, 0));
}
