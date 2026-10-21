//! Parity with Claude Code, the panels and slash commands: key-event replay against the requests
//! the daemon would get and the screens the user would see. Rows: U12 `/config`, M10
//! `/permissions`, M11 don't-ask-again, X16 `/hooks` `/agents` `/mcp`, C6 `/memory`, C9
//! `/context`, C14 effort, C15 `/model`, S1 `/resume`, S3 `/diff`, S4 `/cost`, T22 todos, T23
//! `ask_user`, and the session commands (`/rewind`, `/rename`, `/fork`, `/export`, `/compact`,
//! `/add-dir`, `/loop`).
#![allow(non_snake_case)]

mod common;

use common::*;
use marlowe_model::{Effort, Usage};
use marlowe_proto::{
    AgentTypeInfo, ApprovalDecision, ApprovalId, CheckpointInfo, CommandInfo, CostReport, Event, ExportFormat, HookInfo,
    LibraryModel, McpServerInfo, ModelCost, PermissionMode, Question, Request, RewindScope, RuleAction, RuleInfo,
    SessionId, SessionSummary, SettingsScope, Slot, SlotAssignment, TodoItem, TodoStatus,
};
use marlowe_tui::coding::{Effect, InstructionFile, LocalEvent};
use marlowe_tui::state::Overlay;
use marlowe_tui::ClientEvent;
use serde_json::json;

const WS: &str = "C:/work/valiance";

fn ws() -> Option<String> {
    Some(WS.into())
}

fn local(s: &mut marlowe_tui::State, ev: LocalEvent) -> Vec<Request> {
    s.on_client(ClientEvent::Local(ev))
}

// ---- U12 -------------------------------------------------------------------------------------------

#[test]
fn parity_U12_config_lists_the_settings_and_writes_the_layer_you_pick() {
    let mut s = coding_session();
    let reqs = keys(&mut s, "'/config' enter");
    assert!(reqs.contains(&Request::GetSettings { workspace: ws() }), "{reqs:?}");
    assert_eq!(s.overlay, Some(Overlay::Panel));
    feed(
        &mut s,
        0,
        settings_event(&[
            (SettingsScope::User, "editor_url = \"vscode://file/{path}:{line}\"\n[thinking]\nshow = true\n"),
            (SettingsScope::Project, "[statusline]\ncommand = \"echo hi\"\n"),
        ]),
    );
    let buf = palette_frame(&s);
    for want in ["config", "editor_url", "vscode://file/{path}:{line}", "statusline.command", "echo hi", "thinking.show", "on", "not set"] {
        assert!(contains(&buf, want), "{want:?}\n{}", dump(&buf));
    }
    snapshot("config_panel", &buf);
    // Tab picks the layer; the change goes there.
    keys(&mut s, "tab");
    assert!(contains(&palette_frame(&s), "written to the project settings"));
    keys(&mut s, "enter ctrl-u 'cursor://file/{path}:{line}'");
    let reqs = keys(&mut s, "enter");
    assert_eq!(
        reqs,
        vec![Request::SetSetting { scope: SettingsScope::Project, key: "editor_url".into(), value: json!("cursor://file/{path}:{line}"), workspace: ws() }]
    );
    // A boolean toggles on enter.
    let reqs = keys(&mut s, "down down down down down enter");
    assert_eq!(reqs, vec![Request::SetSetting { scope: SettingsScope::Project, key: "thinking.show".into(), value: json!(false), workspace: ws() }]);
    // A list is comma separated.
    keys(&mut s, "up up enter 'pypi.org, crates.io'");
    let reqs = keys(&mut s, "enter");
    assert_eq!(
        reqs,
        vec![Request::SetSetting { scope: SettingsScope::Project, key: "network.allow_hosts".into(), value: json!(["pypi.org", "crates.io"]), workspace: ws() }]
    );
    // x unsets a key that is set.
    keys(&mut s, "up up up");
    let reqs = keys(&mut s, "x");
    assert_eq!(reqs, vec![Request::SetSetting { scope: SettingsScope::Project, key: "editor_url".into(), value: serde_json::Value::Null, workspace: ws() }]);
    keys(&mut s, "esc");
    assert_eq!(s.overlay, None);
}

#[test]
fn parity_U12_config_before_the_daemon_serves_settings_says_so_once() {
    let mut s = coding_session();
    keys(&mut s, "'/config' enter");
    feed(&mut s, 0, Event::Error { session: None, message: "`get_settings` is not available yet (M4.6 track E)".into() });
    let buf = palette_frame(&s);
    assert!(contains(&buf, "does not serve settings yet"), "{}", dump(&buf));
}

// ---- M10, M11 --------------------------------------------------------------------------------------

fn rule(a: RuleAction, p: &str, scope: SettingsScope, src: Option<&str>) -> RuleInfo {
    RuleInfo { action: a, pattern: p.into(), scope, source: src.map(str::to_string) }
}

