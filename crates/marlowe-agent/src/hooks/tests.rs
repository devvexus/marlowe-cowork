//! Hooks end to end (design 2.6 X1-X9): real command hooks through Git Bash (`sh` elsewhere),
//! the call runner and the loop with a scripted provider. Includes claw-code's hook runner cases
//! (ultraworkers/claw-code `rust/crates/runtime/src/hooks.rs` at
//! 08106b0c3771ef5b4a5aa176acccd460e88b7325, MIT) on Claude Code's exit-code semantics.

use std::sync::Arc;
use std::time::{Duration, Instant};

use marlowe_proto::{ApprovalDecision, PermissionMode, SessionId};
use marlowe_settings::hooks::{HookEntry, HookEvent, HookKind};
use marlowe_settings::layers::Source;
use marlowe_settings::protocol::{EventData, HookContext};
use serde_json::{json, Value};

use super::*;
use crate::approval::tests::{agent, batch, call, run_turn, sh, text_step, FakeHost, Recording, Script, Tmp};
use crate::approval::Permissions;
use crate::middleware::{NotificationKind, ToolMiddleware, TurnObserver};
use crate::tools::Services;
use crate::Conversation;

fn hook(event: HookEvent, matcher: Option<&str>, command: &str) -> HookEntry {
    HookEntry { event, matcher: matcher.map(str::to_string), kind: HookKind::Command(command.into()), timeout: 20, source: Source::User, path: None }
}

struct Setup {
    t: Tmp,
    runner: Arc<HookRunner>,
    mw: Arc<HookMiddleware>,
    perms: Permissions,
}

impl Setup {
    fn root(&self) -> std::path::PathBuf {
        self.t.0.join("proj")
    }

    fn services(&self) -> Services {
        Services::new().with(self.perms.clone()).with(self.runner.clone())
    }

    fn convo(&self) -> Conversation {
        Conversation::new(SessionId("s".into()), Some(self.root()))
    }

    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.root().join(name)).unwrap_or_default()
    }
}

fn setup(tag: &str, mode: PermissionMode, hooks: Vec<HookEntry>) -> Setup {
    let t = Tmp::new(tag);
    let root = t.0.join("proj");
    std::fs::create_dir_all(&root).unwrap();
    let perms = Permissions::with_parser(&t.0.join("home"), Some(&root), None, None);
    perms.set_mode(mode);
    let runner = Arc::new(HookRunner::new(hooks, Some(root), SessionId("s".into())).with_permissions(perms.clone()));
    let mw = Arc::new(HookMiddleware::new(runner.clone()));
    Setup { t, runner, mw, perms }
}

fn ctx() -> HookContext {
    HookContext { session_id: "s".into(), cwd: ".".into(), permission_mode: "default".into(), ..HookContext::default() }
}

fn pre(tool: &str, args: Value) -> EventData {
    EventData::PreToolUse { tool_name: tool.into(), tool_input: args, tool_use_id: "c1".into() }
}

/// A hook that prints `out` as its JSON answer.
fn json_hook(event: HookEvent, matcher: Option<&str>, out: Value) -> HookEntry {
    hook(event, matcher, &format!("printf '%s' '{}'", out.to_string().replace('\'', "")))
}

// ---- claw-code's runner cases

#[test]
fn claw_allows_exit_code_zero_and_captures_stdout() {
    let s = setup("claw0", PermissionMode::Default, vec![hook(HookEvent::PreToolUse, None, "printf 'pre ok'")]);
    let o = s.runner.run(&pre("read", json!({"path": "README.md"})), &ctx(), &|| false);
    assert_eq!(o.notes, ["pre ok"]);
    assert_eq!(o.permission, None);
}

#[test]
fn claw_object_style_hook_matchers_filter_runtime_execution() {
    let s = setup(
        "clawm",
        PermissionMode::Default,
        vec![
            hook(HookEvent::PreToolUse, None, "printf 'legacy'"),
            hook(HookEvent::PreToolUse, Some("Bash"), "printf 'bash only'"),
            hook(HookEvent::PreToolUse, Some("Read*"), "printf 'read only'"),
        ],
    );
    assert_eq!(s.runner.run(&pre("read", json!({})), &ctx(), &|| false).notes, ["legacy", "read only"]);
    assert_eq!(s.runner.run(&pre("bash", json!({"command": "pwd"})), &ctx(), &|| false).notes, ["legacy", "bash only"]);
}

