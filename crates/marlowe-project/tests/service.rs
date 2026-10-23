//! The manager's service end to end on fakes: a plan becomes a parked project at once, a card
//! parks it with nothing held, answers resume it, a restart finds it again, and the finished
//! project reaches the host with its words. No network, no model, no GPU.

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::*;
use marlowe_project::engine::SlotPool;
use marlowe_project::*;
use serde_json::{json, Value};

struct Host {
    model: Mutex<Arc<Fake>>,
    pool: SlotPool,
    no_model: AtomicBool,
    ended: Mutex<Vec<Ended>>,
    events: Mutex<Vec<ProjectEvent>>,
}

impl Host {
    fn new(model: Arc<Fake>) -> Arc<Host> {
        Arc::new(Host { model: Mutex::new(model), pool: SlotPool::new(3), no_model: AtomicBool::new(false), ended: Mutex::new(Vec::new()), events: Mutex::new(Vec::new()) })
    }

    fn model(&self) -> Arc<Fake> {
        self.model.lock().unwrap().clone()
    }
}

impl ProjectHost for Host {
    fn parts(&self, _state: &ProjectState) -> Result<RunParts, String> {
        if self.no_model.load(Ordering::SeqCst) {
            return Err("no model is loaded".into());
        }
        Ok(RunParts { provider: self.model(), gate: Arc::new(self.pool.clone()), toolbox: Arc::new(research_tools()), config: config("mini") })
    }

    fn event(&self, event: ProjectEvent) {
        self.events.lock().unwrap().push(event);
    }

    fn ended(&self, ended: Ended) {
        self.ended.lock().unwrap().push(ended);
    }
}

fn service(home: &Home, host: &Arc<Host>) -> ProjectService {
    let cfg = ServiceConfig { root: home.0.join("projects"), db: Some(home.0.join("cowork.db")), registry: Arc::new(Registry::builtin()), actor: actor_cfg() };
    ProjectService::start(cfg, host.clone()).unwrap()
}

fn parked(svc: &ProjectService, id: &str) {
    wait_until(60, "the project to park or end", || !svc.is_running(id));
}

fn plan() -> Value {
    research_plan(2)
}

/// Approves every card until the project ends.
fn run_out(svc: &ProjectService, id: &str) -> ProjectState {
    for _ in 0..8 {
        parked(svc, id);
        let st = svc.state(id).unwrap();
        if st.status.is_terminal() {
            return st;
        }
        svc.answer(id, None, "approve", None).unwrap();
    }
    panic!("the project did not end");
}

#[test]
fn create_returns_at_once_with_the_card_and_the_project_parks_with_nothing_read() {
    let home = Home::new("svc-create");
    let host = Host::new(Fake::with(|_, _, _| {
        std::thread::sleep(Duration::from_secs(3));
        Say::Schema
    }));
    let svc = service(&home, &host);
    let t = Instant::now();
    let made = svc.create(Some("s_1"), None, &plan()).unwrap();
    assert!(t.elapsed() < Duration::from_secs(2), "the conversation never waits: {:?}", t.elapsed());
    assert!(made.id.starts_with("p_") && made.card.contains("Nothing is read before the user approves") && made.card.contains("2. "));
    parked(&svc, &made.id);
    assert_eq!(host.model().count(), 0, "nothing is asked of a model before approval");
    let st = svc.state(&made.id).unwrap();
    assert_eq!((st.status, st.open_gate().map(|g| g.kind)), (ProjectStatus::AwaitingPlan, Some(GateKind::Plan)));
    assert_eq!(st.session.as_deref(), Some("s_1"));
    assert!(svc.status(&made.id).unwrap().contains("Waiting for the user: card g_1 (plan). Answers: approve, cancel"));
    assert!(svc.list().unwrap().contains(&made.id));
    assert!(!svc.is_running(&made.id), "a parked project holds no engine");
    wait_until(5, "the card event", || host.events.lock().unwrap().iter().any(|e| matches!(e, ProjectEvent::GateRaised { kind: GateKind::Plan, .. })));
}