#[test]
fn parity_M10_permissions_lists_rules_in_the_order_they_bind_and_adds_and_removes() {
    let mut s = coding_session();
    let reqs = keys(&mut s, "'/permissions' enter");
    assert!(reqs.contains(&Request::ListRules { workspace: ws() }));
    feed(
        &mut s,
        0,
        Event::Rules {
            rules: vec![
                rule(RuleAction::Allow, "bash(git diff*)", SettingsScope::User, None),
                rule(RuleAction::Deny, "bash(git push*)", SettingsScope::Project, Some(".claude/settings.json import")),
                rule(RuleAction::Ask, "web_fetch(domain:example.com)", SettingsScope::Local, None),
            ],
        },
    );
    let buf = palette_frame(&s);
    let text = dump(&buf);
    let at = |p: &str| text.find(p).unwrap_or_else(|| panic!("{p:?}\n{text}"));
    let (deny, ask, allow) = (at("bash(git push*)"), at("web_fetch(domain:example.com)"), at("bash(git diff*)"));
    assert!(deny < ask && ask < allow, "deny, then ask, then allow\n{text}");
    assert!(text.contains(".claude/settings.json import"), "which file a rule came from");
    snapshot("permissions_panel", &buf);
    // The rule the daemon says decided a call is marked.
    assert!(!text.contains("fired"), "no rule is marked until the daemon says one decided");
    s.set_rule_fired("bash(git push*)", "denied git push");
    assert!(contains(&palette_frame(&s), "fired denied git push"));
    // Remove the selected rule (the first shown is the deny).
    let reqs = keys(&mut s, "d");
    assert_eq!(reqs, vec![Request::RemoveRule { pattern: "bash(git push*)".into(), scope: SettingsScope::Project, workspace: ws() }]);
    // Add: allow, saved locally by default.
    keys(&mut s, "a 'bash(cargo test*)'");
    assert!(contains(&palette_frame(&s), "new rule"));
    let reqs = keys(&mut s, "enter");
    assert_eq!(
        reqs,
        vec![Request::AddRule { action: RuleAction::Allow, pattern: "bash(cargo test*)".into(), scope: SettingsScope::Local, workspace: ws() }]
    );
    // Arrow keys change the action and tab the layer.
    keys(&mut s, "a 'edit(src/**)' right tab");
    let reqs = keys(&mut s, "enter");
    assert_eq!(reqs, vec![Request::AddRule { action: RuleAction::Deny, pattern: "edit(src/**)".into(), scope: SettingsScope::User, workspace: ws() }]);
    // An empty pattern is not a rule.
    keys(&mut s, "a");
    assert!(keys(&mut s, "enter").is_empty());
}

fn bash_approval(id: u64, rule: Option<&str>) -> Event {
    Event::ApprovalRequested {
        approval: ApprovalId(id),
        session: sid(),
        tool: "bash".into(),
        summary: "run cargo test -p marlowe-tui".into(),
        detail: r#"{"command":"cargo test -p marlowe-tui"}"#.into(),
        run: None,
        proposed_rule: rule.map(str::to_string),
    }
}

#[test]
fn parity_M11_dont_ask_again_offers_the_rule_for_the_session_or_the_project() {
    let mut s = coding_session();
    feed(&mut s, 3, bash_approval(1, Some("bash(cargo test*)")));
    let buf = palette_frame(&s);
    for want in ["always allow  bash(cargo test*)", "always · session", "always · project", "deny", "allow"] {
        assert!(contains(&buf, want), "{want:?}\n{}", dump(&buf));
    }
    snapshot("approval_with_rule", &buf);
    s.now_ms += 1_000;
    assert_eq!(
        keys(&mut s, "a"),
        vec![Request::Approve { approval: ApprovalId(1), decision: ApprovalDecision::AllowPattern { scope: SettingsScope::Session }, reason: None }]
    );
    feed(&mut s, 4, bash_approval(2, Some("bash(cargo test*)")));
    s.now_ms += 1_000;
    assert_eq!(
        keys(&mut s, "p"),
        vec![Request::Approve { approval: ApprovalId(2), decision: ApprovalDecision::AllowPattern { scope: SettingsScope::Local }, reason: None }]
    );
    // The buttons work from the arrow keys too: left from deny twice is the session rule.
    feed(&mut s, 5, bash_approval(3, Some("bash(cargo test*)")));
    s.now_ms += 1_000;
    keys(&mut s, "left left");
    assert_eq!(
        keys(&mut s, "enter"),
        vec![Request::Approve { approval: ApprovalId(3), decision: ApprovalDecision::AllowPattern { scope: SettingsScope::Session }, reason: None }]
    );
    // Allow-once and deny are as they were.
    feed(&mut s, 6, bash_approval(4, Some("bash(cargo test*)")));
    s.now_ms += 1_000;
    assert_eq!(keys(&mut s, "y"), vec![Request::Approve { approval: ApprovalId(4), decision: ApprovalDecision::Allow, reason: None }]);
    feed(&mut s, 7, bash_approval(5, Some("bash(cargo test*)")));
    s.now_ms += 1_000;
    assert_eq!(keys(&mut s, "n"), vec![Request::Approve { approval: ApprovalId(5), decision: ApprovalDecision::Deny, reason: None }]);
}

#[test]
fn parity_M11_with_no_proposed_rule_the_buttons_are_the_old_three() {
    let mut s = coding_session();
    feed(&mut s, 3, bash_approval(1, None));
    let buf = palette_frame(&s);
    assert!(contains(&buf, "allow for session") && !contains(&buf, "always"), "{}", dump(&buf));
    s.now_ms += 1_000;
    assert_eq!(keys(&mut s, "a"), vec![Request::Approve { approval: ApprovalId(1), decision: ApprovalDecision::AllowForSession, reason: None }]);
}

// ---- X16 -------------------------------------------------------------------------------------------

