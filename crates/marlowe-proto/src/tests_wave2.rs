//! Serde round trips for the wave-2 requests, events and fields (seam2), and the pre-wave-2 JSON
//! they must not break: runs, schedules, todos and questions stored or sent before.

use marlowe_model::{ModelRef, ProviderKind};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::json;

use crate::background::{PlanFileDecision, TaskKind, TaskStatus};
use crate::*;

fn round_trip<T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(v: &T) -> String {
    let s = serde_json::to_string(v).unwrap();
    let back: T = serde_json::from_str(&s).unwrap();
    assert_eq!(&back, v, "round trip changed the value: {s}");
    s
}

fn sid() -> SessionId {
    SessionId("s-1".into())
}

fn task() -> TaskInfo {
    TaskInfo {
        id: "job_j3".into(),
        kind: TaskKind::Job,
        session: Some(sid()),
        title: "cargo test".into(),
        status: TaskStatus::Running,
        started_ms: 10,
        ended_ms: None,
        output_file: Some("C:/h/jobs/j3.log".into()),
        detail: None,
    }
}

fn workflow_info() -> WorkflowInfo {
    WorkflowInfo {
        run: RunId("r-1".into()),
        name: "code-review".into(),
        phases: vec!["find".into(), "verify".into()],
        phase: Some("find".into()),
        agents: vec![WorkflowAgentInfo {
            index: 2,
            label: "lens: reuse".into(),
            phase: Some("find".into()),
            status: WorkflowAgentStatus::Running,
            tokens: 1200,
            elapsed_ms: 4000,
        }],
        log: vec!["3 candidates".into()],
        budget_total: Some(500_000),
        budget_spent: 12_000,
    }
}

fn finding() -> Finding {
    Finding {
        lens: "correctness".into(),
        severity: FindingSeverity::High,
        file: "src/lib.rs".into(),
        line: Some(42),
        claim: "off by one".into(),
        evidence: "i <= len".into(),
        fix: Some("use <".into()),
    }
}