#[test]
fn a_bad_plan_is_refused_with_sentences_the_model_can_act_on_and_nothing_is_written() {
    let home = Home::new("svc-bad");
    let host = Host::new(Fake::schema());
    let svc = service(&home, &host);
    let err = svc.create(None, None, &json!({"objective": "short", "questions": []})).unwrap_err();
    assert_eq!(err.class(), "project_plan");
    let text = err.to_string();
    assert!(text.lines().count() <= 4 && text.contains("plan.questions"), "{text}");
    assert_eq!(svc.list().unwrap(), "No projects yet.");
    // No model: the project is not made either.
    host.no_model.store(true, Ordering::SeqCst);
    let err = svc.create(None, None, &plan()).unwrap_err();
    assert_eq!(err.class(), "project_no_model");
    assert_eq!(svc.list().unwrap(), "No projects yet.");
}

#[test]
fn an_answer_that_is_not_an_option_says_what_the_options_are() {
    let home = Home::new("svc-answer-bad");
    let host = Host::new(Fake::schema());
    let svc = service(&home, &host);
    let id = svc.create(None, None, &plan()).unwrap().id;
    parked(&svc, &id);
    let e = svc.answer(&id, None, "go", None).unwrap_err();
    assert_eq!(e.class(), "project_answer");
    assert!(e.to_string().contains("approve, cancel"), "{e}");
    let e = svc.answer(&id, Some("g_9"), "approve", None).unwrap_err();
    assert!(e.to_string().contains("g_1"), "{e}");
    assert_eq!(svc.answer("p_deadbeef", None, "approve", None).unwrap_err().class(), "project_missing");
    assert_eq!(svc.answer("nonsense", None, "approve", None).unwrap_err().class(), "project_missing");
    assert_eq!(host.model().count(), 0);
}

#[test]
fn approval_runs_the_project_to_its_end_and_the_host_gets_the_words_once() {
    let home = Home::new("svc-run");
    let host = Host::new(Fake::schema());
    let svc = service(&home, &host);
    let id = svc.create(Some("s_9"), None, &plan()).unwrap().id;
    parked(&svc, &id);
    let a = svc.answer(&id, None, "approve", None).unwrap();
    assert!(a.line.contains("running"), "{}", a.line);
    let st = run_out(&svc, &id);
    assert_eq!(st.status, ProjectStatus::Finished, "{:?}", st.status);
    wait_until(10, "the host to hear of the end", || !host.ended.lock().unwrap().is_empty());
    let ended = host.ended.lock().unwrap().clone();
    assert_eq!(ended.len(), 1);
    assert_eq!(ended[0].session.as_deref(), Some("s_9"));
    assert!(ended[0].text.starts_with(&format!("Project {id} finished")) && ended[0].text.contains("project.json"), "{}", ended[0].text);
    assert_eq!(svc.relays_due().len(), 1, "due until the host says it was delivered");
    svc.mark_relayed(&id).unwrap();
    assert!(svc.relays_due().is_empty());
    // Nothing left to answer.
    assert_eq!(svc.answer(&id, None, "approve", None).unwrap_err().class(), "project_state");
}

#[test]
fn a_restart_finds_the_parked_card_and_the_same_answer_resumes_it() {
    let home = Home::new("svc-restart");
    let host = Host::new(Fake::schema());
    let id = {
        let svc = service(&home, &host);
        let id = svc.create(None, None, &plan()).unwrap().id;
        parked(&svc, &id);
        id
    };
    // A new service on the same files: the boot path.
    let host2 = Host::new(Fake::schema());
    let svc2 = service(&home, &host2);
    let rec = svc2.recover();
    assert_eq!((rec.parked.clone(), rec.resumed.len(), rec.stuck.len()), (vec![id.clone()], 0, 0));
    wait_until(5, "the card to be announced again", || host2.events.lock().unwrap().iter().any(|e| matches!(e, ProjectEvent::GateRaised { .. })));
    assert_eq!(host2.model().count(), 0);
    svc2.answer(&id, None, "approve", None).unwrap();
    wait_until(60, "the model to be asked", || host2.model().count() > 0);
}