#[test]
fn claw_hooks_run_in_configured_order_and_a_failure_does_not_block() {
    let s = setup(
        "clawo",
        PermissionMode::Default,
        vec![
            hook(HookEvent::PreToolUse, None, "printf 'first'"),
            hook(HookEvent::PreToolUse, None, "printf 'broken' >&2; exit 1"),
            hook(HookEvent::PreToolUse, None, "printf 'second'"),
        ],
    );
    let o = s.runner.run(&pre("edit", json!({"path": "src/lib.rs"})), &ctx(), &|| false);
    assert_eq!(o.notes[0], "first");
    assert!(o.notes[1].contains("broken") && o.notes[1].contains("exit 1"), "{:?}", o.notes);
    assert_eq!(o.notes[2], "second", "a non-blocking error lets later hooks run (Claude Code)");
    assert_eq!(o.permission, None);
}

#[test]
fn claw_malformed_output_reports_a_diagnostic() {
    let s = setup("clawj", PermissionMode::Default, vec![hook(HookEvent::PreToolUse, None, "printf '{not-json'; printf 'stderr warning' >&2")]);
    let o = s.runner.run(&pre("edit", json!({})), &ctx(), &|| false);
    let d = o.diagnostics.join("\n");
    assert!(d.contains("hook_invalid_json") && d.contains("phase=PreToolUse") && d.contains("tool=edit") && d.contains("stderr_preview=stderr warning"), "{d}");
    assert!(s.runner.take_messages().iter().any(|m| m.contains("hook_invalid_json")), "diagnostics reach the user");
}

#[test]
fn claw_abort_cancels_a_long_running_hook_and_timeouts_kill_it() {
    let s = setup("clawc", PermissionMode::Default, vec![hook(HookEvent::PreToolUse, None, "sleep 30")]);
    let start = Instant::now();
    let o = s.runner.run(&pre("bash", json!({})), &ctx(), &|| start.elapsed() > Duration::from_millis(200));
    assert!(o.diagnostics[0].contains("cancelled"), "{:?}", o.diagnostics);
    assert!(start.elapsed() < Duration::from_secs(10));
    let mut h = hook(HookEvent::PreToolUse, None, "sleep 30");
    h.timeout = 1;
    s.runner.set_hooks(vec![h]);
    let start = Instant::now();
    let o = s.runner.run(&pre("bash", json!({})), &ctx(), &|| false);
    assert!(o.diagnostics[0].contains("timed out"), "{:?}", o.diagnostics);
    assert!(start.elapsed() < Duration::from_secs(10));
}

// ---- X1 PreToolUse

fn run_pre(s: &Setup, gate: &Recording, calls: &[ToolCall]) -> (Vec<marlowe_model::ToolResult>, Arc<FakeHost>) {
    let host = FakeHost::new(&s.root());
    let mut convo = s.convo();
    let mw: Vec<Arc<dyn ToolMiddleware>> = vec![s.mw.clone()];
    let r = batch(&*host, &mw, &s.services(), gate, calls, &mut convo);
    (r, host)
}

#[test]
fn parity_x1_pre_tool_use_exit_2_denies() {
    let s = setup("x1d", PermissionMode::Bypass, vec![hook(HookEvent::PreToolUse, Some("Bash"), "echo 'no rm here' >&2; exit 2")]);
    let (r, host) = run_pre(&s, &Recording::new(&[]), &[sh("1", "rm -rf build"), call("2", "edit", json!({"path": "a.rs"}))]);
    assert!(r[0].content.contains("PreToolUse hook blocked it: no rm here"), "{}", r[0].content);
    assert_eq!(host.ran(), ["edit a.rs"], "the matcher only picked the shell");
}

#[test]
fn parity_x1_pre_tool_use_decision_fields() {
    let pd = |d: &str| {
        json_hook(
            HookEvent::PreToolUse,
            Some("Bash"),
            json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": d, "permissionDecisionReason": "hook says"}}),
        )
    };
    // allow: no prompt in default mode.
    let s = setup("x1a", PermissionMode::Default, vec![pd("allow")]);
    let gate = Recording::new(&[]);
    let (_, host) = run_pre(&s, &gate, &[sh("1", "cargo build")]);
    assert_eq!(host.ran(), ["bash cargo build"]);
    assert!(gate.asked().is_empty());
    // ask: a prompt even in bypass, with the hook's reason.
    let s = setup("x1k", PermissionMode::Bypass, vec![pd("ask")]);
    let gate = Recording::new(&[ApprovalDecision::Allow]);
    run_pre(&s, &gate, &[sh("1", "cargo build")]);
    assert!(gate.asked()[0].contains("hook says"), "{:?}", gate.asked());
    // deny: refused with the reason.
    let s = setup("x1n", PermissionMode::Bypass, vec![pd("deny")]);
    let (r, host) = run_pre(&s, &Recording::new(&[]), &[sh("1", "cargo build")]);
    assert!(r[0].content.contains("hook says") && host.ran().is_empty());
}

