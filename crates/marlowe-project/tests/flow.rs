//! The core end to end, through the public API only: Matthew's plan, a state, the store, the
//! actor, a source arriving, a card parking the run, a client following the patches, the screen
//! drawn from the JSON, a snapshot, and a reopen.

use std::time::Duration;

use crossbeam_channel::unbounded;
use marlowe_project::*;
use serde_json::json;

fn temp(tag: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("marlowe-project-flow-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn clock() -> u64 {
    4_000
}

#[test]
fn a_plan_becomes_a_project_that_runs_parks_draws_and_reopens() {
    let home = temp("flow");
    let reg = Registry::builtin();

    // The manager's one tool call, about 250 tokens.
    let plan = parse_plan(
        &json!({
            "objective": "Find the newest and upcoming Kimi, DeepSeek and Qwen models and say what is confirmed and what is rumoured.",
            "questions": ["Which Kimi models are newest or upcoming?", "Is Qwen4 released or announced, and what is known about it?"],
            "fields": [{"key": "model_name"}, {"key": "vendor"}, {"key": "status", "values": ["released", "announced", "rumoured", "unknown"]}],
            "send": true
        }),
        &reg,
        1_000,
    )
    .unwrap();
    let state = ProjectState::new("p_7f3a91c2".into(), plan, &reg, 1_000);
    let store = ProjectStore::open(&home.join("projects"), &home.join("cowork.db")).unwrap();
    store.create(&state).unwrap();

    // What an employee would be forced into: the model never sees a code field.
    let analyze = state.forms.get("analyze@1").unwrap();
    let schema = model_schema(analyze, &state.plan);
    assert!(schema["properties"].get("source").is_none() && schema["properties"].get("credibility").is_none());
    assert_eq!(schema["properties"]["records"]["items"]["properties"].as_object().unwrap().len(), 3);

    let (tx, rx) = unbounded();
    let actor = ProjectActor::spawn(
        ProjectStore::open(&home.join("projects"), &home.join("cowork.db")).unwrap(),
        state.clone(),
        tx,
        ActorConfig { coalesce: Duration::from_secs(3600), save_every: Duration::from_secs(3600), clock },
    )
    .unwrap();
    let h = actor.handle();
    let mut client = Replica::from_state(state.to_value());

    // The plan card parks the project; the user approves.
    h.mutate(Mutation::PushGate(Box::new(Gate {
        id: "g_1".into(),
        kind: GateKind::Plan,
        status: GateStatus::Open,
        payload: json!({"questions": 2}),
        options: GateKind::Plan.options(0),
        answer: None,
        asked_ms: 1_100,
        answered_ms: None,
    phase: String::new(),
    step: String::new(),
    })))
    .unwrap();
    h.mutate(Mutation::AnswerGate { id: "g_1".into(), choice: "approve".into(), text: None }).unwrap();
    h.mutate(Mutation::SetStatus(ProjectStatus::Running)).unwrap();
    h.archive("ph_dc").unwrap();

    // A source is analysed: its data must be exact to its form before it can be `done`.
    let q1 = h.snapshot().unwrap().items_of_kind("question")[0].id.clone();
    let mut src = Item::new("i_5a5a5a5a".into(), "source", "analyze@1");
    src.parent = Some(q1);
    src.status = ItemStatus::Done;
    src.data = json!({"index": 1, "title": "Kimi K3 announced", "relevancy": "Critical", "source_kind": "official", "one_line": "K3 announced",
        "source": "https://example.org/k3", "notes": "n", "credibility": 0.9, "claims": [], "additional_sources": [],
        "records": [{"model_name": "Kimi K3", "vendor": "Moonshot", "status": "announced"}]})
    .as_object()
    .unwrap()
    .clone();
    let refused = h.mutate(Mutation::PutItem(Box::new({
        let mut bad = src.clone();
        bad.data.insert("relevancy".into(), json!("Maybe"));
        bad
    })));
    assert!(matches!(refused, Err(ActorError::Mutation(MutationError::Invalid { .. }))));
    h.mutate(Mutation::PutItem(Box::new(src))).unwrap();
    h.flush().unwrap();

    // The client followed every patch and holds the same document.
    for e in rx.try_iter() {
        if let Some(marlowe_proto::Event::ProjectUpdated { base, rev, patches, .. }) = e.into_wire() {
            client.apply(base, rev, &patches).unwrap();
        }
    }
    let live = h.snapshot().unwrap();
    assert_eq!(client.state, live.to_value());

    // The screen comes from the JSON: the source draws itself under its question.
    let text = paint(&view_tree(&live, 5_000), 100);
    assert!(text.contains("\n  1. Kimi K3 announced   done   [Critical] !"), "{text}");
    let o = overview(&[&live], &reg, 5_000);
    assert_eq!(o.rollups.iter().find(|r| r.title == "Sources").unwrap().groups[0].value, "Critical");

    // Reopen from the snapshot taken before DataCollection ran: no source yet, running again.
    assert!(actor.stop().is_none());
    let reopened = store.reopen("p_7f3a91c2", "ph_dc", 9_000).unwrap();
    assert!(reopened.items_of_kind("source").is_empty());
    assert_eq!(reopened.status, ProjectStatus::Running);
    assert!(reopened.rev > live.rev);
    assert_eq!(store.list(10).unwrap()[0].rev, reopened.rev);
    let _ = std::fs::remove_dir_all(&home);
}