#[test]
fn parity_X16_hooks_shows_which_hooks_run_and_trusts_a_project_hook() {
    let mut s = coding_session();
    let reqs = keys(&mut s, "'/hooks' enter");
    assert!(reqs.contains(&Request::ListHooks { workspace: ws() }));
    feed(
        &mut s,
        0,
        Event::Hooks {
            hooks: vec![
                HookInfo { event: "PreToolUse".into(), matcher: Some("Bash".into()), command: "./scripts/check.sh".into(), scope: SettingsScope::Project, trusted: false },
                HookInfo { event: "PostToolUse".into(), matcher: None, command: "cargo fmt".into(), scope: SettingsScope::User, trusted: true },
            ],
            digest: String::new(),
        },
    );
    let buf = palette_frame(&s);
    for want in ["hooks", "PreToolUse", "Bash", "not trusted — does not run", "./scripts/check.sh", "PostToolUse", "runs"] {
        assert!(contains(&buf, want), "{want:?}\n{}", dump(&buf));
    }
    snapshot("hooks_panel", &buf);
    // Nothing is trusted from a listing that carried no digest: the daemon would refuse it, and
    // there is nothing the user reviewed to bind the trust to.
    assert!(keys(&mut s, "t").is_empty());
    assert!(contains(&palette_frame(&s), "sent no digest"));
    // With the digest of the listing, `t` asks first, naming everything it would trust.
    s.set_hooks_digest("d1g3st-0f-list");
    assert!(keys(&mut s, "t").is_empty(), "nothing is sent until it is confirmed");
    let buf = palette_frame(&s);
    for want in ["trust all 1 project hook?", "PreToolUse Bash · ./scripts/check.sh", "d1g3st-0f-list"] {
        assert!(contains(&buf, want), "{want:?}\n{}", dump(&buf));
    }
    snapshot("hooks_trust_confirm", &buf);
    // Anything but a yes backs out, and the panel stays open.
    assert!(keys(&mut s, "esc").is_empty());
    assert!(contains(&palette_frame(&s), "hooks") && !contains(&palette_frame(&s), "trust all"));
    // Yes sends the digest that was on screen.
    keys(&mut s, "t");
    assert_eq!(keys(&mut s, "enter"), vec![Request::TrustHooks { workspace: WS.into(), digest: "d1g3st-0f-list".into() }]);
    // A listing with another digest while the question is up drops it: that is not what was read.
    keys(&mut s, "t");
    s.set_hooks_digest("another-digest");
    assert!(keys(&mut s, "enter").is_empty());
    assert!(contains(&palette_frame(&s), "hooks changed"));
    // The daemon's own refusal is shown, and the listing is asked for again.
    feed(&mut s, 0, Event::Error { session: None, message: "the project's hooks or settings changed since you looked; review them again".into() });
    let reqs = feed(&mut s, 0, Event::Error { session: None, message: "the project's hooks or settings changed since you looked; review them again".into() });
    assert!(reqs.contains(&Request::ListHooks { workspace: ws() }), "{reqs:?}");
    assert!(contains(&palette_frame(&s), "changed since you looked"));
    keys(&mut s, "down");
    assert!(keys(&mut s, "t").is_empty());
    assert!(contains(&palette_frame(&s), "already runs"));
}

#[test]
fn parity_X16_agents_lists_types_and_opens_one() {
    let mut s = coding_session();
    let reqs = keys(&mut s, "'/agents' enter");
    assert!(reqs.contains(&Request::ListAgentTypes { workspace: ws() }));
    feed(
        &mut s,
        0,
        Event::AgentTypes {
            list: vec![
                AgentTypeInfo { name: "explore".into(), description: "Read-only search of the codebase".into(), model: None, tools: vec!["read".into(), "grep".into(), "glob".into()], source: "builtin".into() },
                AgentTypeInfo { name: "reviewer".into(), description: "Reviews a diff for bugs".into(), model: Some("opus".into()), tools: vec![], source: "C:/work/valiance/.claude/agents/reviewer.md".into() },
            ],
        },
    );
    let buf = palette_frame(&s);
    for want in ["agents", "explore", "reviewer", "opus", "inherit"] {
        assert!(contains(&buf, want), "{want:?}\n{}", dump(&buf));
    }
    keys(&mut s, "down enter");
    let buf = palette_frame(&s);
    assert!(contains(&buf, "Reviews a diff for bugs") && contains(&buf, "the caller's tools, without agent"), "{}", dump(&buf));
    snapshot("agents_panel", &buf);
}

#[test]
fn parity_X16_mcp_lists_servers_adds_and_removes() {
    let mut s = coding_session();
    let reqs = keys(&mut s, "'/mcp' enter");
    assert!(reqs.contains(&Request::McpList { workspace: ws() }));
    feed(
        &mut s,
        0,
        Event::McpServers {
            list: vec![
                McpServerInfo { name: "github".into(), scope: SettingsScope::Project, transport: "http".into(), tools: 31, status: "needs auth".into(), detail: String::new() },
                McpServerInfo { name: "files".into(), scope: SettingsScope::User, transport: "stdio".into(), tools: 6, status: "connected".into(), detail: String::new() },
            ],
        },
    );
    let buf = palette_frame(&s);
    for want in ["mcp servers", "github", "needs auth", "31 tools", "connected"] {
        assert!(contains(&buf, want), "{want:?}\n{}", dump(&buf));
    }
    snapshot("mcp_panel", &buf);
    assert_eq!(keys(&mut s, "d"), vec![Request::McpRemove { name: "github".into(), scope: SettingsScope::Project, workspace: ws() }]);
    keys(&mut s, "a 'x'");
    keys(&mut s, "enter");
    assert!(contains(&palette_frame(&s), "then a command or an https:// URL"));
    keys(&mut s, "ctrl-u 'fs npx -y @mcp/fs .'");
    let reqs = keys(&mut s, "enter");
    assert_eq!(
        reqs,
        vec![Request::McpAdd { name: "fs".into(), scope: SettingsScope::Project, config: json!({"command": "npx", "args": ["-y", "@mcp/fs", "."]}), workspace: ws() }]
    );
}

// ---- C6 --------------------------------------------------------------------------------------------

#[test]
fn parity_C6_memory_opens_the_instruction_files_in_the_editor_and_reloads_after() {
    let mut s = coding_session();
    keys(&mut s, "'/memory' enter");
    assert_eq!(s.take_effects(), vec![Effect::FindInstructions { workspace: ws() }]);
    let files = vec![
        InstructionFile { path: "C:/work/valiance/CLAUDE.md".into(), scope: "project".into(), exists: true },
        InstructionFile { path: "C:/work/valiance/MARLOWE.md".into(), scope: "project".into(), exists: false },
    ];
    local(&mut s, LocalEvent::Instructions(files));
    let buf = palette_frame(&s);
    for want in ["instructions", "CLAUDE.md", "MARLOWE.md", "new"] {
        assert!(contains(&buf, want), "{want:?}\n{}", dump(&buf));
    }
    keys(&mut s, "down enter");
    assert_eq!(s.take_effects(), vec![Effect::OpenEditor { path: "C:/work/valiance/MARLOWE.md".into() }]);
    // The editor closes; the conductor re-reads.
    let reqs = local(&mut s, LocalEvent::EditorClosed { path: "C:/work/valiance/MARLOWE.md".into(), result: Ok(()) });
    assert_eq!(reqs, vec![Request::ReloadInstructions { session: sid() }]);
    let reqs = local(&mut s, LocalEvent::EditorClosed { path: "x".into(), result: Err("notepad exited 1".into()) });
    assert!(reqs.is_empty());
    assert!(contains(&palette_frame(&s), "the editor failed"));
}