#[test]
fn parity_x1_pre_tool_use_updated_input_and_context() {
    let out = json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "updatedInput": {"command": "git status"}, "additionalContext": "status is cheap"}, "systemMessage": "rewrote it"});
    let s = setup("x1u", PermissionMode::Bypass, vec![json_hook(HookEvent::PreToolUse, Some("Bash"), out)]);
    let (r, host) = run_pre(&s, &Recording::new(&[]), &[sh("1", "git push --force")]);
    assert_eq!(host.ran(), ["bash git status"], "the gate judged and ran the rewritten call");
    assert!(r[0].content.contains("status is cheap"));
    assert!(s.runner.take_messages().contains(&"rewrote it".to_string()));
}

#[test]
fn parity_x1_pre_tool_use_continue_false_stops_the_turn() {
    let out = json!({"continue": false, "stopReason": "frozen for release"});
    let s = setup("x1c", PermissionMode::Bypass, vec![json_hook(HookEvent::PreToolUse, None, out)]);
    let host = FakeHost::new(&s.root());
    let mut convo = s.convo();
    let mw: Vec<Arc<dyn ToolMiddleware>> = vec![s.mw.clone()];
    let cancel = marlowe_model::CancelToken::new();
    let gate = Recording::new(&[]);
    let r = crate::run_tools::run_calls(&*host, &mw, &s.services(), &*gate, &[sh("1", "ls")], &mut convo, 0, &cancel, &mut |_| {});
    assert!(r[0].content.contains("frozen for release"));
    assert!(cancel.is_cancelled());
}

// ---- X2 PostToolUse, PostToolUseFailure

#[test]
fn parity_x2_post_tool_use_feeds_back() {
    let s = setup(
        "x2",
        PermissionMode::Bypass,
        vec![
            json_hook(
                HookEvent::PostToolUse,
                Some("Edit"),
                json!({"decision": "block", "reason": "rustfmt failed", "hookSpecificOutput": {"hookEventName": "PostToolUse", "additionalContext": "run cargo fmt"}}),
            ),
            hook(HookEvent::PostToolUseFailure, None, "cat > failure.json; echo 'retry with --locked' >&2; exit 2"),
        ],
    );
    let (r, _) = run_pre(&s, &Recording::new(&[]), &[call("1", "edit", json!({"path": "a.rs"})), call("2", "fail", json!({}))]);
    assert!(r[0].content.contains("rustfmt failed") && r[0].content.contains("run cargo fmt"), "{}", r[0].content);
    assert!(r[1].content.contains("retry with --locked"), "{}", r[1].content);
    let input: Value = serde_json::from_str(&s.read("failure.json")).unwrap();
    assert_eq!(input["hook_event_name"], "PostToolUseFailure");
    assert_eq!(input["error"], "it failed");
}

// ---- X3 UserPromptSubmit

#[test]
fn parity_x3_user_prompt_submit_blocks_or_adds_context() {
    let script = "if grep -q secret; then echo 'no secrets in prompts' >&2; exit 2; fi; echo 'today is release day'";
    let s = setup("x3", PermissionMode::Default, vec![hook(HookEvent::UserPromptSubmit, None, script)]);
    let provider = Script::new(vec![text_step("ok")]);
    let mut a = agent(provider.clone(), FakeHost::new(&s.root()), Recording::new(&[]), s.services());
    a.middleware.push(s.mw.clone());
    let mut convo = s.convo();
    let blocked = run_turn(&a, &mut convo, "my secret is 42");
    assert!(blocked.blocked && blocked.error.as_deref() == Some("no secrets in prompts"), "{blocked:?}");
    assert!(convo.messages.is_empty(), "a blocked prompt is not kept");
    let out = run_turn(&a, &mut convo, "ship it");
    assert!(!out.blocked);
    let sent = format!("{:?}", provider.requests.lock().unwrap()[0].messages);
    assert!(sent.contains("today is release day"), "{sent}");
}

