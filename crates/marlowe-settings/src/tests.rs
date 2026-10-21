//! Layering, the `.claude/settings.json` import, edits, and the hook protocol against Codex's
//! hook schemas (openai/codex `codex-rs/hooks/schema/generated` at
//! 6c0c6759d0617d7670826b61aca0a931bbb6db1f, Apache-2.0) and claw-code's hook cases
//! (`rust/crates/runtime/src/hooks.rs` at 08106b0c3771ef5b4a5aa176acccd460e88b7325, MIT).

use std::path::{Path, PathBuf};

use marlowe_permission::rules::policy::HookPermission;
use marlowe_permission::rules::{Action, Kind, Subject};
use marlowe_proto::{PermissionMode, RuleAction, SettingsScope};
use serde_json::{json, Value};

use crate::hooks::{matcher_matches, HookEvent, HookKind, TrustStore};
use crate::protocol::{input_json, interpret, EventData, HookContext, HookOutcome, HookRun};
use crate::{edit, Settings};

struct Tmp(PathBuf);

impl Tmp {
    fn new(tag: &str) -> Tmp {
        let p = std::env::temp_dir().join(format!("marlowe-settings-{tag}-{}-{:?}", std::process::id(), std::thread::current().id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Tmp(p)
    }
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// A Claude Code project settings file as Claude Code writes it.
const CLAUDE_JSON: &str = r#"{
  "permissions": {
    "allow": ["Bash(npm run test:*)", "Read(~/.zshrc)"],
    "deny": ["WebFetch(domain:evil.example)", "Bash(curl:*)"],
    "ask": ["Bash(git commit:*)"],
    "defaultMode": "acceptEdits",
    "additionalDirectories": ["../shared"]
  },
  "hooks": {
    "PreToolUse": [
      { "matcher": "Bash", "hooks": [{ "type": "command", "command": "python .claude/hooks/check.py", "timeout": 30 }] }
    ],
    "Stop": [{ "hooks": [{ "type": "prompt", "prompt": "Is the task done? $ARGUMENTS" }] }]
  },
  "env": { "SECRET": "not imported" },
  "model": "claude-x"
}"#;

fn shell_rule(set: &marlowe_permission::rules::RuleSet, cmd: &str) -> Option<Action> {
    let root = PathBuf::from(if cfg!(windows) { r"C:\ws" } else { "/ws" });
    let roots = [root.clone()];
    let a = marlowe_permission::shell::analyze(
        marlowe_permission::shell::Dialect::Bash,
        cmd,
        &marlowe_permission::shell::ShellCtx::workspace(root, &roots),
    );
    let mut s = Subject::new("bash", Kind::Shell);
    s.shell = Some(&a);
    set.evaluate(&s).map(|m| m.action)
}

#[test]
fn parity_m9_layering_and_import_fixture() {
    let t = Tmp::new("layers");
    let home = t.0.join("home");
    let ws = t.0.join("ws");
    write(&home.join("settings.toml"), "# mine\n[permissions]\nallow = [\"bash(cargo test:*)\"]\ndeny = [\"read(.env)\"]\n");
    write(&ws.join(".claude/settings.json"), CLAUDE_JSON);
    write(
        &ws.join(".marlowe/settings.toml"),
        "[permissions]\nask = [\"bash(cargo publish:*)\"]\nproject_commands = [\"bash(just test:*)\"]\ndefault_mode = \"bypass\"\n[network]\nallow_hosts = [\"crates.io\"]\n",
    );
    write(&ws.join(".marlowe/settings.local.toml"), "[permissions]\nallow = [\"bash(make:*)\"]\n");

    // Untrusted: the project's denies and asks apply, its allows, hooks, commands, hosts and
    // mode do not.
    let s = Settings::load(&home, Some(&ws), None);
    assert!(!s.trusted, "a project that widens anything starts untrusted");
    assert!(s.errors.is_empty(), "{:?}", s.errors);
    let r = s.rule_set();
    assert_eq!(shell_rule(&r, "cargo test -q"), Some(Action::Allow), "user allow");
    assert_eq!(shell_rule(&r, "curl https://x.dev"), Some(Action::Deny), "imported deny always applies");
    assert_eq!(shell_rule(&r, "git commit -m x"), Some(Action::Ask), "imported ask always applies");
    assert_eq!(shell_rule(&r, "cargo publish"), Some(Action::Ask));
    assert_eq!(shell_rule(&r, "npm run test"), None, "an untrusted project allows nothing");
    assert_eq!(shell_rule(&r, "make"), None);
    assert!(s.hooks().is_empty());
    assert_eq!(s.hook_rows().len(), 2);
    assert!(s.hook_rows().iter().all(|(_, trusted)| !trusted));
    assert_eq!(s.default_mode(), None);
    assert!(s.project_commands().is_empty());
    assert!(s.allow_hosts().is_empty());

    // Trusted at this digest: everything applies, but a project can never pick bypass.
    TrustStore::trust(&home, &ws, &s.digest).unwrap();
    let s = Settings::load(&home, Some(&ws), None);
    assert!(s.trusted);
    let r = s.rule_set();
    assert_eq!(shell_rule(&r, "npm run test -- --watch"), Some(Action::Allow), "Claude Code's spelling");
    assert_eq!(shell_rule(&r, "make all"), Some(Action::Allow), "local allow");
    assert_eq!(s.default_mode(), Some(PermissionMode::AcceptEdits), "bypass from a project file is ignored; acceptEdits imported");
    assert_eq!(s.project_commands(), ["bash(just test:*)"]);
    assert_eq!(s.allow_hosts(), ["crates.io"]);
    assert_eq!(s.additional_directories(), [ws.join("../shared")]);
    let hooks = s.hooks();
    assert_eq!(hooks.len(), 2);
    let pre = hooks.iter().find(|h| h.event == HookEvent::PreToolUse).unwrap();
    assert_eq!(pre.kind, HookKind::Command("python .claude/hooks/check.py".into()));
    assert_eq!(pre.timeout, 30);
    assert!(pre.matches("bash"), "a Claude Code matcher names Marlowe's tool");
    assert!(matches!(hooks.iter().find(|h| h.event == HookEvent::Stop).unwrap().kind, HookKind::Prompt(_)));

    // Changing a hook changes the digest: untrusted again until the user looks.
    write(&ws.join(".claude/settings.json"), &CLAUDE_JSON.replace("check.py", "pwn.py"));
    assert!(!Settings::load(&home, Some(&ws), None).trusted);

    // The command line is the highest layer.
    let cli = crate::import::from_cli_arg(r#"{"permissions":{"defaultMode":"plan","deny":["Bash(make:*)"]}}"#).unwrap();
    let s = Settings::load(&home, Some(&ws), Some(cli));
    assert_eq!(s.default_mode(), Some(PermissionMode::Plan));
    assert_eq!(shell_rule(&s.rule_set(), "make"), Some(Action::Deny), "deny > allow across layers");

    // A user setting may choose bypass.
    write(&home.join("settings.toml"), "[permissions]\ndefault_mode = \"auto\"\n");
    write(&ws.join(".marlowe/settings.toml"), "");
    let s = Settings::load(&home, Some(&ws), None);
    assert!(!s.trusted);
    assert_eq!(s.default_mode(), Some(PermissionMode::Bypass), "the user's own choice; the untrusted project's is ignored");
}

#[test]
fn a_project_with_nothing_to_widen_is_trusted_and_broken_files_are_reported() {
    let t = Tmp::new("plain");
    let home = t.0.join("home");
    let ws = t.0.join("ws");
    write(&ws.join(".marlowe/settings.toml"), "[permissions]\ndeny = [\"bash(rm:*)\", \"bash(oops\"]\n");
    write(&ws.join(".claude/settings.json"), "{ not json");
    let s = Settings::load(&home, Some(&ws), None);
    assert!(s.trusted);
    assert_eq!(s.errors.len(), 2, "{:?}", s.errors);
    assert_eq!(shell_rule(&s.rule_set(), "rm x"), Some(Action::Deny));
}

#[test]
fn edits_keep_comments_and_validate() {
    let t = Tmp::new("edit");
    let p = t.0.join(".marlowe/settings.local.toml");
    write(&p, "# keep me\n[permissions]\nallow = [\"bash(ls:*)\"] # and me\n\n[statusline]\ncommand = \"x\"\n");
    assert!(edit::add_rule(&p, RuleAction::Allow, "bash(cargo test:*)").unwrap());
    assert!(!edit::add_rule(&p, RuleAction::Allow, "bash(cargo test:*)").unwrap(), "once");
    assert!(edit::add_rule(&p, RuleAction::Deny, "bash(git").is_err(), "invalid patterns are refused");
    let text = std::fs::read_to_string(&p).unwrap();
    assert!(text.contains("# keep me") && text.contains("# and me") && text.contains("[statusline]"), "{text}");
    let s = crate::SettingsFile::from_toml(&text).unwrap();
    assert_eq!(s.permissions.allow, ["bash(ls:*)", "bash(cargo test:*)"]);
    assert!(edit::remove_rule(&p, "bash(ls:*)").unwrap());
    assert!(!edit::remove_rule(&p, "bash(ls:*)").unwrap());
    edit::set_key(&p, "network.allow_hosts", &json!(["pypi.org"])).unwrap();
    edit::set_key(&p, "permissions.default_mode", &json!("plan")).unwrap();
    let s = crate::SettingsFile::from_toml(&std::fs::read_to_string(&p).unwrap()).unwrap();
    assert_eq!(s.network.allow_hosts, ["pypi.org"]);
    assert_eq!(s.permissions.default_mode.as_deref(), Some("plan"));
    edit::set_key(&p, "permissions.default_mode", &Value::Null).unwrap();
    assert!(edit::set_key(&p, "permissions.allow", &json!(5)).is_err(), "the result must still be settings");
    // A new file and its directory are created; Claude Code's files are never written.
    let fresh = t.0.join("new/.marlowe/settings.local.toml");
    assert!(edit::add_rule(&fresh, RuleAction::Allow, "edit(src/**)").unwrap());
    assert!(edit::add_rule(&t.0.join(".claude/settings.json"), RuleAction::Allow, "edit").is_err());
    // Scope paths.
    let paths = crate::layers::Paths::new(&t.0, Some(&t.0.join("ws")));
    assert_eq!(paths.for_scope(SettingsScope::Local), Some(t.0.join("ws/.marlowe/settings.local.toml")));
    assert_eq!(paths.for_scope(SettingsScope::Session), None);
}

#[test]
fn retrust_after_a_user_edit_keeps_a_trusted_project_trusted() {
    let t = Tmp::new("retrust");
    let home = t.0.join("home");
    let ws = t.0.join("ws");
    write(&ws.join(".marlowe/settings.toml"), "[permissions]\nallow = [\"bash(make:*)\"]\n");
    let s = Settings::load(&home, Some(&ws), None);
    TrustStore::trust(&home, &ws, &s.digest).unwrap();
    edit::add_rule(&ws.join(".marlowe/settings.local.toml"), RuleAction::Allow, "bash(cargo test:*)").unwrap();
    assert!(!Settings::load(&home, Some(&ws), None).trusted);
    Settings::retrust_after_edit(&home, &ws, true).unwrap();
    assert!(Settings::load(&home, Some(&ws), None).trusted);
}

// ---- hook protocol

fn ctx() -> HookContext {
    HookContext {
        session_id: "s1".into(),
        transcript_path: Some("C:/h/sessions/s1.jsonl".into()),
        cwd: "C:/ws".into(),
        model: "marlowe-dusk:27b".into(),
        permission_mode: "acceptEdits".into(),
        turn_id: "7".into(),
        agent_id: Some("a1".into()),
        agent_type: Some("explore".into()),
        source: None,
    }
}

/// Every event with an input schema in Codex's oracle, as Marlowe fills it.
fn all_events() -> Vec<(&'static str, EventData)> {
    vec![
        ("pre-tool-use", EventData::PreToolUse { tool_name: "bash".into(), tool_input: json!({"command": "ls"}), tool_use_id: "c1".into() }),
        ("post-tool-use", EventData::PostToolUse { tool_name: "edit".into(), tool_input: json!({"path": "a"}), tool_response: json!("ok"), tool_use_id: "c1".into() }),
        ("permission-request", EventData::PermissionRequest { tool_name: "bash".into(), tool_input: json!({"command": "ls"}) }),
        ("user-prompt-submit", EventData::UserPromptSubmit { prompt: "fix it".into() }),
        ("stop", EventData::Stop { stop_hook_active: false, last_assistant_message: "done".into() }),
        ("subagent-start", EventData::SubagentStart { agent_id: "a1".into(), agent_type: "explore".into() }),
        (
            "subagent-stop",
            EventData::SubagentStop {
                agent_id: "a1".into(),
                agent_type: "explore".into(),
                agent_transcript_path: None,
                stop_hook_active: true,
                last_assistant_message: "found it".into(),
            },
        ),
        ("pre-compact", EventData::PreCompact { trigger: "auto".into(), custom_instructions: None }),
        ("post-compact", EventData::PostCompact { trigger: "manual".into() }),
        ("session-start", EventData::SessionStart { source: "startup".into() }),
        ("session-end", EventData::SessionEnd { reason: "other".into() }),
    ]
}

fn schema(name: &str) -> Value {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex-hooks");
    serde_json::from_str(&std::fs::read_to_string(dir.join(name)).unwrap()).unwrap()
}

/// A JSON-schema subset (draft 7): type, const, enum, properties, required,
/// additionalProperties, $ref into definitions, allOf. Returns every violation.
fn validate(v: &Value, s: &Value, root: &Value, at: &str, out: &mut Vec<String>) {
    if s == &Value::Bool(true) {
        return;
    }
    if let Some(r) = s.get("$ref").and_then(Value::as_str) {
        let name = r.trim_start_matches("#/definitions/");
        return validate(v, &root["definitions"][name], root, at, out);
    }
    if let Some(all) = s.get("allOf").and_then(Value::as_array) {
        for sub in all {
            validate(v, sub, root, at, out);
        }
    }
    if let Some(t) = s.get("type") {
        let types: Vec<&str> = match t {
            Value::String(x) => vec![x.as_str()],
            Value::Array(xs) => xs.iter().filter_map(Value::as_str).collect(),
            _ => vec![],
        };
        let ok = types.iter().any(|t| match *t {
            "string" => v.is_string(),
            "boolean" => v.is_boolean(),
            "object" => v.is_object(),
            "array" => v.is_array(),
            "null" => v.is_null(),
            "number" | "integer" => v.is_number(),
            _ => true,
        });
        if !ok {
            out.push(format!("{at}: {v} is not {types:?}"));
        }
    }
    if let Some(c) = s.get("const") {
        if v != c {
            out.push(format!("{at}: {v} is not {c}"));
        }
    }
    if let Some(e) = s.get("enum").and_then(Value::as_array) {
        if !e.contains(v) {
            out.push(format!("{at}: {v} not in {e:?}"));
        }
    }
    if let (Some(obj), Some(props)) = (v.as_object(), s.get("properties").and_then(Value::as_object)) {
        for r in s.get("required").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
            if !obj.contains_key(r) {
                out.push(format!("{at}: missing `{r}`"));
            }
        }
        for (k, val) in obj {
            match props.get(k) {
                Some(ps) => validate(val, ps, root, &format!("{at}.{k}"), out),
                None if s.get("additionalProperties") == Some(&Value::Bool(false)) => out.push(format!("{at}: unexpected `{k}`")),
                None => {}
            }
        }
    }
}

#[test]
fn parity_x_hook_input_matches_codex_schemas() {
    for (file, data) in all_events() {
        let s = schema(&format!("{file}.command.input.schema.json"));
        let v = input_json(&ctx(), &data);
        let mut errors = Vec::new();
        validate(&v, &s, &s, file, &mut errors);
        assert!(errors.is_empty(), "{file}: {errors:?}\n{v}");
    }
    // Claude Code's tool names reach `.claude` hooks.
    let v = input_json(&ctx(), &all_events()[0].1);
    assert_eq!(v["tool_name"], "Bash");
    assert_eq!(v["permission_mode"], "acceptEdits");
}

/// The output fields Marlowe reads, per event; every field of Codex's output schemas must be
/// here (so a field the oracle adds fails this test until it is handled).
fn handled(event: &str) -> (&'static [&'static str], &'static [&'static str]) {
    const COMMON: &[&str] = &["continue", "stopReason", "suppressOutput", "systemMessage", "decision", "reason", "hookSpecificOutput"];
    match event {
        "pre-tool-use" => (COMMON, &["hookEventName", "permissionDecision", "permissionDecisionReason", "updatedInput", "additionalContext"]),
        "post-tool-use" => (COMMON, &["hookEventName", "additionalContext", "updatedMCPToolOutput"]),
        "permission-request" => (COMMON, &["hookEventName", "decision"]),
        "user-prompt-submit" | "session-start" | "subagent-start" => (COMMON, &["hookEventName", "additionalContext"]),
        _ => (COMMON, &[]),
    }
}