// ---- C9 --------------------------------------------------------------------------------------------

#[test]
fn parity_C9_context_shows_the_meter_and_what_fills_it_and_compacts_on_request() {
    let mut s = busy_session();
    feed(&mut s, 3, Event::TextDelta { session: sid(), turn: 1, text: "Looking at the gate now.".into() });
    feed(&mut s, 4, tool_started("c1", "read", "read src/gate.rs"));
    feed(&mut s, 5, tool_finished("c1", true, &format!("80 lines\n{}", "fn gate() {}\n".repeat(80))));
    feed(&mut s, 6, Event::ContextUsage { session: sid(), turn: 1, tokens: 150_000, window: 200_000 });
    keys(&mut s, "'/context'");
    keys(&mut s, "enter");
    let buf = palette_frame(&s);
    for want in ["context", "75% of the window used", "an estimate", "your messages", "replies", "tool calls and results", "0 open todos"] {
        assert!(contains(&buf, want), "{want:?}\n{}", dump(&buf));
    }
    snapshot("context_usage_panel", &buf);
    let reqs = keys(&mut s, "c");
    assert_eq!(reqs, vec![Request::Compact { session: sid(), focus: None }]);
    assert_eq!(s.overlay, None);
}

#[test]
fn parity_C9_the_daemons_breakdown_and_truncation_notices_replace_the_estimate_when_it_sends_them() {
    let mut s = busy_session();
    feed(&mut s, 6, Event::ContextUsage { session: sid(), turn: 1, tokens: 150_000, window: 200_000 });
    s.set_context_breakdown(
        vec![("system prompt".into(), 6_200), ("tool schemas".into(), 11_800), ("MCP tools".into(), 3_400), ("instruction files".into(), 2_100), ("messages".into(), 126_500)],
        vec!["3 older tool results were masked".into(), "bash output over 30,000 characters was cut".into()],
    );
    keys(&mut s, "'/context'");
    keys(&mut s, "enter");
    let buf = palette_frame(&s);
    for want in ["what fills the window", "system prompt", "tool schemas", "MCP tools", "instruction files", "126,500", "3 older tool results were masked", "bash output over 30,000 characters was cut"] {
        assert!(contains(&buf, want), "{want:?}\n{}", dump(&buf));
    }
    assert!(!contains(&buf, "an estimate"), "the daemon's count is not labelled an estimate\n{}", dump(&buf));
}

// ---- C14, C15 --------------------------------------------------------------------------------------

#[test]
fn parity_C14_effort_is_set_for_the_next_turns_and_thinking_stays_collapsed_until_asked() {
    let mut s = busy_session();
    let reqs = keys(&mut s, "alt-t");
    assert_eq!(
        reqs,
        vec![
            Request::SetSlot { slot: Slot::Conductor, model: Some(sonnet_ref()), effort: Effort::High },
            Request::SwitchModel { session: sid(), model: sonnet_ref(), effort: Some(Effort::High) },
        ]
    );
    assert!(contains(&palette_frame(&s), "thinking effort: high"));
    let reqs = keys(&mut s, "'/effort low' enter");
    assert!(reqs.contains(&Request::SwitchModel { session: sid(), model: sonnet_ref(), effort: Some(Effort::Low) }), "{reqs:?}");
    assert!(keys(&mut s, "'/effort turbo' enter").is_empty());
    assert!(contains(&palette_frame(&s), "effort: off, low, medium, high or max"));
    // Reasoning shows as a count, collapsed; ^t opens it.
    s.composer.clear();
    feed(&mut s, 4, Event::ReasoningDelta { session: sid(), turn: 1, text: "Considering the gate threshold carefully".into(), tokens: 6 });
    feed(&mut s, 5, Event::TextDelta { session: sid(), turn: 1, text: "Done.".into() });
    let buf = palette_frame(&s);
    assert!(contains(&buf, "thought · 6 tokens") && !contains(&buf, "Considering the gate"), "{}", dump(&buf));
    keys(&mut s, "ctrl-t");
    assert!(contains(&palette_frame(&s), "Considering the gate threshold carefully"));
}

fn two_models() -> Event {
    let list = catalog_models();
    let models: Vec<LibraryModel> = list.iter().take(2).map(|i| LibraryModel { model: i.model_ref(), info: i.clone(), added_ms: 1_799_999_000_000 }).collect();
    let first = models[0].model.clone();
    Event::LibraryState {
        models,
        slots: [Slot::Conductor, Slot::Reflection, Slot::Quick]
            .into_iter()
            .map(|slot| SlotAssignment { slot, model: Some(first.clone()), effort: Effort::Medium, inherited: false })
            .collect(),
        providers: providers(true),
    }
}

#[test]
fn parity_C15_model_switches_the_conductor_and_the_open_session_keeps_its_history() {
    let mut s = coding_session();
    feed(&mut s, 2, two_models());
    let mini = catalog_models()[1].model_ref();
    let reqs = keys(&mut s, "'/model gpt-5' enter");
    assert_eq!(
        reqs,
        vec![
            Request::SetSlot { slot: Slot::Conductor, model: Some(mini.clone()), effort: Effort::Medium },
            Request::SwitchModel { session: sid(), model: mini.clone(), effort: Some(Effort::Medium) },
            Request::LibraryState,
        ]
    );
    // The picker does the same.
    let mut s = coding_session();
    feed(&mut s, 2, two_models());
    keys(&mut s, "'/model' enter");
    assert!(matches!(s.overlay, Some(Overlay::ModelPicker { .. })));
    let reqs = keys(&mut s, "down enter");
    assert!(reqs.contains(&Request::SwitchModel { session: sid(), model: mini, effort: Some(Effort::Medium) }), "{reqs:?}");
    // A model the library does not have is said so.
    assert!(keys(&mut s, "'/model nonesuch' enter").is_empty());
    assert!(contains(&palette_frame(&s), "no model in the library matches"));
}

