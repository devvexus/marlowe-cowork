//! The phase engine end to end on fake models: the real research menu, run by the real
//! scheduler, with every model answer coming from the form's own schema and every code step a
//! small fake. Nothing here needs a model, a network or a GPU.

mod common;

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use common::*;
use marlowe_project::engine::SlotPool;
use marlowe_project::*;

fn debug(st: &ProjectState) -> String {
    let phases: Vec<String> = st.phases.iter().map(|p| format!("{} {:?} {:?}", p.id, p.status, p.steps.iter().map(|s| format!("{}:{:?}:{}/{}/{}/{}", s.id, s.status, s.counts.queued, s.counts.running, s.counts.done, s.counts.failed)).collect::<Vec<_>>())).collect();
    let gates: Vec<String> = st.gates.iter().map(|g| format!("{} {:?} {} {}", g.id, g.status, g.payload["reason"], g.step)).collect();
    let failed: Vec<String> = st.items.values().filter(|i| i.status == ItemStatus::Failed).map(|i| format!("{} {:?}", i.kind, i.error)).collect();
    format!("status {:?}
{}
gates {:?}
failed {:?}", st.status, phases.join("
"), gates, failed)
}

fn model_calls_of(st: &ProjectState) -> usize {
    st.agents.len()
}

#[test]
fn the_research_menu_parks_on_its_plan_card_then_runs_to_a_finished_project() {
    let p = Project::new("e2e", Registry::builtin(), &research_plan(3));
    let model = Fake::schema();
    let pool = SlotPool::new(3);
    let mk = || p.engine(model.clone(), Arc::new(pool.clone()), research_tools(), config("mini"));

    // The first run builds the plan card and parks: no model was asked, no slot is held, and the
    // thread that ran it has ended.
    let e = mk();
    let h = e.spawn().unwrap();
    assert_eq!(join_within(h, 20), RunOutcome::Parked { gate: "g_1".into() });
    assert_eq!((model.count(), pool.held("mini"), pool.calls()), (0, 0, 0));
    let st = p.state();
    assert_eq!(st.status, ProjectStatus::AwaitingPlan);
    let gate = st.open_gate().unwrap();
    assert_eq!((gate.kind, gate.phase.as_str(), gate.step.as_str()), (GateKind::Plan, "ph_plan", "plan_card"));
    assert_eq!(gate.payload["questions"].as_array().unwrap().len(), 3);

    // The user approves; the run goes on to the end.
    let e = mk();
    e.answer("g_1", "approve", None).unwrap();
    assert_eq!(join_within(e.spawn().unwrap(), 120), RunOutcome::Finished, "state: {}", debug(&p.state()));

    let st = p.state();
    assert_eq!(st.status, ProjectStatus::Finished);
    assert!(st.phases.iter().all(|ph| ph.status == StageStatus::Done), "{:?}", st.phases.iter().map(|p| (&p.id, p.status)).collect::<Vec<_>>());
    // The network grant of the approval is in place.
    let net = st.grants.net.as_ref().unwrap();
    assert_eq!((net.scope.as_str(), net.quota, net.public_https_get, net.robots), ("run", 60, true, true));
    assert_eq!(st.plans[0].decision.as_deref(), Some("approve"));

    // Fan-out and pipelining: 3 questions, 3 vector items, 6 pages, 6 sources, 6 claims.
    let count = |k: &str| st.items_of_kind(k).len();
    assert_eq!((count("question"), count("vector"), count("page"), count("source"), count("claim"), count("cclaim")), (3, 3, 6, 6, 6, 6));
    assert!(st.items_of_kind("source").iter().all(|s| s.status == ItemStatus::Done));
    assert_eq!((count("decision"), count("section"), count("answer")), (1, 3, 1));

    // Every employee is a recorded agent, assigned to the node it worked on: so the screen can show it.
    assert_eq!(model_calls_of(&st), model.count(), "one call per valid employee");
    assert!(model.count() >= 3 + 6 + 3 + 6 + 1 + 3 + 1 + 1);
    for s in st.items_of_kind("source") {
        assert_eq!(s.agents.len(), 1, "{}", s.label);
        let a = &st.agents[&s.agents[0]];
        assert_eq!((a.role.as_str(), a.model.as_str(), a.status, a.attempt), ("analyst", "mini", AgentStatus::Done, 1));
        assert!(a.tokens.input > 0);
        assert!(s.parent.as_deref().is_some_and(|q| st.items[q].kind == "question"), "a source hangs under its question");
        // The page it read also lists the analyst.
        assert!(st.items[a.item.as_ref().unwrap()].agents.contains(&a.id));
        // Code-filled fields were merged in next to the model's.
        assert!(s.data["index"].as_u64().unwrap() >= 1 && s.data["source"].as_str().unwrap().starts_with("https://example.org/"));
        assert!(s.live.started_ms.is_some() && s.live.ended_ms.is_some());
    }
    // The model never saw a code field; it could not have written one.
    for r in model.requests() {
        assert!(r.tools.is_empty());
        let f = r.format.unwrap();
        assert!(f["properties"].get("source").is_none() && f["properties"].get("credibility").is_none());
    }
    // A phase's declared output was validated and stored.
    let dv = st.phase("ph_dv").unwrap();
    assert_eq!(dv.output.as_ref().unwrap()["coverage"]["claims"], 6);
    assert_eq!(dv.progress.done, dv.progress.total);
    // Snapshots: one when each phase was about to start, and the final one.
    let labels: Vec<String> = p.home.store().snapshots(&p.id).unwrap().into_iter().map(|s| s.label).collect();
    for want in ["ph_dc", "ph_dv", "ph_think", "ph_report", "ph_finish", "final"] {
        assert!(labels.contains(&want.to_string()), "{labels:?}");
    }
    // The slot pool was respected throughout and is empty at the end.
    assert!(pool.peak("mini") <= 3 && pool.held("mini") == 0);
    // The screen is drawn from the JSON: a source draws with its agent chip, no source-specific code.
    let rows = view_tree(&st, fixed_clock());
    let text = paint(&rows, 100);
    assert!(text.contains("analyst"), "{text}");
    assert!(st.metrics.tokens.input > 0 && st.metrics.tokens.out > 0);
}

#[test]
fn a_parked_project_survives_a_restart_and_resumes_in_place() {
    let p = Project::new("restart", Registry::builtin(), &research_plan(2));
    let model = Fake::schema();
    let gate: Arc<dyn engine::ModelGate> = Arc::new(SlotPool::new(2));
    let e = p.engine(model.clone(), gate.clone(), research_tools(), config("mini"));
    assert!(matches!(join_within(e.spawn().unwrap(), 20), RunOutcome::Parked { .. }));
    let id = p.id.clone();
    let reg = (*p.registry).clone();
    let home = p.stop();

    // A new process: load project.json, start a new actor and a new engine.
    let state = home.store().load(&id).unwrap();
    assert!(state.open_gate().is_some(), "the card was written to disk");
    let p = Project::attach(home, state, reg);
    let e = p.engine(model.clone(), gate.clone(), research_tools(), config("mini"));
    // Running again parks on the same card at once: no thread, no call.
    assert_eq!(join_within(e.spawn().unwrap(), 20), RunOutcome::Parked { gate: "g_1".into() });
    assert_eq!(model.count(), 0);
    let e = p.engine(model.clone(), gate, research_tools(), config("mini"));
    // A choice that is not on the card is refused and leaves it open.
    assert!(e.answer("g_1", "shrug", None).is_err());
    e.answer("g_1", "approve", None).unwrap();
    assert_eq!(join_within(e.spawn().unwrap(), 120), RunOutcome::Finished);
    assert!(model.count() > 0);
}

#[test]
fn spawning_returns_before_the_model_is_called_and_the_slot_limit_holds() {
    let p = Project::new("slots", Registry::builtin(), &research_plan(3));
    // A model that takes a moment per call, so calls overlap.
    let model = Fake::with(|_, _, _| {
        std::thread::sleep(Duration::from_millis(15));
        Say::Schema
    });
    let pool = SlotPool::new(2);
    let gate: Arc<dyn engine::ModelGate> = Arc::new(pool.clone());
    let e = p.engine(model.clone(), gate.clone(), research_tools(), config("mini"));
    assert!(matches!(join_within(e.spawn().unwrap(), 20), RunOutcome::Parked { .. }));
    let e = p.engine(model.clone(), gate, research_tools(), config("mini"));
    e.answer("g_1", "approve", None).unwrap();
    let started = std::time::Instant::now();
    let h = e.spawn().unwrap();
    let spawn_took = started.elapsed();
    assert!(spawn_took < Duration::from_millis(200), "spawn took {spawn_took:?}: the conversation would have waited");
    assert_eq!(join_within(h, 120), RunOutcome::Finished);
    // Never more than the pool's two at once, and the pool really was used in parallel.
    assert!(model.peak.load(Ordering::SeqCst) <= 2, "peak {}", model.peak.load(Ordering::SeqCst));
    assert_eq!(pool.peak("mini"), 2);
}