#[test]
fn a_project_that_was_running_when_the_process_died_resumes_at_boot() {
    let home = Home::new("svc-resume");
    let host = Host::new(Fake::with(|_, _, cancel| {
        // The first process dies while an employee is mid-call.
        while !cancel.is_cancelled() {
            std::thread::sleep(Duration::from_millis(5));
        }
        Say::Schema
    }));
    let id = {
        let svc = service(&home, &host);
        let id = svc.create(None, None, &plan()).unwrap().id;
        parked(&svc, &id);
        svc.answer(&id, None, "approve", None).unwrap();
        wait_until(20, "an employee to be in a call", || host.model().in_flight.load(Ordering::SeqCst) > 0);
        svc.shutdown();
        id
    };
    // A second process finds it unfinished: running again, or parked on a card the shutdown left.
    let host2 = Host::new(Fake::schema());
    let svc2 = service(&home, &host2);
    let rec = svc2.recover();
    assert_eq!(rec.stuck.len(), 0);
    assert!(rec.resumed.contains(&id) || rec.parked.contains(&id), "{rec:?}");
}

#[test]
fn revise_rebuilds_the_card_while_it_is_open_and_not_after() {
    let home = Home::new("svc-revise");
    let host = Host::new(Fake::schema());
    let svc = service(&home, &host);
    let made = svc.create(None, None, &plan()).unwrap();
    parked(&svc, &made.id);
    let r = svc.revise(&made.id, &json!({"questions": ["What is the newest open model from Moonshot AI?"]})).unwrap();
    assert!(r.card.contains("1. What is the newest open model from Moonshot AI?") && !r.card.contains("2. "), "{}", r.card);
    parked(&svc, &made.id);
    let st = svc.state(&made.id).unwrap();
    assert_eq!(st.plan.questions.len(), 1);
    assert_eq!(st.items_of_kind("question").len(), 1, "no question of the old plan is left");
    assert_eq!(st.open_gate().map(|g| g.id.as_str()), Some("g_1"));
    assert_eq!(host.model().count(), 0);
    // A bad change is refused and the card stays.
    let e = svc.revise(&made.id, &json!({"questions": []})).unwrap_err();
    assert_eq!(e.class(), "project_plan");
    assert_eq!(svc.state(&made.id).unwrap().plan.questions.len(), 1);
    svc.answer(&made.id, None, "approve", None).unwrap();
    wait_until(60, "the model to be asked", || host.model().count() > 0);
    parked(&svc, &made.id);
    let e = svc.revise(&made.id, &json!({"questions": ["What is the newest open model from DeepSeek?"]}));
    assert_eq!(e.unwrap_err().class(), "project_state", "the plan cannot change once it is approved");
}

#[test]
fn cancel_ends_a_parked_project_without_a_relay_and_it_cannot_be_answered() {
    let home = Home::new("svc-cancel");
    let host = Host::new(Fake::schema());
    let svc = service(&home, &host);
    let id = svc.create(Some("s_1"), None, &plan()).unwrap().id;
    parked(&svc, &id);
    svc.cancel(&id).unwrap();
    assert_eq!(svc.state(&id).unwrap().status, ProjectStatus::Cancelled);
    assert!(svc.relays_due().is_empty() && host.ended.lock().unwrap().is_empty(), "the user knows");
    assert_eq!(svc.answer(&id, None, "approve", None).unwrap_err().class(), "project_state");
    assert_eq!(svc.cancel(&id).unwrap_err().class(), "project_state");
    // `answer` with cancel is the same thing.
    let id2 = svc.create(None, None, &plan()).unwrap().id;
    parked(&svc, &id2);
    svc.answer(&id2, None, "cancel", None).unwrap();
    assert_eq!(svc.state(&id2).unwrap().status, ProjectStatus::Cancelled);
}