// ---- S1 --------------------------------------------------------------------------------------------

fn summary(id: &str, title: &str, ws: &str) -> SessionSummary {
    SessionSummary { id: SessionId::from(id), title: title.into(), workspace: Some(ws.into()), created_ms: 1_799_000_000_000, updated_ms: 1_799_900_000_000, turns: 7 }
}

#[test]
fn parity_S1_resume_is_a_picker_you_can_type_into() {
    let mut s = coding_session();
    let reqs = keys(&mut s, "'/resume' enter");
    assert_eq!(reqs, vec![Request::ListSessions { limit: 30 }]);
    assert!(matches!(s.overlay, Some(Overlay::Sessions { sel: 0 })));
    feed(
        &mut s,
        0,
        Event::SessionList {
            sessions: vec![summary("a1", "fix the gate threshold", "C:/work/valiance"), summary("b2", "paper outline", "D:/papers"), summary("c3", "gate eval rerun", "C:/work/valiance")],
        },
    );
    let buf = palette_frame(&s);
    for want in ["sessions", "search sessions", "fix the gate threshold", "paper outline", "gate eval rerun"] {
        assert!(contains(&buf, want), "{want:?}\n{}", dump(&buf));
    }
    snapshot("resume_picker", &buf);
    keys(&mut s, "'gate'");
    let buf = palette_frame(&s);
    assert!(contains(&buf, "gate eval rerun") && !contains(&buf, "paper outline"), "typing narrows the list\n{}", dump(&buf));
    keys(&mut s, "down");
    let reqs = keys(&mut s, "enter");
    assert_eq!(reqs, vec![Request::OpenSession { session: SessionId::from("c3") }]);
    // Backspace edits the search and, with it empty, closes the picker.
    keys(&mut s, "alt-s");
    feed(&mut s, 0, Event::SessionList { sessions: vec![summary("a1", "fix the gate threshold", "C:/work/valiance")] });
    keys(&mut s, "'zz' backspace backspace");
    assert!(s.overlay.is_some());
    keys(&mut s, "backspace");
    assert_eq!(s.overlay, None);
}

// ---- S3, S4, rewind and the session commands -------------------------------------------------------

const UNIFIED: &str = "diff --git a/src/gate.rs b/src/gate.rs\n--- a/src/gate.rs\n+++ b/src/gate.rs\n@@ -10,4 +10,4 @@ fn gate\n fn gate(x: f32) -> bool {\n-    x > 0.5\n+    x > 0.62\n }\n diff --git a/notes.md b/notes.md\nnew file mode 100644\n--- /dev/null\n+++ b/notes.md\n@@ -0,0 +1,2 @@\n+threshold moved\n+see run 22\n";

#[test]
fn parity_S3_diff_shows_the_sessions_changes_exactly_as_the_daemon_computed_them() {
    let mut s = coding_session();
    let reqs = keys(&mut s, "'/diff' enter");
    assert_eq!(reqs, vec![Request::SessionDiff { session: sid(), turn: None }]);
    assert!(contains(&palette_frame(&s), "asking the daemon"));
    feed(&mut s, 0, Event::SessionDiff { session: sid(), turn: None, diff: UNIFIED.into() });
    let buf = palette_frame(&s);
    let text = dump(&buf);
    for want in ["src/gate.rs", "notes.md", "new file", "x > 0.5", "x > 0.62", "threshold moved", "see run 22"] {
        assert!(text.contains(want), "{want:?}\n{text}");
    }
    // Every added and removed line of the daemon's diff is on screen, with its sign.
    for l in UNIFIED.lines().filter(|l| (l.starts_with('+') || l.starts_with('-')) && !l.starts_with("+++") && !l.starts_with("---")) {
        let (sign, body) = l.split_at(1);
        let shown = format!("{} {}", if sign == "-" { "−" } else { "+" }, body);
        assert!(text.contains(&shown), "{shown:?} missing\n{text}");
    }
    snapshot("diff_viewer", &buf);
    keys(&mut s, "n");
    keys(&mut s, "esc");
    assert_eq!(s.overlay, None);
    // /diff 3 is one turn.
    let reqs = keys(&mut s, "'/diff 3' enter");
    assert_eq!(reqs, vec![Request::SessionDiff { session: sid(), turn: Some(3) }]);
}

#[test]
fn parity_S4_cost_shows_what_the_daemon_totalled() {
    let mut s = coding_session();
    let reqs = keys(&mut s, "'/cost' enter");
    assert_eq!(reqs, vec![Request::SessionCost { session: sid() }]);
    let usage = Usage { input: 120_000, output: 8_000, reasoning: 0, cached_input: 90_000, cost_usd: Some(0.42), ..Usage::default() };
    feed(
        &mut s,
        0,
        Event::SessionCost { session: sid(), report: CostReport { calls: 12, usage, by_model: vec![ModelCost { model: sonnet_ref(), calls: 12, usage }] } },
    );
    let buf = palette_frame(&s);
    for want in ["cost", "12 model calls", "120k in", "8k out", "90k cached", "$0.42", "anthropic/claude-sonnet-4.5"] {
        assert!(contains(&buf, want), "{want:?}\n{}", dump(&buf));
    }
    snapshot("cost_panel", &buf);
}

