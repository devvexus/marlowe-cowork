//! Project wire round-trips.

use serde_json::json;

use crate::*;

#[test]
fn project_events_round_trip() {
    let events = vec![
        Event::ProjectUpdated {
            project: "p_7f3a91c2".into(),
            base: 4,
            rev: 6,
            patches: vec![
                ProjectPatch::set("/items/i_1a2b3c4d/status", json!("done")),
                ProjectPatch::remove("/items/i_1a2b3c4d/error"),
            ],
        },
        Event::ProjectState { project: "p_7f3a91c2".into(), rev: 6, state: json!({"schema": "marlowe.project/1"}) },
        Event::ProjectGateRaised {
            project: "p_7f3a91c2".into(),
            gate: "g_1".into(),
            kind: "plan".into(),
            summary: "A plan is ready".into(),
            options: vec![ProjectGateOption { id: "approve".into(), label: "Approve".into(), needs_text: false }],
        },
        Event::ProjectGateAnswered { project: "p_7f3a91c2".into(), gate: "g_1".into(), choice: "approve".into() },
        Event::ProjectList {
            projects: vec![ProjectSummary {
                id: "p_7f3a91c2".into(),
                title: "Newest AI models".into(),
                kind: "research".into(),
                status: "awaiting_plan".into(),
                phase: "Plan".into(),
                rev: 3,
                updated_ms: 1_792_756_800_000,
                session: Some("s_1".into()),
            }],
        },
    ];
    for e in events {
        let s = serde_json::to_string(&e).unwrap();
        let back: Event = serde_json::from_str(&s).unwrap();
        assert_eq!(back, e, "round trip changed the event: {s}");
    }
}

#[test]
fn a_remove_patch_carries_no_value_on_the_wire() {
    let s = serde_json::to_string(&ProjectPatch::remove("/items/i_1/error")).unwrap();
    assert_eq!(s, r#"{"op":"remove","path":"/items/i_1/error"}"#);
}

#[test]
fn project_requests_round_trip_and_optional_text_is_omitted() {
    let requests = vec![
        Request::ListProjects,
        Request::GetProject { project: "p_7f3a91c2".into() },
        Request::AnswerProjectGate { project: "p_7f3a91c2".into(), gate: "g_2".into(), choice: "steer".into(), text: Some("look at benchmarks".into()) },
        Request::AnswerProjectGate { project: "p_7f3a91c2".into(), gate: "g_1".into(), choice: "approve".into(), text: None },
        Request::CheckinProject { project: "p_7f3a91c2".into() },
        Request::CancelProject { project: "p_7f3a91c2".into() },
        Request::ReopenProject { project: "p_7f3a91c2".into(), phase: "DataValidation".into() },
    ];
    for r in requests {
        let s = serde_json::to_string(&r).unwrap();
        let back: Request = serde_json::from_str(&s).unwrap();
        assert_eq!(back, r, "round trip changed the request: {s}");
    }
    let s = serde_json::to_string(&Request::AnswerProjectGate { project: "p_1".into(), gate: "g_1".into(), choice: "approve".into(), text: None }).unwrap();
    assert!(!s.contains("text"), "{s}");
    assert_eq!(serde_json::to_string(&Request::ListProjects).unwrap(), r#"{"op":"list_projects"}"#);
}