#[test]
fn parity_x_every_codex_output_field_is_handled() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex-hooks");
    let mut seen = 0;
    for entry in std::fs::read_dir(dir).unwrap() {
        let name = entry.unwrap().file_name().to_string_lossy().to_string();
        let Some(event) = name.strip_suffix(".command.output.schema.json") else { continue };
        if event == "interrupt" {
            continue; // Codex's own event (Marlowe interrupts through the turn's cancel token).
        }
        let s = schema(&name);
        let (top, specific) = handled(event);
        for k in s["properties"].as_object().unwrap().keys() {
            assert!(top.contains(&k.as_str()), "{event}: output field `{k}` is not handled");
        }
        for (dname, d) in s["definitions"].as_object().into_iter().flatten() {
            if dname.ends_with("HookSpecificOutputWire") {
                for k in d["properties"].as_object().unwrap().keys() {
                    assert!(specific.contains(&k.as_str()), "{event}: hookSpecificOutput.{k} is not handled");
                }
            }
        }
        seen += 1;
    }
    assert_eq!(seen, 10);
    // Every Claude Code event has a name Marlowe parses.
    for e in HookEvent::ALL {
        assert_eq!(HookEvent::parse(e.name()), Some(e));
    }
}

fn ok_json(event: HookEvent, v: Value) -> HookOutcome {
    interpret(event, &HookRun { command: "h".into(), exit: Some(0), stdout: v.to_string(), ..HookRun::default() })
}

