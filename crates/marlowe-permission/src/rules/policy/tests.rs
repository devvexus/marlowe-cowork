//! The decision table per mode (design 2.3 M1-M5), and claw-code's permission cases
//! (ultraworkers/claw-code `rust/crates/runtime/src/permissions.rs` tests at
//! 08106b0c3771ef5b4a5aa176acccd460e88b7325, MIT) mapped onto Marlowe's modes: their
//! `ReadOnly`/`WorkspaceWrite`/`DangerFullAccess` are `plan`/`default`/`bypass`, their prompter is
//! an Ask, their `denied_tools` a deny rule.

use std::path::PathBuf;

use super::*;
use crate::rules::{Kind, RuleSet, Subject};
use crate::shell::{analyze, Dialect, ShellCtx};

fn root() -> PathBuf {
    PathBuf::from(if cfg!(windows) { r"C:\ws\proj" } else { "/ws/proj" })
}

fn sh(cmd: &str) -> Analysis {
    let roots = [root()];
    analyze(Dialect::Bash, cmd, &ShellCtx::workspace(root(), &roots))
}

/// A shell call's verdict in `mode` under `rules`.
fn shell(mode: Mode, rules: &RuleSet, cmd: &str) -> Verdict {
    let a = sh(cmd);
    let mut s = Subject::new("bash", Kind::Shell);
    s.shell = Some(&a);
    let mut f = Facts::new(mode, ToolSays::Ask(format!("bash: {cmd}")));
    f.runs = true;
    f.writes = true;
    f.shell = Some(&a);
    f.new_hosts = a.hosts.clone();
    f.rule = rules.evaluate(&s);
    decide(&f)
}

/// A file write inside (`inside`) or outside the workspace.
fn edit(mode: Mode, inside: bool) -> Verdict {
    let tool = if inside { ToolSays::Free } else { ToolSays::Ask("edit ../x (outside the workspace)".into()) };
    let mut f = Facts::new(mode, tool);
    f.writes = true;
    decide(&f)
}

fn read(mode: Mode) -> Verdict {
    decide(&Facts::new(mode, ToolSays::Free))
}

fn kind(v: &Verdict) -> &'static str {
    match v {
        Verdict::Free => "free",
        Verdict::Ask { .. } => "ask",
        Verdict::Deny(_) => "deny",
    }
}

#[test]
fn parity_m1_to_m5_mode_table() {
    use Mode::*;
    let none = RuleSet::new();
    // (input, expected per mode: plan, default, accept_edits, bypass, dont_ask)
    let rows: &[(&str, [&str; 5])] = &[
        ("read src/lib.rs", ["free", "free", "free", "free", "free"]),
        ("edit inside", ["deny", "free", "free", "free", "free"]),
        ("edit outside", ["deny", "ask", "ask", "ask", "deny"]),
        ("ls -la", ["ask", "ask", "free", "free", "deny"]),
        ("git status", ["ask", "ask", "free", "free", "deny"]),
        ("mkdir -p src/new && touch src/new/mod.rs", ["deny", "ask", "free", "free", "deny"]),
        ("cargo test", ["deny", "ask", "ask", "free", "deny"]),
        ("rm -rf target", ["deny", "ask", "ask", "free", "deny"]),
        ("rm -rf ../x", ["deny", "ask", "ask", "ask", "deny"]),
        ("curl https://new.host/x", ["deny", "ask", "ask", "ask", "deny"]),
        ("git push", ["deny", "ask", "ask", "ask", "deny"]),
        ("git push --force", ["deny", "deny", "deny", "deny", "deny"]),
        ("git commit --no-verify -m x", ["deny", "deny", "deny", "deny", "deny"]),
    ];
    for (input, want) in rows {
        for (i, mode) in [Plan, Default, AcceptEdits, Bypass, DontAsk].into_iter().enumerate() {
            let v = match *input {
                "read src/lib.rs" => read(mode),
                "edit inside" => edit(mode, true),
                "edit outside" => edit(mode, false),
                cmd => shell(mode, &none, cmd),
            };
            assert_eq!(kind(&v), want[i], "{input} in {mode:?}: {v:?}");
        }
    }
}

#[test]
fn parity_m1_plan_blocks_every_write_tool() {
    for (writes, runs) in [(true, false), (false, true), (true, true)] {
        let mut f = Facts::new(Mode::Plan, ToolSays::Free);
        f.writes = writes;
        f.runs = runs;
        assert!(matches!(decide(&f), Verdict::Deny(why) if why.contains("plan mode")));
    }
    // An allow rule or a hook does not lift it.
    let mut f = Facts::new(Mode::Plan, ToolSays::Free);
    f.writes = true;
    f.rule = Some(Matched { action: Action::Allow, rule: "edit".into() });
    f.hook = Some(HookPermission::Allow(None));
    assert!(matches!(decide(&f), Verdict::Deny(_)));
}

#[test]
fn parity_m5_dont_ask_denies_with_a_reason() {
    let v = shell(Mode::DontAsk, &RuleSet::new(), "cargo build");
    match v {
        Verdict::Deny(why) => assert!(why.contains("dont_ask") && why.contains("cargo build"), "{why}"),
        other => panic!("{other:?}"),
    }
    let mut allowed = RuleSet::new();
    allowed.add_text(Action::Allow, "bash(cargo build:*)").unwrap();
    assert_eq!(shell(Mode::DontAsk, &allowed, "cargo build --release"), Verdict::Free, "allowed calls still run");
}