#[test]
fn parity_S2_rewind_picks_a_turn_then_what_to_restore() {
    let mut s = coding_session();
    let reqs = keys(&mut s, "'/rewind' enter");
    assert_eq!(reqs, vec![Request::ListCheckpoints { session: sid() }]);
    feed(
        &mut s,
        0,
        Event::Checkpoints {
            session: sid(),
            list: vec![
                CheckpointInfo { turn: 5, at_ms: 1_800_000_000_000 - 300_000, files: 3, summary: "raised the threshold".into() },
                CheckpointInfo { turn: 2, at_ms: 1_800_000_000_000 - 3_600_000, files: 1, summary: "first edit".into() },
            ],
        },
    );
    let buf = palette_frame(&s);
    for want in ["rewind", "turn 5", "3 files", "raised the threshold", "turn 2", "1 file"] {
        assert!(contains(&buf, want), "{want:?}\n{}", dump(&buf));
    }
    keys(&mut s, "down enter");
    let buf = palette_frame(&s);
    assert!(contains(&buf, "go back to turn 2") && contains(&buf, "code only"), "{}", dump(&buf));
    snapshot("rewind_scope", &buf);
    keys(&mut s, "down");
    let reqs = keys(&mut s, "enter");
    assert_eq!(reqs, vec![Request::Rewind { session: sid(), turn: 2, scope: RewindScope::Code }]);
    // The rewound conversation is reloaded; a code-only rewind leaves it.
    let reqs = feed(&mut s, 3, Event::Rewound { session: sid(), turn: 2, scope: RewindScope::Both });
    assert!(reqs.contains(&Request::OpenSession { session: sid() }), "{reqs:?}");
    let reqs = feed(&mut s, 4, Event::Rewound { session: sid(), turn: 2, scope: RewindScope::Code });
    assert!(!reqs.iter().any(|r| matches!(r, Request::OpenSession { .. })));
}

#[test]
fn parity_S6_S7_the_session_commands_send_their_requests() {
    let mut s = coding_session();
    s.title = None;
    assert_eq!(keys(&mut s, "'/rename Gate threshold work' enter"), vec![Request::RenameSession { session: sid(), title: "Gate threshold work".into() }]);
    assert_eq!(keys(&mut s, "'/export json' enter"), vec![Request::ExportSession { session: sid(), format: ExportFormat::Json }]);
    feed(&mut s, 0, Event::SessionExported { session: sid(), format: ExportFormat::Json, text: "{}".into() });
    assert_eq!(s.take_effects(), vec![Effect::WriteFile { path: "C:/work/valiance/marlowe-session-5e55.json".into(), text: "{}".into() }]);
    assert_eq!(keys(&mut s, "'/export' enter"), vec![Request::ExportSession { session: sid(), format: ExportFormat::Markdown }]);
    assert_eq!(keys(&mut s, "'/compact keep the gate work' enter"), vec![Request::Compact { session: sid(), focus: Some("keep the gate work".into()) }]);
    assert_eq!(keys(&mut s, "'/add-dir ../lib' enter"), vec![Request::AddDir { session: sid(), path: "../lib".into() }]);
    assert_eq!(
        keys(&mut s, "'/loop 10m check the build' enter"),
        vec![Request::AddSchedule { session: Some(sid()), every_secs: 600, text: "check the build".into() }]
    );
    assert_eq!(keys(&mut s, "'/loop' enter"), vec![Request::ListSchedules]);
    assert!(keys(&mut s, "'/rename ' enter").is_empty());
    assert!(contains(&palette_frame(&s), "/rename needs title"));
    // /fork: the copy opens when the daemon names it.
    s.composer.clear();
    assert_eq!(keys(&mut s, "'/fork' enter"), vec![Request::ForkSession { session: sid(), at_turn: None }]);
    let reqs = feed(&mut s, 0, Event::SessionOpened { session: SessionId::from("f0"), title: "gate work (fork)".into(), workspace: ws() });
    assert_eq!(reqs, vec![Request::OpenSession { session: SessionId::from("f0") }]);
}

#[test]
fn parity_X10_a_command_the_daemon_lists_is_sent_as_the_message_and_unknown_ones_are_not() {
    let mut s = coding_session();
    feed(
        &mut s,
        0,
        Event::Commands {
            list: vec![
                CommandInfo { name: "init".into(), description: "write MARLOWE.md from the repo".into(), argument_hint: None, source: "builtin".into() },
                CommandInfo { name: "review".into(), description: "review a pull request".into(), argument_hint: Some("<pr>".into()), source: ".marlowe/commands/review.md".into() },
            ],
        },
    );
    // Completion lists them with their hints.
    keys(&mut s, "'/re'");
    let buf = palette_frame(&s);
    assert!(contains(&buf, "/review") && contains(&buf, "<pr>") && contains(&buf, "review a pull request"), "{}", dump(&buf));
    snapshot("slash_popup", &buf);
    s.composer.clear();
    let reqs = keys(&mut s, "'/review 12' enter");
    assert_eq!(reqs, vec![Request::Ask { session: Some(sid()), text: "/review 12".into(), workspace: None, images: vec![], options: None, client_ms: None }]);
    // An unknown command is not sent; the line stays for editing.
    assert!(keys(&mut s, "'/frobnicate now' enter").is_empty());
    assert_eq!(s.composer.text(), "/frobnicate now");
    assert!(contains(&palette_frame(&s), "unknown command /frobnicate"));
    // A path is not a command.
    s.composer.clear();
    let reqs = keys(&mut s, "'/usr/bin/python is where?' enter");
    assert_eq!(reqs.len(), 1);
    // Tab completes a name; a command that needs an argument waits for it.
    s.composer.clear();
    keys(&mut s, "'/ren' tab");
    assert_eq!(s.composer.text(), "/rename ");
    // Esc closes the popup without losing the text.
    s.composer.clear();
    keys(&mut s, "'/co' esc");
    assert_eq!(s.composer.text(), "/co");
    assert!(!contains(&palette_frame(&s), "/config"));
}

#[test]
fn parity_slash_commands_are_there_offline_and_the_mode_command_sets_the_mode() {
    let mut s = coding_session();
    assert_eq!(keys(&mut s, "'/plan' enter"), vec![Request::SetMode { session: sid(), mode: PermissionMode::Plan }]);
    assert_eq!(keys(&mut s, "'/mode accept_edits' enter"), vec![Request::SetMode { session: sid(), mode: PermissionMode::AcceptEdits }]);
    // `/mode` typed in full is /mode, not /model.
    assert_eq!(keys(&mut s, "'/mode' enter"), vec![Request::SetMode { session: sid(), mode: PermissionMode::Plan }]);
    keys(&mut s, "'/help' enter");
    let buf = palette_frame(&s);
    for want in ["help", "shift+tab", "ctrl+o", "/config", "/permissions"] {
        assert!(contains(&buf, want), "{want:?}\n{}", dump(&buf));
    }
    snapshot("help_panel", &buf);
}