#[test]
fn parity_x1_pre_tool_use_output_fields() {
    let e = HookEvent::PreToolUse;
    let hso = |extra: Value| {
        let mut h = json!({"hookEventName": "PreToolUse"});
        h.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        ok_json(e, json!({ "hookSpecificOutput": h }))
    };
    assert_eq!(hso(json!({"permissionDecision": "allow", "permissionDecisionReason": "ok"})).permission, Some(HookPermission::Allow(Some("ok".into()))));
    let deny = hso(json!({"permissionDecision": "deny", "permissionDecisionReason": "no"}));
    assert_eq!(deny.permission, Some(HookPermission::Deny("no".into())));
    assert_eq!(deny.block.as_deref(), Some("no"));
    assert_eq!(hso(json!({"permissionDecision": "ask"})).permission, Some(HookPermission::Ask(None)));
    assert_eq!(hso(json!({"updatedInput": {"command": "git status"}})).updated_input, Some(json!({"command": "git status"})));
    assert_eq!(hso(json!({"additionalContext": "lint first"})).additional_context, ["lint first"]);
    assert_eq!(ok_json(e, json!({"continue": false, "stopReason": "freeze"})).stop.as_deref(), Some("freeze"));
    assert_eq!(ok_json(e, json!({"systemMessage": "heads up"})).system_messages, ["heads up"]);
    assert!(ok_json(e, json!({"suppressOutput": true})).suppress_output);
    assert_eq!(ok_json(e, json!({"decision": "block", "reason": "nope"})).permission, Some(HookPermission::Deny("nope".into())));
    assert_eq!(ok_json(e, json!({"decision": "approve"})).permission, Some(HookPermission::Allow(None)));
    // Exit 2 blocks with stderr as the reason.
    let blocked = interpret(e, &HookRun { exit: Some(2), stderr: "blocked by hook".into(), ..HookRun::default() });
    assert_eq!(blocked.permission, Some(HookPermission::Deny("blocked by hook".into())));
}