#[test]
fn tripwires_are_never_widened_and_rules_are() {
    let none = RuleSet::new();
    assert!(matches!(shell(Mode::Default, &none, "git push"), Verdict::Ask { tripwire: true, .. }));
    assert!(matches!(shell(Mode::Default, &none, "cargo test"), Verdict::Ask { tripwire: false, .. }));
    // An allow rule does not lift a tripwire.
    let mut r = RuleSet::new();
    r.add_text(Action::Allow, "bash(git push:*)").unwrap();
    assert!(matches!(shell(Mode::Bypass, &r, "git push origin main"), Verdict::Ask { tripwire: true, .. }));
    // Accept-edits frees project commands.
    let a = sh("just test");
    let mut f = Facts::new(Mode::AcceptEdits, ToolSays::Ask("bash: just test".into()));
    f.runs = true;
    f.shell = Some(&a);
    assert!(matches!(decide(&f), Verdict::Ask { .. }));
    f.project_command = true;
    assert_eq!(decide(&f), Verdict::Free);
    // After untrusted content, running anything asks even in bypass.
    let a = sh("cargo test");
    let mut f = Facts::new(Mode::Bypass, ToolSays::Ask("bash: cargo test".into()));
    f.runs = true;
    f.shell = Some(&a);
    f.tainted = true;
    assert!(matches!(decide(&f), Verdict::Ask { tripwire: true, .. }));
    // A new host from a fetcher's analysis is a tripwire; an MCP tool's own Ask is not.
    let mut f = Facts::new(Mode::Bypass, ToolSays::Ask("mcp__srv__x".into()));
    f.mcp = true;
    assert_eq!(decide(&f), Verdict::Free);
    f.new_hosts = vec!["evil.example".into()];
    assert!(matches!(decide(&f), Verdict::Ask { tripwire: true, .. }));
}

// ---- claw-code fixtures

#[test]
fn claw_allows_tools_when_active_mode_meets_requirement() {
    // read in plan, write in default: no prompt.
    assert_eq!(read(Mode::Plan), Verdict::Free);
    assert_eq!(edit(Mode::Default, true), Verdict::Free);
}

#[test]
fn claw_denies_read_only_escalations_without_prompt() {
    assert!(matches!(edit(Mode::Plan, true), Verdict::Deny(_)));
    assert!(matches!(shell(Mode::Plan, &RuleSet::new(), "echo hi > x"), Verdict::Deny(_)));
}

#[test]
fn claw_prompts_for_workspace_write_to_danger_full_access_escalation() {
    assert!(matches!(shell(Mode::Default, &RuleSet::new(), "echo hi && cargo run"), Verdict::Ask { .. }));
}

#[test]
fn claw_applies_rule_based_denials_and_allows() {
    let mut r = RuleSet::new();
    r.add_text(Action::Allow, "bash(git:*)").unwrap();
    r.add_text(Action::Deny, "bash(rm -rf:*)").unwrap();
    assert_eq!(shell(Mode::Default, &r, "git status"), Verdict::Free);
    assert!(matches!(shell(Mode::Default, &r, "rm -rf /tmp/x"), Verdict::Deny(why) if why.contains("denied by the rule")));
}

#[test]
fn claw_denied_tools_denies_listed_tools_unconditionally() {
    let mut r = RuleSet::new();
    r.add_text(Action::Deny, "bash").unwrap();
    assert!(matches!(shell(Mode::Bypass, &r, "echo hello"), Verdict::Deny(why) if why.contains("`bash`")));
}

#[test]
fn claw_ask_rules_force_prompt_even_when_mode_allows() {
    let mut r = RuleSet::new();
    r.add_text(Action::Ask, "bash(git:*)").unwrap();
    assert!(matches!(shell(Mode::Bypass, &r, "git status"), Verdict::Ask { reason, .. } if reason.contains("ask rule") || reason.contains("rule `bash(git:*)`")));
}

#[test]
fn claw_hook_allow_still_respects_ask_rules() {
    let a = sh("git status");
    let mut r = RuleSet::new();
    r.add_text(Action::Ask, "bash(git:*)").unwrap();
    let mut s = Subject::new("bash", Kind::Shell);
    s.shell = Some(&a);
    let mut f = Facts::new(Mode::Default, ToolSays::Ask("bash: git status".into()));
    f.runs = true;
    f.shell = Some(&a);
    f.rule = r.evaluate(&s);
    f.hook = Some(HookPermission::Allow(Some("hook approved".into())));
    assert!(matches!(decide(&f), Verdict::Ask { .. }));
    // Without the ask rule the hook's allow frees it.
    f.rule = None;
    assert_eq!(decide(&f), Verdict::Free);
}

#[test]
fn claw_hook_deny_short_circuits_permission_flow() {
    let mut f = Facts::new(Mode::Bypass, ToolSays::Free);
    f.hook = Some(HookPermission::Deny("blocked by hook".into()));
    assert_eq!(decide(&f), Verdict::Deny("blocked by hook".into()));
}

#[test]
fn claw_hook_ask_forces_prompt() {
    let mut f = Facts::new(Mode::Bypass, ToolSays::Free);
    f.hook = Some(HookPermission::Ask(Some("hook requested confirmation".into())));
    assert_eq!(decide(&f), Verdict::Ask { reason: "hook requested confirmation".into(), tripwire: false });
}