// ---- X4 Stop, SubagentStop

#[test]
fn parity_x4_stop_hook_forces_one_more_step() {
    let script = "cat > stop-input.json; if [ -f once ]; then exit 0; fi; touch once; echo 'run the tests first' >&2; exit 2";
    let s = setup("x4", PermissionMode::Default, vec![hook(HookEvent::Stop, None, script)]);
    let provider = Script::new(vec![text_step("done"), text_step("tests pass, done")]);
    let mut a = agent(provider.clone(), FakeHost::new(&s.root()), Recording::new(&[]), s.services());
    a.middleware.push(s.mw.clone());
    let mut convo = s.convo();
    let out = run_turn(&a, &mut convo, "fix it");
    assert_eq!(provider.requests.lock().unwrap().len(), 2, "the stop hook kept the agent going once");
    assert!(format!("{:?}", provider.requests.lock().unwrap()[1].messages).contains("run the tests first"));
    assert_eq!(out.stop, crate::TurnStop::Done);
    let last: Value = serde_json::from_str(&s.read("stop-input.json")).unwrap();
    assert_eq!(last["stop_hook_active"], true, "the second Stop says it is already continuing");
    assert_eq!(last["last_assistant_message"], "tests pass, done");
}

#[test]
fn parity_x4_subagent_stop_through_a_prompt_hook_never_loops_forever() {
    struct Judge;
    impl PromptHookEvaluator for Judge {
        fn evaluate(&self, prompt: &str) -> Result<String, String> {
            assert!(prompt.contains("SubagentStop"), "$ARGUMENTS is the event JSON: {prompt}");
            Ok(r#"{"decision":"block","reason":"cite a file"}"#.into())
        }
    }
    let t = Tmp::new("x4s");
    let mut h = hook(HookEvent::SubagentStop, None, "");
    h.kind = HookKind::Prompt("Is it done? $ARGUMENTS".into());
    let runner = Arc::new(HookRunner::new(vec![h], Some(t.0.clone()), SessionId("s".into())).with_prompt_evaluator(Arc::new(Judge)));
    let sub = HookMiddleware::for_subagent(runner, "a1", "explore");
    let mut convo = Conversation::new(SessionId("s".into()), None);
    let info = crate::StepInfo { turn: 0, step: 1 };
    assert!(matches!(sub.before_final(info, &mut convo, "found it"), crate::FinalFlow::Continue(t) if t.contains("cite a file")));
    for _ in 1..MAX_FORCED_CONTINUES {
        assert!(matches!(sub.before_final(info, &mut convo, "x"), crate::FinalFlow::Continue(_)));
    }
    assert_eq!(sub.before_final(info, &mut convo, "x"), crate::FinalFlow::Accept, "never forever");
}

// ---- X5 SessionStart, SessionEnd

#[test]
fn parity_x5_session_start_and_end() {
    let s = setup(
        "x5",
        PermissionMode::Default,
        vec![hook(HookEvent::SessionStart, Some("startup"), "echo 'on branch main'"), hook(HookEvent::SessionEnd, None, "cat > end.json")],
    );
    let provider = Script::new(vec![text_step("hi")]);
    let mut a = agent(provider.clone(), FakeHost::new(&s.root()), Recording::new(&[]), s.services());
    a.middleware.push(s.mw.clone());
    let mut convo = s.convo();
    s.mw.on_session_start(&convo);
    run_turn(&a, &mut convo, "hello");
    assert!(format!("{:?}", provider.requests.lock().unwrap()[0].messages).contains("on branch main"));
    s.runner.session_end("other");
    let end: Value = serde_json::from_str(&s.read("end.json")).unwrap();
    assert_eq!(end["hook_event_name"], "SessionEnd");
    assert_eq!(end["reason"], "other");
}

// ---- X6 Notification

#[test]
fn parity_x6_notification_runs_off_the_asking_thread() {
    let s = setup("x6", PermissionMode::Default, vec![hook(HookEvent::Notification, Some("permission_prompt"), "cat > note.tmp && mv note.tmp note.json")]);
    let note = crate::Notification { kind: NotificationKind::PermissionPrompt, message: "bash: cargo build".into() };
    s.mw.on_notification(&SessionId("s".into()), &note);
    let start = Instant::now();
    while s.read("note.json").is_empty() && start.elapsed() < Duration::from_secs(15) {
        std::thread::sleep(Duration::from_millis(50));
    }
    let v: Value = serde_json::from_str(&s.read("note.json")).unwrap();
    assert_eq!(v["notification_type"], "permission_prompt");
    assert_eq!(v["message"], "bash: cargo build");
}

// ---- X7 PreCompact

#[test]
fn parity_x7_pre_compact() {
    let s = setup("x7", PermissionMode::Default, vec![hook(HookEvent::PreCompact, Some("manual"), "cat > compact.json")]);
    let mut convo = s.convo();
    s.mw.before_compact(&mut convo, Some("keep the API notes"));
    let v: Value = serde_json::from_str(&s.read("compact.json")).unwrap();
    assert_eq!(v["trigger"], "manual");
    assert_eq!(v["custom_instructions"], "keep the API notes");
    std::fs::remove_file(s.root().join("compact.json")).unwrap();
    s.mw.before_compact(&mut convo, None);
    assert!(s.read("compact.json").is_empty(), "the matcher picked manual compactions only");
    s.runner.set_hooks(vec![hook(HookEvent::PostCompact, None, "cat > post.json")]);
    s.runner.post_compact("auto");
    let v: Value = serde_json::from_str(&s.read("post.json")).unwrap();
    assert_eq!(v["hook_event_name"], "PostCompact");
}

// ---- X8 PermissionRequest

#[test]
fn parity_x8_permission_request_hook_answers_the_modal() {
    let allow = json!({"hookSpecificOutput": {"hookEventName": "PermissionRequest", "decision": {"behavior": "allow"}}});
    let s = setup("x8a", PermissionMode::Default, vec![json_hook(HookEvent::PermissionRequest, Some("Bash"), allow)]);
    let gate = Recording::new(&[]);
    let (_, host) = run_pre(&s, &gate, &[sh("1", "cargo build")]);
    assert_eq!(host.ran(), ["bash cargo build"]);
    assert!(gate.asked().is_empty(), "the hook answered; the user was not asked");
    let deny = json!({"hookSpecificOutput": {"hookEventName": "PermissionRequest", "decision": {"behavior": "deny", "message": "not on Fridays"}}});
    let s = setup("x8d", PermissionMode::Default, vec![json_hook(HookEvent::PermissionRequest, None, deny)]);
    let gate = Recording::new(&[]);
    let (r, host) = run_pre(&s, &gate, &[sh("1", "cargo build")]);
    assert!(host.ran().is_empty() && r[0].content.contains("not on Fridays"));
    assert!(gate.asked().is_empty());
}

#[test]
fn parity_x8_a_permission_request_allow_never_answers_a_tripwire() {
    let allow = json!({"hookSpecificOutput": {"hookEventName": "PermissionRequest", "decision": {"behavior": "allow"}}});
    for mode in [PermissionMode::Default, PermissionMode::Bypass] {
        let s = setup("x8t", mode, vec![json_hook(HookEvent::PermissionRequest, None, allow.clone())]);
        let gate = Recording::new(&[]);
        let (r, host) = run_pre(&s, &gate, &[sh("1", "git push origin main"), sh("2", "rm -rf ../outside")]);
        assert_eq!(gate.asked().len(), 2, "{mode:?}: the user is asked for each tripwire");
        assert!(host.ran().is_empty(), "{mode:?}: nobody allowed them: {r:?}");
    }
    // Its deny still stops a tripwire before the user is bothered.
    let deny = json!({"hookSpecificOutput": {"hookEventName": "PermissionRequest", "decision": {"behavior": "deny", "message": "no pushes"}}});
    let s = setup("x8u", PermissionMode::Default, vec![json_hook(HookEvent::PermissionRequest, None, deny)]);
    let gate = Recording::new(&[]);
    let (r, _) = run_pre(&s, &gate, &[sh("1", "git push origin main")]);
    assert!(gate.asked().is_empty() && r[0].content.contains("no pushes"));
}

// ---- X9 environment and input

#[test]
fn parity_x9_claude_code_argument_names_both_ways() {
    let out = json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "updatedInput": {"file_path": "b.rs"}}});
    let s = setup(
        "x9n",
        PermissionMode::Bypass,
        vec![json_hook(HookEvent::PreToolUse, Some("Edit"), out), hook(HookEvent::PostToolUse, Some("Bash"), "cat > post.json")],
    );
    let (_, host) = run_pre(&s, &Recording::new(&[]), &[call("1", "edit", json!({"path": "a.rs"})), sh("2", "ls")]);
    assert_eq!(host.ran(), ["edit b.rs", "bash ls"], "`file_path` from a Claude Code hook rewrites `path`");
    let v: Value = serde_json::from_str(&s.read("post.json")).unwrap();
    assert_eq!(v["tool_response"]["stdout"], "done", "tool_response is an object, as Claude Code sends");
    assert_eq!(v["tool_response"]["output"], "done");
}