#[test]
fn parity_x2_post_tool_use_and_failure_output() {
    let o = ok_json(HookEvent::PostToolUse, json!({"decision": "block", "reason": "tests fail", "hookSpecificOutput": {"hookEventName": "PostToolUse", "additionalContext": "see log", "updatedMCPToolOutput": {"x": 1}}}));
    assert_eq!(o.block.as_deref(), Some("tests fail"));
    assert_eq!(o.additional_context, ["see log"]);
    assert_eq!(o.updated_mcp_output, Some(json!({"x": 1})));
    let f = interpret(HookEvent::PostToolUseFailure, &HookRun { exit: Some(2), stderr: "retry with --locked".into(), ..HookRun::default() });
    assert_eq!(f.block.as_deref(), Some("retry with --locked"));
}

#[test]
fn parity_x3_user_prompt_submit_blocks_or_adds_context() {
    let e = HookEvent::UserPromptSubmit;
    let plain = interpret(e, &HookRun { exit: Some(0), stdout: "today is Friday\n".into(), ..HookRun::default() });
    assert_eq!(plain.additional_context, ["today is Friday"], "plain stdout is context here");
    assert_eq!(ok_json(e, json!({"decision": "block", "reason": "no secrets"})).block.as_deref(), Some("no secrets"));
    assert_eq!(ok_json(e, json!({"hookSpecificOutput": {"hookEventName": "UserPromptSubmit", "additionalContext": "ctx"}})).additional_context, ["ctx"]);
}

