//! Crew wire round-trips.

use marlowe_model::Effort;
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::*;

fn round_trip<T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(v: &T) -> String {
    let s = serde_json::to_string(v).unwrap();
    let back: T = serde_json::from_str(&s).unwrap();
    assert_eq!(&back, v, "round trip changed the value: {s}");
    s
}

fn rid() -> RunId {
    RunId("0123456789abcdef0123456789abcdef".into())
}

fn dispatch() -> Dispatch {
    Dispatch {
        summary: "Rank 8 matches rank 64 within noise on all three tasks.".into(),
        artifacts: vec!["C:/h/scratch/r/report.md".into()],
        confidence: Confidence::Medium,
        citations: vec![Citation { title: "LoRA".into(), url_or_path: "https://arxiv.org/abs/2106.09685".into() }],
        diff: Some(DiffSummary {
            branch: "crew/01234567".into(),
            files: vec!["src/lib.rs".into()],
            insertions: 12,
            deletions: 3,
        }),
    }
}

fn info(status: CrewRunStatus) -> CrewRunInfo {
    CrewRunInfo {
        id: rid(),
        session: Some(SessionId("s1".into())),
        role: "researcher".into(),
        title: "LoRA rank ablations".into(),
        goal: "What does the literature say about LoRA rank?".into(),
        playbook: Some(Playbook::DeepResearch),
        status,
        workspace: Some("C:/w".into()),
        sandbox: Some(SandboxInfo {
            kind: SandboxKind::Worktree,
            root: "C:/h/worktrees/0123".into(),
            origin: Some("C:/w".into()),
            branch: Some("crew/0123".into()),
        }),
        budget: CrewBudget { tokens: Some(400_000), steps: Some(60), usd: Some(1.5) },
        spent: CrewSpent { tokens: 1200, steps: 3, usd: 0.02 },
        step: 3,
        current_action: Some("scout 2/4 · web_search".into()),
        note: None,
        tainted: false,
        created_ms: 1,
        updated_ms: 2,
        result: Some(dispatch()),
    }
}

fn role() -> RoleInfo {
    RoleInfo {
        name: "scout".into(),
        description: "Answers one sub-question with cited claims.".into(),
        slot: Slot::Crew("scout".into()),
        effort: Effort::Low,
        tools: vec!["web_search".into(), "web_fetch".into()],
        budget: CrewBudget { tokens: Some(120_000), steps: Some(20), usd: None },
        sandbox: SandboxKind::None,
        user_file: None,
    }
}