#[test]
fn every_wave2_request_round_trips_and_is_tagged_by_op() {
    let requests = vec![
        (Request::ListTasks { session: Some(sid()) }, "list_tasks"),
        (Request::StopTask { id: "wf_ab12".into() }, "stop_task"),
        (Request::TaskOutput { id: "job_j3".into(), since: Some(120), filter: Some("^error".into()) }, "task_output"),
        (
            Request::CronCreate {
                session: Some(sid()),
                cron: Some("*/5 * * * *".into()),
                once_at_ms: None,
                text: "check CI".into(),
                durable: true,
            },
            "cron_create",
        ),
        (Request::NotifySettings { toast: Some(false), voice: None }, "notify_settings"),
        (Request::OpenFile { path: "C:/h/outbox/s-1/plot.png".into(), reveal: true }, "open_file"),
        (Request::ListWorkflows, "list_workflows"),
        (Request::WatchWorkflow { run: RunId("r-1".into()) }, "watch_workflow"),
        (
            Request::RunWorkflow {
                session: Some(sid()),
                name: Some("code-review".into()),
                script_path: None,
                args: json!({"level": "medium"}),
                resume_from: Some(RunId("r-0".into())),
            },
            "run_workflow",
        ),
        (Request::SkipWorkflowAgent { run: RunId("r-1".into()), agent: 2 }, "skip_workflow_agent"),
        (Request::ListFindings { run: Some(RunId("r-1".into())), session: None }, "list_findings"),
        (Request::SetFast { session: sid(), on: true }, "set_fast"),
        (
            Request::AnswerPlan {
                session: sid(),
                id: "p1".into(),
                decision: PlanFileDecision::Approve { mode: PermissionMode::AcceptEdits },
            },
            "answer_plan",
        ),
        (Request::SuggestRules { workspace: Some("C:/w".into()) }, "suggest_rules"),
    ];
    for (r, op) in &requests {
        let s = round_trip(r);
        assert!(s.contains(&format!(r#""op":"{op}""#)), "{s}");
    }
    // Optional fields may be left out by a client.
    let r: Request = serde_json::from_str(r#"{"op":"list_tasks"}"#).unwrap();
    assert_eq!(r, Request::ListTasks { session: None });
    let r: Request = serde_json::from_str(r#"{"op":"cron_create","text":"later","once_at_ms":5}"#).unwrap();
    assert_eq!(r, Request::CronCreate { session: None, cron: None, once_at_ms: Some(5), text: "later".into(), durable: false });
    let r: Request = serde_json::from_str(r#"{"op":"run_workflow","name":"simplify"}"#).unwrap();
    assert!(matches!(r, Request::RunWorkflow { args: serde_json::Value::Null, resume_from: None, .. }));
    let r: Request = serde_json::from_str(r#"{"op":"open_file","path":"x"}"#).unwrap();
    assert_eq!(r, Request::OpenFile { path: "x".into(), reveal: false });
    // A plan answer names its decision.
    let s = serde_json::to_string(&PlanFileDecision::Reject { feedback: None }).unwrap();
    assert_eq!(s, r#"{"decision":"reject"}"#);
}

#[test]
fn every_wave2_event_round_trips_inside_an_envelope_and_names_its_session() {
    let events: Vec<(Event, Option<SessionId>)> = vec![
        (Event::Tasks { list: vec![task()] }, None),
        (Event::TaskUpdated(task()), Some(sid())),
        (Event::TaskUpdated(TaskInfo { session: None, ..task() }), None),
        (Event::TaskOutput { id: "job_j3".into(), text: "ok\n".into(), next_since: 3, done: true }, None),
        (
            Event::WorkflowList {
                saved: vec![SavedWorkflow {
                    name: "code-review".into(),
                    description: "Review the diff".into(),
                    when_to_use: Some("before a PR".into()),
                    phases: vec!["find".into()],
                    source: "builtin".into(),
                }],
                runs: vec![workflow_info()],
            },
            None,
        ),
        (Event::WorkflowUpdated(workflow_info()), None),
        (Event::WorkflowLog { run: RunId("r-1".into()), line: "3 candidates".into() }, None),
        (Event::Findings { session: Some(sid()), run: Some(RunId("r-1".into())), findings: vec![finding()] }, Some(sid())),
        (Event::Findings { session: None, run: Some(RunId("r-1".into())), findings: vec![] }, None),
        (
            Event::Notified { session: Some(sid()), title: "Training finished".into(), body: "loss 0.31".into(), urgency: Urgency::Urgent },
            Some(sid()),
        ),
        (Event::NotifyState(NotifyState { toast: true, voice: false }), None),
        (
            Event::FileSent {
                session: sid(),
                file: SentFile {
                    name: "plot.png".into(),
                    path: "C:/h/outbox/s-1/plot.png".into(),
                    bytes: 2048,
                    digest: "ab".repeat(32),
                    note: "loss curve".into(),
                },
            },
            Some(sid()),
        ),
        (Event::PlanReady { session: sid(), id: "p1".into(), path: "C:/h/plans/s-1-fix.md".into(), text: "1. fix".into() }, Some(sid())),
        (
            Event::FastChanged {
                session: sid(),
                state: FastState { on: true, model: Some(ModelRef::new(ProviderKind::Ollama, "marlowe-dusk:27b-super")) },
            },
            Some(sid()),
        ),
        (
            Event::RuleSuggestions {
                list: vec![RuleSuggestion { pattern: "bash(git status*)".into(), count: 5, example: "git status".into() }],
            },
            None,
        ),
    ];
    for (event, session) in events {
        assert_eq!(event.session().cloned(), session, "{event:?}");
        let s = round_trip(&Envelope { seq: 9, at_ms: 1, event });
        assert!(s.contains(r#""ev":""#), "{s}");
    }
}

#[test]
fn task_ids_are_typed_and_statuses_know_when_they_are_over() {
    for k in TaskKind::ALL {
        let id = k.id("x1");
        assert!(id.starts_with(k.prefix()));
        assert_eq!(TaskKind::split(&id), Some((k, "x1")));
        assert_eq!(serde_json::to_string(&k).unwrap(), format!("\"{}\"", k.as_str()));
    }
    assert_eq!(TaskKind::Job.prefix(), "job_");
    assert_eq!(TaskKind::Agent.prefix(), "agent_");
    assert_eq!(TaskKind::Workflow.prefix(), "wf_");
    assert_eq!(TaskKind::Monitor.prefix(), "mon_");
    assert_eq!(TaskKind::Cron.prefix(), "cron_");
    assert_eq!(TaskKind::split("job_"), None);
    assert_eq!(TaskKind::split("bogus_1"), None);
    for (s, over) in [
        (TaskStatus::Running, false),
        (TaskStatus::Waiting, false),
        (TaskStatus::Done, true),
        (TaskStatus::Failed, true),
        (TaskStatus::Stopped, true),
    ] {
        assert_eq!(s.is_terminal(), over);
        assert_eq!(serde_json::to_string(&s).unwrap(), format!("\"{}\"", s.as_str()));
    }
}

#[test]
fn a_question_carries_option_details_and_an_old_one_still_loads() {
    let q = Question {
        question: "Which test runner?".into(),
        header: "Runner".into(),
        options: vec!["nextest".into(), "cargo test".into()],
        multi_select: false,
        option_details: vec![
            QuestionOption { label: "nextest".into(), description: "Faster, per-test processes".into(), preview: Some("cargo nextest run".into()) },
            QuestionOption { label: "cargo test".into(), description: String::new(), preview: None },
        ],
    };
    let s = round_trip(&q);
    assert!(s.contains("option_details") && s.contains("cargo nextest run"));
    // Without details the field is not written, so an old client sees what it always saw.
    let plain = Question { option_details: Vec::new(), ..q };
    assert!(!round_trip(&plain).contains("option_details"));
    // A question from before wave 2.
    let old: Question = serde_json::from_str(r#"{"question":"Which?","header":"Pick","options":["a","b"],"multi_select":true}"#).unwrap();
    assert_eq!((old.options.len(), old.multi_select, old.option_details.len()), (2, true, 0));
    let bare: Question = serde_json::from_str(r#"{"question":"Which?"}"#).unwrap();
    assert!(bare.options.is_empty() && bare.option_details.is_empty());
}

#[test]
fn a_schedule_gains_cron_one_shot_durable_and_missed_and_an_old_row_still_loads() {
    let old: ScheduleInfo = serde_json::from_str(r#"{"id":"sch-1","every_secs":60,"text":"/ci","next_at_ms":5}"#).unwrap();
    assert_eq!(old, ScheduleInfo { id: "sch-1".into(), every_secs: 60, text: "/ci".into(), next_at_ms: Some(5), ..Default::default() });
    assert_eq!(
        serde_json::to_string(&old).unwrap(),
        r#"{"id":"sch-1","every_secs":60,"text":"/ci","next_at_ms":5}"#,
        "an interval schedule is written exactly as before"
    );
    let cron = ScheduleInfo {
        id: "cron_9".into(),
        session: Some(sid()),
        every_secs: 0,
        text: "run the nightly eval".into(),
        next_at_ms: Some(99),
        cron: Some("0 3 * * *".into()),
        once_at_ms: None,
        durable: true,
        missed: 2,
    };
    let s = round_trip(&cron);
    assert!(s.contains(r#""cron":"0 3 * * *""#) && s.contains(r#""durable":true"#) && s.contains(r#""missed":2"#), "{s}");
    let once = ScheduleInfo { once_at_ms: Some(7), cron: None, durable: false, missed: 0, ..cron };
    round_trip(&once);
}

#[test]
fn a_todo_gains_task_list_fields_and_an_old_item_still_loads() {
    let old: TodoItem = serde_json::from_str(r#"{"content":"write tests","status":"in_progress","active_form":"Writing tests"}"#).unwrap();
    assert_eq!(old.id, None);
    assert!(old.blocked_by.is_empty() && old.owner.is_none() && old.description.is_empty());
    assert_eq!(
        serde_json::to_string(&old).unwrap(),
        r#"{"content":"write tests","status":"in_progress","active_form":"Writing tests"}"#,
        "a todo{{items}} entry is written exactly as before"
    );
    let item = TodoItem {
        content: "wire the parser".into(),
        status: TodoStatus::Pending,
        active_form: None,
        id: Some("t3".into()),
        description: "after the lexer".into(),
        owner: Some("agent_2".into()),
        blocked_by: vec!["t1".into(), "t2".into()],
    };
    round_trip(&item);
    assert_eq!(TodoItem::default().status, TodoStatus::Pending);
}

/// `crew_runs.record` and `RunUpdated` from before workflows have no `workflow`.
#[test]
fn a_run_names_its_workflow_and_an_old_run_still_loads() {
    let old = json!({
        "id": "abcd1234-0000", "role": "engineer", "title": "Fix the parser", "goal": "fix it",
        "status": "running", "budget": {}, "spent": {}, "step": 3, "created_ms": 1, "updated_ms": 2
    });
    let info: CrewRunInfo = serde_json::from_value(old.clone()).unwrap();
    assert_eq!(info.workflow, None);
    assert!(!serde_json::to_string(&info).unwrap().contains("workflow"), "a plain run is written exactly as before");

    let wf = WorkflowRef {
        name: Some("code-review".into()),
        script_path: "C:/h/workflows/runs/r-1/script.star".into(),
        digest: "cd".repeat(32),
        args: json!({"level": "medium", "paths": ["src"]}),
        foreground: true,
        resumed_from: Some(RunId("r-0".into())),
    };
    let info = CrewRunInfo { workflow: Some(wf.clone()), ..info };
    let s = round_trip(&info);
    assert!(s.contains(r#""workflow":{"name":"code-review""#), "{s}");
    // Args are kept verbatim: null args are not written, an inline script has no name.
    let inline = WorkflowRef { name: None, args: serde_json::Value::Null, foreground: false, resumed_from: None, ..wf };
    let s = round_trip(&inline);
    assert!(!s.contains("args") && !s.contains("name") && !s.contains("resumed_from"), "{s}");
}

#[test]
fn severities_order_and_names_are_stable() {
    assert!(FindingSeverity::Critical > FindingSeverity::High && FindingSeverity::High > FindingSeverity::Medium);
    assert!(FindingSeverity::Info < FindingSeverity::Low);
    assert_eq!(serde_json::to_string(&FindingSeverity::Critical).unwrap(), "\"critical\"");
    assert_eq!(serde_json::to_string(&Urgency::Normal).unwrap(), "\"normal\"");
    assert!(Urgency::Urgent > Urgency::Normal);
    // Both notification switches start on.
    assert_eq!(NotifyState::default(), NotifyState { toast: true, voice: true });
    // A finding without a line or a fix.
    let f: Finding = serde_json::from_str(r#"{"lens":"reuse","severity":"low","file":"a.rs","claim":"dup"}"#).unwrap();
    assert_eq!((f.line, f.fix, f.evidence.as_str()), (None, None, ""));
}