#[test]
fn parity_x4_stop_can_force_continuation() {
    for e in [HookEvent::Stop, HookEvent::SubagentStop] {
        assert_eq!(ok_json(e, json!({"decision": "block", "reason": "run the tests"})).block.as_deref(), Some("run the tests"));
        assert_eq!(interpret(e, &HookRun { exit: Some(2), stderr: "keep going".into(), ..HookRun::default() }).block.as_deref(), Some("keep going"));
    }
}

#[test]
fn parity_x5_x7_session_and_compact_events_only_inform() {
    let s = ok_json(HookEvent::SessionStart, json!({"hookSpecificOutput": {"hookEventName": "SessionStart", "additionalContext": "branch: main"}}));
    assert_eq!(s.additional_context, ["branch: main"]);
    for e in [HookEvent::SessionEnd, HookEvent::PreCompact, HookEvent::PostCompact, HookEvent::Notification] {
        let o = interpret(e, &HookRun { exit: Some(2), stderr: "shown only".into(), ..HookRun::default() });
        assert_eq!(o.block, None, "{e:?} cannot block");
        assert_eq!(o.notes, ["shown only"]);
    }
}

#[test]
fn parity_x8_permission_request_decides_the_modal() {
    let o = ok_json(
        HookEvent::PermissionRequest,
        json!({"hookSpecificOutput": {"hookEventName": "PermissionRequest", "decision": {"behavior": "allow", "updatedInput": {"command": "ls"}}}}),
    );
    let d = o.permission_request.unwrap();
    assert!(d.allow);
    assert_eq!(d.updated_input, Some(json!({"command": "ls"})));
    let o = ok_json(
        HookEvent::PermissionRequest,
        json!({"hookSpecificOutput": {"hookEventName": "PermissionRequest", "decision": {"behavior": "deny", "message": "not on Fridays", "interrupt": true}}}),
    );
    let d = o.permission_request.unwrap();
    assert!(!d.allow && d.interrupt);
    assert_eq!(d.message.as_deref(), Some("not on Fridays"));
}

