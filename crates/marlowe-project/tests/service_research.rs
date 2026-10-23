//! The manager's whole path on the recorded research world: the plan the manager would write
//! for a natural request, the card that parks the project, the approval, the lead's checkin, the
//! report, and the words the conversation is given with the path to the report. The real
//! research functions and the real scheduler run; the models are recorded replies.

mod common;
mod world;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::*;
use marlowe_project::engine::SlotPool;
use marlowe_project::research;
use marlowe_project::*;
use serde_json::{json, Value};
use world::*;

struct Host {
    rig: Rig,
    model: Arc<Fake>,
    pool: SlotPool,
    ended: Mutex<Vec<Ended>>,
    gates: AtomicUsize,
}

impl ProjectHost for Host {
    fn parts(&self, _state: &ProjectState) -> Result<RunParts, String> {
        Ok(RunParts { provider: self.model.clone(), gate: Arc::new(self.pool.clone()), toolbox: Arc::new(research::toolbox(&self.rig.deps)), config: config("mini") })
    }

    fn event(&self, event: ProjectEvent) {
        if matches!(event, ProjectEvent::GateRaised { .. }) {
            self.gates.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn ended(&self, ended: Ended) {
        self.ended.lock().unwrap().push(ended);
    }
}

fn setup(tag: &str, lead: &'static str) -> (Home, Arc<Host>, ProjectService) {
    let w = World::load();
    let home = Home::new(tag);
    let host = Arc::new(Host { rig: rig(&w, &home.0), model: model(&w, lead), pool: SlotPool::new(3), ended: Mutex::new(Vec::new()), gates: AtomicUsize::new(0) });
    let cfg = ServiceConfig { root: home.0.join("projects"), db: Some(home.0.join("cowork.db")), registry: Arc::new(Registry::builtin()), actor: actor_cfg() };
    let svc = ProjectService::start(cfg, host.clone()).unwrap();
    (home, host, svc)
}

/// What a 9b manager writes for "research the newest Kimi, DeepSeek and Qwen models and tell me
/// what is confirmed and what is rumoured": the objective, the questions, the fields.
fn manager_plan() -> Value {
    json!({
        "objective": "Find the newest and upcoming Kimi, DeepSeek and Qwen models (including Qwen4) and say what is confirmed and what is rumoured.",
        "questions": [
            "Which Kimi models (Moonshot AI) are newest or upcoming?",
            "Which DeepSeek models are newest or upcoming?",
            "Is Qwen4 released or announced, and what is known about it?"
        ],
        "fields": [{"key": "model_name"}, {"key": "vendor"}, {"key": "status", "values": ["released", "announced", "rumoured", "unknown"]}, {"key": "release_date"}, {"key": "size"}],
        "send": true
    })
}

fn parked(svc: &ProjectService, id: &str) {
    wait_until(120, "the project to park or end", || !svc.is_running(id));
}

#[test]
fn a_natural_request_becomes_a_parked_plan_then_a_report_and_the_conversation_gets_the_path() {
    let (_home, host, svc) = setup("svc-research", "report");
    let t = Instant::now();
    let made = svc.create(Some("s_matthew"), None, &manager_plan()).unwrap();
    assert!(t.elapsed() < Duration::from_secs(2), "create returns at once");
    parked(&svc, &made.id);
    // Nothing was read or asked before approval.
    assert_eq!((host.model.count(), host.rig.web.queries().len(), host.rig.pages.requests().len()), (0, 0, 0));
    assert!(made.card.contains("Which Kimi models") && made.card.contains("and 60 page reads"), "{}", made.card);

    svc.answer(&made.id, None, "approve", None).unwrap();
    // The conversation stays free while the project works: status and list answer at once.
    let t = Instant::now();
    let _ = svc.status(&made.id).unwrap();
    let _ = svc.list().unwrap();
    assert!(t.elapsed() < Duration::from_secs(1));

    // The lead says report, but flagged sources exist, so it asks the user once; the user keeps
    // researching nothing further and approves.
    let mut cards = Vec::new();
    for _ in 0..4 {
        parked(&svc, &made.id);
        let st = svc.state(&made.id).unwrap();
        if st.status.is_terminal() {
            break;
        }
        let g = st.open_gate().expect("a card");
        cards.push(g.payload["reason"].as_str().unwrap_or("").to_string());
        let status = svc.status(&made.id).unwrap();
        assert!(status.contains("Waiting for the user: card") && status.contains("Recommended: keep_researching"), "{status}");
        svc.answer(&made.id, None, "approve", None).unwrap();
    }
    assert_eq!(cards, ["lead_decision"], "one checkin");
    let st = svc.state(&made.id).unwrap();
    assert_eq!(st.status, ProjectStatus::Finished);

    wait_until(10, "the host to hear of the end", || !host.ended.lock().unwrap().is_empty());
    let ended = host.ended.lock().unwrap().clone();
    assert_eq!(ended.len(), 1, "one relay");
    let e = &ended[0];
    assert_eq!((e.session.as_deref(), e.status, e.tainted), (Some("s_matthew"), ProjectStatus::Finished, true));
    assert!(e.text.starts_with(&format!("Project {} finished", made.id)), "{}", e.text);
    let path = e.text.lines().find_map(|l| l.strip_prefix("Report: ")).expect("the path to the report");
    let report = std::fs::read_to_string(path).expect("the report exists at the path the relay names");
    assert!(report.contains("Qwen4") || report.contains("Kimi"), "{}", &report[..report.len().min(300)]);
    assert!(e.text.contains("send it with send_file") && e.text.contains("web pages") && e.text.contains("project.json"), "{}", e.text);
    assert!(e.text.contains("confidence"), "{}", e.text);
    // The relay is due until the conversation has it, then it is not.
    assert_eq!(svc.relays_due().len(), 1);
    svc.mark_relayed(&made.id).unwrap();
    assert!(svc.relays_due().is_empty());
    // Finished projects answer `status` with the result path, and `list` shows them.
    assert!(svc.status(&made.id).unwrap().contains(&format!("Result: {path}")));
    assert!(svc.list().unwrap().contains("finished"));
    // At least the plan card and the checkin card were announced.
    assert!(host.gates.load(Ordering::SeqCst) >= 2);
}

#[test]
fn keep_researching_on_the_checkin_runs_another_round_through_the_service() {
    let (_home, host, svc) = setup("svc-research-keep", "report");
    let id = svc.create(None, None, &manager_plan()).unwrap().id;
    parked(&svc, &id);
    svc.answer(&id, None, "approve", None).unwrap();
    parked(&svc, &id);
    let st = svc.state(&id).unwrap();
    assert_eq!(st.open_gate().map(|g| g.kind), Some(GateKind::Checkin));
    svc.answer(&id, None, "keep_researching", None).unwrap();
    // Round two ends at its own card (the model recorded `report`) or finishes; either way the
    // project advanced to a second round.
    parked(&svc, &id);
    let st = svc.state(&id).unwrap();
    assert!(st.phases.iter().any(|p| p.round == 2), "{:?}", st.phases.iter().map(|p| p.id.clone()).collect::<Vec<_>>());
    assert!(host.model.count() > 0);
    // Cancel what is left; a cancelled project is not relayed.
    if !st.status.is_terminal() {
        svc.cancel(&id).unwrap();
        assert!(host.ended.lock().unwrap().is_empty());
    }
}
