//! Serde round trips for every M4.6 request and event, and the pre-M4.6 transcript format.

use marlowe_model::{Effort, ImageRef, ModelRef, Part, ProviderKind, ResultPart, Role, ToolResult, Usage};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::json;

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

#[test]
fn every_m46_request_round_trips_and_is_tagged_by_op() {
    let mref = ModelRef::new(ProviderKind::Anthropic, "claude-x");
    let ws = || Some("C:/w".to_string());
    let requests = vec![
        (Request::SetMode { session: sid(), mode: PermissionMode::AcceptEdits }, "set_mode"),
        (Request::ListRules { workspace: ws() }, "list_rules"),
        (
            Request::AddRule { action: RuleAction::Deny, pattern: "bash(git push*)".into(), scope: SettingsScope::Local, workspace: ws() },
            "add_rule",
        ),
        (Request::RemoveRule { pattern: "edit(src/**)".into(), scope: SettingsScope::Project, workspace: None }, "remove_rule"),
        (Request::ListHooks { workspace: None }, "list_hooks"),
        (Request::TrustHooks { workspace: "C:/w".into(), digest: "ab12".into() }, "trust_hooks"),
        (Request::GetSettings { workspace: ws() }, "get_settings"),
        (
            Request::SetSetting {
                scope: SettingsScope::User,
                key: "network.allow_hosts".into(),
                value: json!(["pypi.org"]),
                workspace: None,
            },
            "set_setting",
        ),
        (Request::Compact { session: sid(), focus: Some("the parser".into()) }, "compact"),
        (Request::AddDir { session: sid(), path: "C:/other".into() }, "add_dir"),
        (Request::SwitchModel { session: sid(), model: mref.clone(), effort: Some(Effort::High) }, "switch_model"),
        (Request::ListCommands { workspace: ws() }, "list_commands"),
        (Request::ListAgentTypes { workspace: None }, "list_agent_types"),
        (Request::AnswerQuestion { session: sid(), id: "q1".into(), answers: vec!["yes".into()] }, "answer_question"),
        (Request::ReloadInstructions { session: sid() }, "reload_instructions"),
        (Request::DetachCall { session: sid(), call_id: "call_1_2_0".into() }, "detach_call"),
        (Request::ListSchedules, "list_schedules"),
        (Request::AddSchedule { session: Some(sid()), every_secs: 600, text: "/check-ci".into() }, "add_schedule"),
        (Request::RemoveSchedule { id: "sch-1".into() }, "remove_schedule"),
        (Request::Rewind { session: sid(), turn: 3, scope: RewindScope::Both }, "rewind"),
        (Request::ListCheckpoints { session: sid() }, "list_checkpoints"),
        (Request::SessionDiff { session: sid(), turn: None }, "session_diff"),
        (Request::SessionCost { session: sid() }, "session_cost"),
        (Request::RenameSession { session: sid(), title: "parser fix".into() }, "rename_session"),
        (Request::ForkSession { session: sid(), at_turn: Some(2) }, "fork_session"),
        (Request::ExportSession { session: sid(), format: ExportFormat::Markdown }, "export_session"),
        (Request::OpenPr { run: RunId("r-1".into()), title: Some("Fix".into()), body: None }, "open_pr"),
        (Request::ListExperiments { limit: 20 }, "list_experiments"),
        (Request::McpList { workspace: ws() }, "mcp_list"),
        (
            Request::McpAdd {
                name: "github".into(),
                scope: SettingsScope::Project,
                config: json!({"command": "gh-mcp", "args": []}),
                workspace: ws(),
            },
            "mcp_add",
        ),
        (Request::McpRemove { name: "github".into(), scope: SettingsScope::User, workspace: None }, "mcp_remove"),
    ];
    for (r, op) in &requests {
        let s = round_trip(r);
        assert!(s.contains(&format!(r#""op":"{op}""#)), "{s}");
    }
    // Optional fields may be left out by a client.
    let r: Request = serde_json::from_str(r#"{"op":"compact","session":"s-1"}"#).unwrap();
    assert_eq!(r, Request::Compact { session: sid(), focus: None });
    // `auto` is accepted for bypass.
    let r: Request = serde_json::from_str(r#"{"op":"set_mode","session":"s-1","mode":"auto"}"#).unwrap();
    assert_eq!(r, Request::SetMode { session: sid(), mode: PermissionMode::Bypass });
}

#[test]
fn every_m46_event_round_trips_inside_an_envelope() {
    let usage = Usage { input: 10, output: 5, reasoning: 0, cached_input: 4, cost_usd: Some(0.01) };
    let events = vec![
        Event::ModeChanged { session: sid(), mode: PermissionMode::Plan },
        Event::Rules {
            rules: vec![RuleInfo {
                action: RuleAction::Allow,
                pattern: "bash(cargo test*)".into(),
                scope: SettingsScope::Local,
                source: Some(".marlowe/settings.local.toml".into()),
            }],
        },
        Event::Hooks {
            hooks: vec![HookInfo {
                event: "PreToolUse".into(),
                matcher: Some("bash".into()),
                command: "check.ps1".into(),
                scope: SettingsScope::Project,
                trusted: false,
            }],
            digest: "ab12".into(),
        },
        Event::Settings { layers: vec![SettingsLayer { scope: SettingsScope::User, path: None, text: "[network]".into() }] },
        Event::Commands {
            list: vec![CommandInfo { name: "review".into(), description: "Review".into(), argument_hint: None, source: "builtin".into() }],
        },
        Event::AgentTypes {
            list: vec![AgentTypeInfo {
                name: "explore".into(),
                description: String::new(),
                model: Some("haiku".into()),
                tools: vec!["read".into()],
                source: "builtin".into(),
            }],
        },
        Event::QuestionAsked {
            session: sid(),
            id: "q1".into(),
            questions: vec![Question { question: "Which?".into(), header: "Pick".into(), options: vec!["a".into()], multi_select: false }],
        },
        Event::Todos {
            session: sid(),
            items: vec![TodoItem { content: "write tests".into(), status: TodoStatus::InProgress, active_form: Some("Writing tests".into()) }],
        },
        Event::Schedules {
            list: vec![ScheduleInfo { id: "sch-1".into(), session: None, every_secs: 60, text: "/ci".into(), next_at_ms: Some(5) }],
        },
        Event::Rewound { session: sid(), turn: 2, scope: RewindScope::Code },
        Event::Checkpoints { session: sid(), list: vec![CheckpointInfo { turn: 1, at_ms: 9, files: 3, summary: "edit".into() }] },
        Event::SessionDiff { session: sid(), turn: Some(1), diff: "--- a\n+++ b\n".into() },
        Event::SessionCost {
            session: sid(),
            report: CostReport {
                calls: 2,
                usage,
                by_model: vec![ModelCost { model: ModelRef::new(ProviderKind::Ollama, "m"), calls: 2, usage }],
            },
        },
        Event::SessionExported { session: sid(), format: ExportFormat::Json, text: "{}".into() },
        Event::PrOpened { run: RunId("r-1".into()), url: "https://github.com/o/r/pull/1".into() },
        Event::Experiments {
            list: vec![ExperimentInfo {
                id: "e1".into(),
                command: "python train.py".into(),
                run_dir: "runs/1".into(),
                status: ExperimentStatus::Running,
                started_ms: 1,
                sha: Some("abc".into()),
                metrics: json!({"loss": 0.5}),
                notes: String::new(),
            }],
        },
        Event::McpServers {
            list: vec![McpServerInfo {
                name: "github".into(),
                scope: SettingsScope::Project,
                transport: "stdio".into(),
                tools: 12,
                status: "connected".into(),
            }],
        },
    ];
    for event in events {
        let session = event.session().cloned();
        let s = round_trip(&Envelope { seq: 7, at_ms: 1, event });
        assert!(s.contains(r#""ev":""#), "{s}");
        if let Some(sess) = session {
            assert_eq!(sess, sid());
        }
    }
}

#[test]
fn modes_print_and_cycle() {
    for m in PermissionMode::ALL {
        assert_eq!(serde_json::to_string(&m).unwrap(), format!("\"{}\"", m.as_str()));
    }
    assert_eq!(PermissionMode::Default.cycle(), PermissionMode::AcceptEdits);
    assert_eq!(PermissionMode::AcceptEdits.cycle(), PermissionMode::Plan);
    assert_eq!(PermissionMode::Plan.cycle(), PermissionMode::Default);
    assert_eq!(PermissionMode::Bypass.cycle(), PermissionMode::Default);
}

/// A `Transcript` exactly as an M4.5 daemon sends it (no `parts`, no images) still loads, and
/// one with an image round-trips.
#[test]
fn pre_m46_transcripts_still_load() {
    let old = r#"{"seq":0,"at_ms":1,"event":{"ev":"transcript","session":"s-1","turns":[
        {"idx":0,"role":"user","content":[{"type":"text","text":"hi"}],"at_ms":1},
        {"idx":1,"role":"assistant","content":[{"type":"tool_call","id":"c1","name":"read","args":{"path":"a"}}],"at_ms":2},
        {"idx":2,"role":"tool","content":[{"type":"tool_result","call_id":"c1","content":"x","is_error":false}],"at_ms":3}
    ]}}"#;
    let env: Envelope = serde_json::from_str(old).unwrap();
    let Event::Transcript { turns, .. } = &env.event else { panic!("not a transcript") };
    assert_eq!(turns.len(), 3);
    assert!(matches!(&turns[2].content[0], Part::ToolResult(r) if r.parts.is_empty()));

    let img = ImageRef { mime: "image/png".into(), blob: "00".repeat(32), w: 10, h: 10 };
    let turn = StoredTurn {
        idx: 3,
        role: Role::Tool,
        content: vec![Part::ToolResult(ToolResult {
            parts: vec![ResultPart::Image(img.clone())],
            ..ToolResult::text("c2", "[image]", false)
        })],
        at_ms: 4,
    };
    round_trip(&Envelope { seq: 0, at_ms: 1, event: Event::Transcript { session: sid(), turns: vec![turn] } });
    let user = StoredTurn { idx: 4, role: Role::User, content: vec![Part::Text { text: "see".into() }, Part::Image(img)], at_ms: 5 };
    round_trip(&user);
}

#[test]
fn the_seam_review_additions_round_trip_and_old_json_still_loads() {
    let img = ImageRef { mime: "image/png".into(), blob: "ab".repeat(32), w: 640, h: 480 };
    let options = HeadlessOptions {
        allowed_tools: vec!["bash(git diff*)".into()],
        disallowed_tools: vec!["web_fetch".into()],
        max_steps: Some(12),
        max_budget_usd: Some(0.5),
        append_system_prompt: Some("Be terse.".into()),
        permission_mode: Some(PermissionMode::DontAsk),
    };
    let requests = vec![
        (
            Request::Ask { session: Some(sid()), text: "what is this?".into(), workspace: None, images: vec![img.clone()], options: Some(options) },
            "ask",
        ),
        (Request::PutBlob { mime: "image/png".into(), base64: "iVBORw0KGgo=".into() }, "put_blob"),
        (Request::ListJobs { session: Some(sid()) }, "list_jobs"),
        (Request::KillJob { id: "job-1".into() }, "kill_job"),
        (Request::UserShell { session: sid(), command: "git status".into() }, "user_shell"),
        (
            Request::Approve { approval: ApprovalId(4), decision: ApprovalDecision::AllowPattern { scope: SettingsScope::Local } },
            "approve",
        ),
    ];
    for (r, op) in &requests {
        let s = round_trip(r);
        assert!(s.contains(&format!("\"op\":\"{op}\"")), "{s}");
    }
    assert!(ApprovalDecision::AllowPattern { scope: SettingsScope::Session }.is_allowed());

    // An M4.5 ask has no images or options.
    let old: Request = serde_json::from_str(r#"{"op":"ask","text":"hi"}"#).unwrap();
    assert_eq!(old, Request::Ask { session: None, text: "hi".into(), workspace: None, images: Vec::new(), options: None });
    assert_eq!(serde_json::to_string(&old).unwrap(), r#"{"op":"ask","text":"hi"}"#, "old output unchanged");
    let old: Event = serde_json::from_str(
        r#"{"ev":"approval_requested","approval":1,"session":"s-1","tool":"bash","summary":"ls","detail":"{}"}"#,
    )
    .unwrap_or_else(|e| panic!("{e}"));
    assert!(matches!(old, Event::ApprovalRequested { proposed_rule: None, .. }));

    let job = JobInfo { id: "job-1".into(), session: Some(sid()), command: "cargo build".into(), started_ms: 1, exit_code: None };
    let events = vec![
        Event::BlobStored { blob: img.blob.clone(), mime: img.mime.clone(), w: 640, h: 480 },
        Event::Jobs { list: vec![job.clone()] },
        Event::JobStarted { job },
        Event::JobOutput { session: Some(sid()), id: "job-1".into(), text: "Compiling".into() },
        Event::JobExited { session: None, id: "job-1".into(), code: 0 },
        Event::ApprovalRequested {
            approval: ApprovalId(4),
            session: sid(),
            tool: "bash".into(),
            summary: "cargo test".into(),
            detail: "{}".into(),
            run: None,
            proposed_rule: Some("bash(cargo test*)".into()),
        },
    ];
    for e in &events {
        round_trip(e);
    }
    assert_eq!(events[2].session(), Some(&sid()));
    assert_eq!(events[3].session(), Some(&sid()));
    for s in [NoticeSource::Ci, NoticeSource::Experiment] {
        round_trip(&s);
    }
    assert_eq!(serde_json::to_string(&NoticeSource::Experiment).unwrap(), "\"experiment\"");
}