#[test]
fn parity_x9_matcher_table() {
    let rows: &[(Option<&str>, &str, bool)] = &[
        (None, "bash", true),
        (Some(""), "edit", true),
        (Some("*"), "anything", true),
        (Some("Bash"), "bash", true),
        (Some("Bash"), "job_output", false),
        (Some("Edit|Write"), "edit", true),
        (Some("Edit|Write"), "write", true),
        (Some("Edit|Write"), "read", false),
        (Some("mcp__.*"), "mcp__github__create_issue", true),
        (Some("mcp__github__.*"), "mcp__gitlab__x", false),
        (Some("Notebook.*"), "notebook_edit", true),
        (Some("Read*"), "ReadFile", true),
        (Some("startup|resume"), "resume", true),
        (Some("auto"), "manual", false),
        (Some("permission_prompt"), "permission_prompt", true),
        (Some("[unclosed"), "[unclosed", true),
        // `team` was `workflow`: a hook written for the old name still fires.
        (Some("workflow"), "team", true),
        (Some("team"), "team", true),
        (Some("workflow"), "agent", false),
    ];
    for (m, target, want) in rows {
        assert_eq!(matcher_matches(*m, target), *want, "{m:?} vs {target}");
    }
}

// ---- claw-code hook cases, on the protocol