#[test]
fn a_forced_checkin_parks_a_running_project_on_a_card_and_cancel_stops_a_running_one() {
    let home = Home::new("svc-force");
    let release = Arc::new(AtomicBool::new(false));
    let r2 = release.clone();
    let host = Host::new(Fake::with(move |_, _, cancel| {
        while !r2.load(Ordering::SeqCst) && !cancel.is_cancelled() {
            std::thread::sleep(Duration::from_millis(5));
        }
        Say::Schema
    }));
    let svc = service(&home, &host);
    let id = svc.create(None, None, &plan()).unwrap().id;
    parked(&svc, &id);
    assert_eq!(svc.checkin(&id).unwrap_err().class(), "project_state", "a parked project is already waiting");
    let t = Instant::now();
    svc.answer(&id, None, "approve", None).unwrap();
    assert!(t.elapsed() < Duration::from_secs(2), "answering does not wait for the work");
    wait_until(20, "an employee to be in a call", || host.model().in_flight.load(Ordering::SeqCst) > 0);
    assert!(svc.is_running(&id));
    svc.checkin(&id).unwrap();
    release.store(true, Ordering::SeqCst);
    parked(&svc, &id);
    let st = svc.state(&id).unwrap();
    let g = st.open_gate().expect("a checkin card");
    assert_eq!((g.kind, g.payload["reason"].as_str()), (GateKind::Checkin, Some("user_forced")));
    // Keep going, then cancel while it runs.
    release.store(false, Ordering::SeqCst);
    svc.answer(&id, None, "keep_researching", None).unwrap();
    wait_until(20, "an employee to be in a call", || svc.is_running(&id) && host.model().in_flight.load(Ordering::SeqCst) > 0);
    svc.cancel(&id).unwrap();
    parked(&svc, &id);
    assert_eq!(svc.state(&id).unwrap().status, ProjectStatus::Cancelled);
}

#[test]
fn reopen_runs_a_finished_project_again_from_a_stage_and_says_when_there_is_no_such_stage() {
    let home = Home::new("svc-reopen");
    let host = Host::new(Fake::schema());
    let svc = service(&home, &host);
    let id = svc.create(None, None, &plan()).unwrap().id;
    parked(&svc, &id);
    svc.answer(&id, None, "approve", None).unwrap();
    assert_eq!(run_out(&svc, &id).status, ProjectStatus::Finished);
    let e = svc.reopen(&id, "NoSuchStage").unwrap_err();
    assert_eq!(e.class(), "project_state");
    assert!(e.to_string().contains("final"), "{e}");
    let line = svc.reopen(&id, "DataValidation").unwrap();
    assert!(line.contains("reopened from ph_dv"), "{line}");
    parked(&svc, &id);
    let st = svc.state(&id).unwrap();
    assert!(st.status == ProjectStatus::Finished || st.open_gate().is_some(), "{:?}", st.status);
}

#[test]
fn a_project_started_in_a_workspace_leaves_a_pointer_there() {
    let home = Home::new("svc-link");
    let ws = Home::new("svc-link-ws");
    let host = Host::new(Fake::schema());
    let svc = service(&home, &host);
    let id = svc.create(None, Some(&ws.0), &plan()).unwrap().id;
    parked(&svc, &id);
    let ptr = ws.0.join(".marlowe").join("projects").join(format!("{id}.json"));
    let body: Value = serde_json::from_str(&std::fs::read_to_string(&ptr).expect("the pointer")).unwrap();
    let target = std::path::PathBuf::from(body["project"].as_str().unwrap());
    assert!(target.join("project.json").is_file(), "the pointer leads to the project: {target:?}");
}