#[test]
fn a_hook_that_leaves_a_background_process_does_not_hold_the_turn() {
    let s = setup("bg", PermissionMode::Default, vec![hook(HookEvent::PreToolUse, None, "sleep 30 & echo ok")]);
    let start = Instant::now();
    let o = s.runner.run(&pre("bash", json!({"command": "ls"})), &ctx(), &|| false);
    assert!(start.elapsed() < Duration::from_secs(10), "took {:?}", start.elapsed());
    assert_eq!(o.notes, ["ok"], "output written before it exited is kept");
}

/// Records what the memory engine is asked with.
struct Mem(std::sync::Mutex<Vec<String>>);

impl crate::MemoryHook for Mem {
    fn before_turn(&self, _session: &SessionId, query: &str) -> crate::Recollection {
        self.0.lock().unwrap().push(query.to_string());
        crate::Recollection::default()
    }
    fn after_turn(&self, _session: &SessionId, _user: &str, _assistant: &str) {}
}

#[test]
fn hook_context_reaches_the_model_but_never_the_memory_as_user_words() {
    let s = setup(
        "x3m",
        PermissionMode::Default,
        vec![hook(HookEvent::UserPromptSubmit, None, "echo 'HOOKX from git log'"), hook(HookEvent::SessionStart, None, "echo 'HOOKY on branch'")],
    );
    let provider = Script::new(vec![text_step("ok")]);
    let mut a = agent(provider.clone(), FakeHost::new(&s.root()), Recording::new(&[]), s.services());
    a.middleware.push(s.mw.clone());
    let mem = Arc::new(Mem(std::sync::Mutex::new(Vec::new())));
    a.memory = mem.clone();
    let mut convo = s.convo();
    s.mw.on_session_start(&convo);
    run_turn(&a, &mut convo, "ship it");
    assert_eq!(*mem.0.lock().unwrap(), ["ship it"], "the memory engine ingests the user's own text only");
    let sent = format!("{:?}", provider.requests.lock().unwrap()[0].messages);
    assert!(sent.contains("HOOKX") && sent.contains("HOOKY"), "{sent}");
    assert!(convo.pending_context.is_empty());
}