#[test]
fn claw_exit_code_zero_captures_stdout() {
    let o = interpret(HookEvent::PreToolUse, &HookRun { exit: Some(0), stdout: "pre ok".into(), ..HookRun::default() });
    assert_eq!(o.notes, ["pre ok"]);
    assert_eq!(o.permission, None);
}

#[test]
fn claw_denies_exit_code_two_and_reports_other_failures() {
    let o = interpret(HookEvent::PreToolUse, &HookRun { exit: Some(2), stderr: "blocked by hook".into(), ..HookRun::default() });
    assert!(matches!(o.permission, Some(HookPermission::Deny(_))));
    let o = interpret(HookEvent::PreToolUse, &HookRun { exit: Some(1), stdout: "warning hook".into(), ..HookRun::default() });
    assert_eq!(o.permission, None, "a failing hook does not block");
    assert!(o.notes[0].contains("warning hook"));
}

#[test]
fn claw_parses_permission_override_and_updated_input() {
    let o = ok_json(
        HookEvent::PreToolUse,
        json!({"systemMessage": "updated", "hookSpecificOutput": {"permissionDecision": "allow", "permissionDecisionReason": "hook ok", "updatedInput": {"command": "git status"}}}),
    );
    assert_eq!(o.permission, Some(HookPermission::Allow(Some("hook ok".into()))));
    assert_eq!(o.updated_input, Some(json!({"command": "git status"})));
    assert_eq!(o.system_messages, ["updated"]);
}

#[test]
fn claw_malformed_output_reports_a_diagnostic_with_previews() {
    let o = interpret(
        HookEvent::PreToolUse,
        &HookRun {
            command: "printf '{not-json'".into(),
            exit: Some(0),
            stdout: "{not-json\nsecond line".into(),
            stderr: "stderr warning".into(),
            tool: Some("Edit".into()),
            ..HookRun::default()
        },
    );
    let d = o.diagnostics.join("\n");
    for want in ["hook_invalid_json:", "phase=PreToolUse", "tool=Edit", "command=printf '{not-json'", "key must be a string", "stdout_preview={not-json second line", "stderr_preview=stderr warning"] {
        assert!(d.contains(want), "{want} in {d}");
    }
    assert_eq!(o.permission, None);
}

#[test]
fn outcomes_merge_with_deny_first() {
    let mut a = ok_json(HookEvent::PreToolUse, json!({"hookSpecificOutput": {"permissionDecision": "allow"}}));
    a.merge(ok_json(HookEvent::PreToolUse, json!({"hookSpecificOutput": {"permissionDecision": "deny", "permissionDecisionReason": "x"}})));
    a.merge(ok_json(HookEvent::PreToolUse, json!({"hookSpecificOutput": {"permissionDecision": "ask"}})));
    assert_eq!(a.permission, Some(HookPermission::Deny("x".into())));
    let t = interpret(HookEvent::PreToolUse, &HookRun { timed_out: true, command: "sleep 99".into(), ..HookRun::default() });
    assert!(t.diagnostics[0].contains("timed out"));
}