// ---- T22, T23 --------------------------------------------------------------------------------------

fn todo(content: &str, status: TodoStatus, active: Option<&str>) -> TodoItem {
    TodoItem { content: content.into(), status, active_form: active.map(str::to_string), ..Default::default() }
}

#[test]
fn parity_T22_the_todo_list_is_pinned_while_anything_is_open() {
    let mut s = busy_session();
    feed(
        &mut s,
        3,
        Event::Todos {
            session: sid(),
            items: vec![
                todo("Read the gate code", TodoStatus::Completed, None),
                todo("Raise the threshold", TodoStatus::InProgress, Some("Raising the threshold")),
                todo("Run the tests", TodoStatus::Pending, None),
            ],
        },
    );
    let buf = palette_frame(&s);
    for want in ["todos 1 of 3", "✓ Read the gate code", "● Raising the threshold", "○ Run the tests"] {
        assert!(contains(&buf, want), "{want:?}\n{}", dump(&buf));
    }
    snapshot("todos_pinned", &buf);
    // /todos hides it; the list is still there.
    keys(&mut s, "'/todos' enter");
    assert!(!contains(&palette_frame(&s), "Raising the threshold"));
    keys(&mut s, "'/todos' enter");
    assert!(contains(&palette_frame(&s), "Raising the threshold"));
    // Done is done: nothing open, nothing pinned.
    feed(&mut s, 4, Event::Todos { session: sid(), items: vec![todo("Read the gate code", TodoStatus::Completed, None)] });
    assert!(!contains(&palette_frame(&s), "todos 1 of 1"));
    // Another session's list is not this one's.
    feed(&mut s, 5, Event::Todos { session: SessionId::from("other"), items: vec![todo("Elsewhere", TodoStatus::Pending, None)] });
    assert!(!contains(&palette_frame(&s), "Elsewhere"));
}

fn questions() -> Vec<Question> {
    vec![
        Question { question: "Which test runner should I use?".into(), header: "Runner".into(), options: vec!["cargo test".into(), "cargo nextest".into()], multi_select: false, option_details: vec![] },
        Question { question: "Which crates should the change touch?".into(), header: "Crates".into(), options: vec!["marlowe-tui".into(), "marlowe-agent".into(), "marlowe-proto".into()], multi_select: true, option_details: vec![] },
    ]
}

#[test]
fn parity_T23_ask_user_is_a_modal_whose_answers_go_back_in_order() {
    let mut s = busy_session();
    feed(&mut s, 3, Event::QuestionAsked { session: sid(), id: "q1".into(), questions: questions() });
    assert_eq!(s.overlay, Some(Overlay::Panel));
    let buf = palette_frame(&s);
    for want in ["question", "Runner", "Crates", "Which test runner should I use?", "cargo nextest", "something else"] {
        assert!(contains(&buf, want), "{want:?}\n{}", dump(&buf));
    }
    snapshot("ask_user", &buf);
    s.now_ms += 1_000;
    // 2 picks the second option and moves on.
    assert!(keys(&mut s, "2").is_empty());
    let buf = palette_frame(&s);
    assert!(contains(&buf, "Which crates should the change touch?") && contains(&buf, "[ ] marlowe-tui"), "{}", dump(&buf));
    // Space toggles; enter on the last unanswered question sends everything.
    keys(&mut s, "space down down space");
    let reqs = keys(&mut s, "enter");
    assert_eq!(reqs, vec![Request::AnswerQuestion { session: sid(), id: "q1".into(), answers: vec!["cargo nextest".into(), "marlowe-tui, marlowe-proto".into()] }]);
    assert_eq!(s.overlay, None);
}

#[test]
fn parity_T23_free_text_and_skipping() {
    let mut s = busy_session();
    feed(&mut s, 3, Event::QuestionAsked { session: sid(), id: "q2".into(), questions: questions()[..1].to_vec() });
    s.now_ms += 1_000;
    // Down to "something else", type, enter.
    keys(&mut s, "down down 'cargo test --release'");
    let reqs = keys(&mut s, "enter");
    assert_eq!(reqs, vec![Request::AnswerQuestion { session: sid(), id: "q2".into(), answers: vec!["cargo test --release".into()] }]);
    // Esc declines: the model gets no answers and decides.
    feed(&mut s, 4, Event::QuestionAsked { session: sid(), id: "q3".into(), questions: questions()[..1].to_vec() });
    let reqs = keys(&mut s, "esc");
    assert_eq!(reqs, vec![Request::AnswerQuestion { session: sid(), id: "q3".into(), answers: vec![] }]);
    assert_eq!(s.overlay, None);
    // A question for another session does not take this one's screen.
    feed(&mut s, 5, Event::QuestionAsked { session: SessionId::from("other"), id: "q4".into(), questions: questions() });
    assert_eq!(s.overlay, None);
}

#[test]
fn parity_T23_keys_typed_ahead_of_the_question_are_not_answers() {
    let mut s = busy_session();
    // The user was typing into the Message box when the question arrived.
    feed(&mut s, 3, Event::QuestionAsked { session: sid(), id: "q1".into(), questions: questions()[..1].to_vec() });
    assert!(keys(&mut s, "2").is_empty(), "a digit inside the guard picks nothing");
    assert!(keys(&mut s, "enter").is_empty(), "so does enter");
    assert_eq!(s.overlay, Some(Overlay::Panel), "the question is still waiting");
    s.now_ms += 500;
    assert_eq!(keys(&mut s, "2"), vec![Request::AnswerQuestion { session: sid(), id: "q1".into(), answers: vec!["cargo nextest".into()] }]);
}