#[test]
fn crew_requests_round_trip_and_parse_with_defaults() {
    let reqs = vec![
        Request::Dispatch {
            session: Some(SessionId("s1".into())),
            role: "engineer".into(),
            goal: "add a --json flag".into(),
            playbook: Some(Playbook::Build),
            workspace: Some("C:/w".into()),
            budget: Some(CrewBudget { usd: Some(2.0), ..Default::default() }),
        },
        Request::ListRuns { limit: 20 },
        Request::WatchRun { run: rid() },
        Request::Steer { run: rid(), text: "skip the survey papers".into() },
        Request::CancelRun { run: rid() },
        Request::PauseRun { run: rid() },
        Request::ResumeRun { run: rid() },
        Request::MergeRun { run: rid() },
        Request::DiscardRun { run: rid() },
        Request::ListRoles,
    ];
    for r in &reqs {
        let s = round_trip(r);
        assert!(s.starts_with(r#"{"op":""#), "{s}");
    }
    let s = serde_json::to_string(&reqs[0]).unwrap();
    assert!(s.contains(r#""playbook":"build""#), "{s}");

    let r: Request = serde_json::from_str(r#"{"op":"dispatch","role":"scout","goal":"g"}"#).unwrap();
    assert_eq!(
        r,
        Request::Dispatch { session: None, role: "scout".into(), goal: "g".into(), playbook: None, workspace: None, budget: None }
    );
    let r: Request = serde_json::from_str(r#"{"op":"list_roles"}"#).unwrap();
    assert_eq!(r, Request::ListRoles);
}

#[test]
fn crew_events_round_trip_inside_an_envelope() {
    let events = vec![
        Event::RunUpdated(info(CrewRunStatus::Running)),
        Event::RunStep { run: rid(), step: 4, action: "read src/lib.rs".into(), text_delta: Some("Looking".into()) },
        Event::RunStep { run: rid(), step: 4, action: "bash cargo test".into(), text_delta: None },
        Event::RunFinished { run: rid(), dispatch: dispatch() },
        Event::Runs { list: vec![info(CrewRunStatus::NeedsYou), info(CrewRunStatus::Done)] },
        Event::RunTranscript {
            run: info(CrewRunStatus::Paused),
            steps: vec![CrewStep { step: 1, role: "researcher".into(), action: "plan".into(), text: "1. …".into(), at_ms: 5 }],
        },
        Event::Roles { list: vec![role()] },
        Event::ApprovalRequested {
            approval: ApprovalId(2),
            session: SessionId("s1".into()),
            tool: "bash".into(),
            summary: "git push".into(),
            detail: "{}".into(),
            run: Some(rid()),
        },
    ];
    for (i, event) in events.into_iter().enumerate() {
        let env = Envelope { seq: i as u64 + 1, at_ms: 7, event };
        let s = round_trip(&env);
        assert!(s.contains(r#""ev":""#), "{s}");
    }
    let s = serde_json::to_string(&Event::RunUpdated(info(CrewRunStatus::NeedsYou))).unwrap();
    assert!(s.starts_with(r#"{"ev":"run_updated","#), "{s}");
    assert!(s.contains(r#""status":"needs_you""#), "{s}");
}

#[test]
fn an_approval_without_a_run_keeps_its_old_shape() {
    let e = Event::ApprovalRequested {
        approval: ApprovalId(1),
        session: SessionId("s1".into()),
        tool: "bash".into(),
        summary: "ls".into(),
        detail: "{}".into(),
        run: None,
    };
    let s = round_trip(&e);
    assert!(!s.contains("\"run\""), "{s}");
    let old = r#"{"ev":"approval_requested","approval":1,"session":"s1","tool":"bash","summary":"ls","detail":"{}"}"#;
    assert_eq!(serde_json::from_str::<Event>(old).unwrap(), e);
    assert_eq!(e.session(), Some(&SessionId("s1".into())));
    assert_eq!(Event::RunUpdated(info(CrewRunStatus::Queued)).session(), None);
}

#[test]
fn statuses_playbooks_and_budgets_behave() {
    assert!(CrewRunStatus::Done.is_terminal() && CrewRunStatus::Cancelled.is_terminal());
    assert!(!CrewRunStatus::NeedsYou.is_terminal() && !CrewRunStatus::Paused.is_terminal());
    assert_eq!(CrewRunStatus::NeedsYou.as_str(), "needs_you");
    assert_eq!("deep-research".parse::<Playbook>().unwrap(), Playbook::DeepResearch);
    assert_eq!(" Build ".parse::<Playbook>().unwrap(), Playbook::Build);
    assert!("experiment".parse::<Playbook>().is_err());
    assert_eq!(round_trip(&SandboxKind::Worktree), r#""worktree""#);
    assert_eq!(round_trip(&Confidence::High), r#""high""#);

    let role = CrewBudget { tokens: Some(100), steps: Some(10), usd: None };
    let b = role.overlay(CrewBudget { usd: Some(1.0), steps: Some(5), tokens: None });
    assert_eq!(b, CrewBudget { tokens: Some(100), steps: Some(5), usd: Some(1.0) });
    assert_eq!(b.exceeded(&CrewSpent { tokens: 50, steps: 4, usd: 0.5 }), None);
    assert_eq!(b.exceeded(&CrewSpent { tokens: 50, steps: 5, usd: 0.5 }), Some("steps"));
    assert_eq!(b.exceeded(&CrewSpent { tokens: 100, steps: 0, usd: 0.0 }), Some("tokens"));
    assert_eq!(b.exceeded(&CrewSpent { tokens: 0, steps: 0, usd: 1.0 }), Some("usd"));
    assert_eq!(round_trip(&CrewBudget::default()), "{}");

    let id = RunId::new_random();
    assert_eq!(id.as_str().len(), 32);
    assert_eq!(id.short().len(), 8);
}