#[test]
fn parity_x9_project_dir_env_and_stdin_input() {
    let script = "cat > in.json; printf '%s|%s' \"$CLAUDE_PROJECT_DIR\" \"$MARLOWE_PROJECT_DIR\"";
    let s = setup("x9", PermissionMode::AcceptEdits, vec![hook(HookEvent::PreToolUse, Some("Edit|Write"), script)]);
    let o = s.runner.run(&pre("edit", json!({"path": "a.rs"})), &s.runner.context(Some(3)), &|| false);
    let root = s.root().display().to_string();
    assert_eq!(o.notes, [format!("{root}|{root}")]);
    let v: Value = serde_json::from_str(&s.read("in.json")).unwrap();
    assert_eq!(v["tool_name"], "Edit", "Claude Code's tool name");
    assert_eq!(v["tool_input"]["file_path"], "a.rs", "Claude Code's argument name");
    assert_eq!(v["tool_input"]["path"], "a.rs", "and Marlowe's");
    assert_eq!(v["permission_mode"], "acceptEdits");
    assert_eq!(v["turn_id"], "3");
    assert_eq!(v["cwd"], root.as_str());
    // A prompt hook with no model is reported, not run.
    let mut h = hook(HookEvent::PreToolUse, None, "");
    h.kind = HookKind::Prompt("judge".into());
    s.runner.set_hooks(vec![h]);
    let o = s.runner.run(&pre("edit", json!({})), &ctx(), &|| false);
    assert!(o.diagnostics[0].contains("no model"), "{:?}", o.diagnostics);
}