#[test]
fn parity_T23_a_second_question_waits_behind_the_first_and_both_are_answered_in_order() {
    let mut s = busy_session();
    feed(&mut s, 3, Event::QuestionAsked { session: sid(), id: "q1".into(), questions: questions()[..1].to_vec() });
    // A subagent on the same session asks too, while the first is still open.
    feed(&mut s, 4, Event::QuestionAsked { session: sid(), id: "q2".into(), questions: questions()[..1].to_vec() });
    assert!(contains(&palette_frame(&s), "Which test runner should I use?"));
    s.now_ms += 1_000;
    assert_eq!(keys(&mut s, "1"), vec![Request::AnswerQuestion { session: sid(), id: "q1".into(), answers: vec!["cargo test".into()] }]);
    // The second takes the screen, with its own guard against the key that answered the first.
    assert_eq!(s.overlay, Some(Overlay::Panel));
    assert!(keys(&mut s, "1").is_empty());
    s.now_ms += 1_000;
    assert_eq!(keys(&mut s, "2"), vec![Request::AnswerQuestion { session: sid(), id: "q2".into(), answers: vec!["cargo nextest".into()] }]);
    assert_eq!(s.overlay, None);
}

#[test]
fn parity_T23_a_question_does_not_discard_the_panel_it_covers() {
    let mut s = busy_session();
    keys(&mut s, "'/config' enter");
    feed(&mut s, 0, settings_event(&[(SettingsScope::User, "editor_url = \"vscode://file/{path}\"\n")]));
    keys(&mut s, "enter");
    keys(&mut s, "'vscode://x'");
    feed(&mut s, 3, Event::QuestionAsked { session: sid(), id: "q1".into(), questions: questions()[..1].to_vec() });
    assert!(contains(&palette_frame(&s), "Which test runner should I use?"));
    s.now_ms += 1_000;
    keys(&mut s, "esc");
    // The config panel is back, and its edit with it.
    let buf = palette_frame(&s);
    assert!(contains(&buf, "config") && contains(&buf, "vscode://x"), "{}", dump(&buf));
}

// ---- U14 -------------------------------------------------------------------------------------------

fn last_note(s: &marlowe_tui::State) -> String {
    match s.entries.last() {
        Some(marlowe_tui::state::Entry::Note { text, .. }) => text.clone(),
        other => panic!("no note, got {other:?}"),
    }
}

#[test]
fn parity_u14_ide_says_which_editor_is_connected_and_what_it_has_open() {
    let mut s = coding_session();
    let reqs = keys(&mut s, "'/ide' enter");
    assert!(reqs.contains(&Request::McpList { workspace: ws() }), "{reqs:?}");
    assert_eq!(s.overlay, None, "a line in the transcript, not a panel");
    feed(
        &mut s,
        0,
        Event::McpServers {
            list: vec![McpServerInfo {
                name: "ide".into(),
                scope: SettingsScope::Session,
                transport: "http".into(),
                tools: 6,
                status: "connected".into(),
                detail: "Visual Studio Code connected as `ide` (http://127.0.0.1:5001/mcp); it has C:/work/valiance open".into(),
            }],
        },
    );
    let note = last_note(&s);
    assert!(note.contains("editor: connected") && note.contains("6 tools") && note.contains("Visual Studio Code") && note.contains("C:/work/valiance"), "{note}");
    palette_frame(&s);
}

#[test]
fn parity_u14_ide_with_no_editor_says_how_to_get_one() {
    let mut s = coding_session();
    keys(&mut s, "'/ide' enter");
    feed(&mut s, 0, Event::McpServers { list: Vec::new() });
    let note = last_note(&s);
    assert!(note.contains("none connected") && note.contains("VS Code extension"), "{note}");
    // A later server list (the /mcp panel's) says nothing more.
    let before = s.entries.len();
    feed(&mut s, 1, Event::McpServers { list: Vec::new() });
    assert_eq!(s.entries.len(), before);
}

#[test]
fn deny_and_tell_marlowe_why_is_an_option_that_opens_a_one_line_input() {
    let mut s = coding_session();
    feed(&mut s, 3, bash_approval(1, None));
    let buf = palette_frame(&s);
    assert!(contains(&buf, "deny, and tell Marlowe why"), "{}", dump(&buf));
    s.now_ms += 1_000;
    // w opens the input; nothing is sent yet, and the line takes the keys (even the buttons' own).
    assert!(keys(&mut s, "w").is_empty());
    let buf = palette_frame(&s);
    assert!(contains(&buf, "tell Marlowe why") && contains(&buf, "esc back"), "{}", dump(&buf));
    assert!(keys(&mut s, "n y a").is_empty());
    assert_eq!(s.approvals.len(), 1);
    // Esc goes back to the options with the reason option selected.
    assert!(keys(&mut s, "esc").is_empty());
    assert!(s.approval_reason.is_none() && s.approval_sel == s.approvals[0].why_index());
    // Enter on that option opens it again; Enter sends Deny with what was typed.
    assert!(keys(&mut s, "enter").is_empty());
    type_str(&mut s, "use the scratch copy");
    assert_eq!(
        s.on_key(key(crossterm::event::KeyCode::Enter)),
        vec![Request::Approve { approval: ApprovalId(1), decision: ApprovalDecision::Deny, reason: Some("use the scratch copy".into()) }]
    );
    assert!(s.approvals.is_empty());
    assert!(contains(&palette_frame(&s), "you told Marlowe why: use the scratch copy"));
}

#[test]
fn an_empty_reason_is_a_plain_denial_and_a_click_opens_the_input() {
    use marlowe_tui::state::Target;
    let mut s = coding_session();
    feed(&mut s, 3, bash_approval(1, None));
    s.now_ms += 1_000;
    keys(&mut s, "w");
    assert_eq!(
        s.on_key(key(crossterm::event::KeyCode::Enter)),
        vec![Request::Approve { approval: ApprovalId(1), decision: ApprovalDecision::Deny, reason: None }]
    );
    feed(&mut s, 4, bash_approval(2, None));
    s.now_ms += 1_000;
    let _ = palette_frame(&s);
    let rect = s.hits.borrow().iter().rev().find(|(_, t)| *t == Target::Button(3)).map(|(r, _)| *r).expect("the reason option is clickable");
    let m = crossterm::event::MouseEvent {
        kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
        column: rect.x + 2,
        row: rect.y,
        modifiers: crossterm::event::KeyModifiers::NONE,
    };
    assert!(s.on_mouse(m, ratatui::layout::Rect::new(0, 0, W, H)).is_empty());
    assert!(s.approval_reason.is_some());
}
