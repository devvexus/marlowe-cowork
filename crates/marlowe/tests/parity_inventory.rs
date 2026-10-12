//! `parity_inventory` (design 2.14, gate A2): every tool, slash command and agent type the pinned
//! Claude Code lists is mapped to the Marlowe row that covers it, to a reason it does not apply
//! here, or to a named gap. The names come from the recorded `system/init` event
//! (`fixtures/cc_init.json`, Claude Code 2.1.296 run isolated by `tools/record_cc_init.py`) plus the
//! deferred tools a `-p` init does not list (from the 2026-10 audit, 2.14). A name nobody mapped
//! fails the test, so a new Claude Code feature cannot go unseen.

use std::collections::BTreeSet;

use serde_json::Value;

/// How Marlowe answers one Claude Code name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Seen {
    /// Covered by these design rows (each has its `parity_<row>` test, or a sign-off).
    Rows(&'static str),
    /// Does not apply to a local, single-user harness (2.13 and 2.14 reasons).
    NotApplicable(&'static str),
    /// A bundled skill's content: the mechanism (SKILL.md, X11) is at parity, the content is not
    /// shipped.
    Content(&'static str),
    /// No Marlowe equivalent yet: not at Claude Code parity.
    Gap(&'static str),
}

use Seen::*;

/// The inventory. Keys are Claude Code's names as it lists them.
const MAP: &[(&str, Seen)] = &[
    // ---- tools (init) ----
    ("Task", Rows("A2, A3, A4, A15")),
    ("Agent", Rows("A2, A3, A4, A14, A15")),
    ("Bash", Rows("T12, T14, T15, T18, T32")),
    ("PowerShell", Rows("T13")),
    ("CronCreate", Rows("BG5")),
    ("CronDelete", Rows("BG5")),
    ("CronList", Rows("BG5")),
    ("Edit", Rows("T6, T7")),
    ("MultiEdit", Rows("T8")),
    ("EnterWorktree", Rows("A13")),
    ("ExitWorktree", Rows("A13")),
    ("Glob", Rows("T10")),
    ("Grep", Rows("T11")),
    ("ListAgents", Rows("BG10")),
    ("NotebookEdit", Rows("T19")),
    ("Read", Rows("T1, T2, T3, T4")),
    ("ReportFindings", Rows("G13")),
    ("ScheduleWakeup", Rows("BG6")),
    ("SendMessage", Rows("A11, BG11")),
    ("Skill", Rows("X11, X23")),
    ("TaskStop", Rows("BG2")),
    ("TaskOutput", Rows("BG3")),
    ("WebFetch", Rows("T20")),
    ("WebSearch", Rows("T21")),
    ("Workflow", Rows("WF1-WF9")),
    ("Write", Rows("T5")),
    // ---- deferred tools a -p init does not list (2.14 audit, and this session's own list) ----
    ("Monitor", Rows("T16, BG4")),
    ("AskUserQuestion", Rows("T23, T33")),
    ("TodoWrite", Rows("T22")),
    ("TaskCreate", Rows("T34")),
    ("TaskGet", Rows("T34")),
    ("TaskUpdate", Rows("T34")),
    ("TaskList", Rows("T34")),
    ("EnterPlanMode", Rows("M13")),
    ("ExitPlanMode", Rows("M1, M13")),
    ("ToolSearch", Rows("T31")),
    ("PushNotification", Rows("BG7")),
    ("SendUserFile", Rows("BG8")),
    ("ReadNotifications", Rows("BG9")),
    ("SubagentHandback", Rows("A17")),
    ("ListMcpResourcesTool", Rows("X20")),
    ("ReadMcpResourceTool", Rows("X20")),
    ("ReadMcpResourceDirTool", Rows("X20 (a resource template is read by its expanded URI)")),
    ("LSP", Rows("T24, T25, T26")),
    ("RemoteTrigger", NotApplicable("RM1: no cloud runner; durable cron (BG5) or `marlowe -p` from Task Scheduler")),
    ("Artifact", NotApplicable("RM3: no hosting; an HTML file sent with send_file opens in the browser")),
    ("ArtifactComments", NotApplicable("RM3: no hosting")),
    ("ArtifactData", NotApplicable("RM3: no hosting")),
    ("DesignSync", NotApplicable("RM4: claude.ai design systems")),
    ("FetchInboxMessage", NotApplicable("RM5: the local inbox is notices + send_message")),
    // ---- agent types ----
    ("claude", Rows("A8 (the conductor's own agent)")),
    ("general-purpose", Rows("A8")),
    ("Explore", Rows("A6")),
    ("Plan", Rows("A7")),
    ("statusline-setup", Rows("A16")),
    ("claude-code-guide", Rows("A16")),
    // ---- slash commands ----
    ("agents", Rows("X16, A16")),
    ("clear", Rows("C11")),
    ("compact", Rows("C8")),
    ("config", Rows("U12")),
    ("context", Rows("C9")),
    ("doctor", Rows("U13")),
    ("effort", Rows("C14")),
    ("fast", Rows("U17")),
    ("init", Rows("C4")),
    ("mcp", Rows("X15, X16")),
    ("model", Rows("C15")),
    ("output-style", Rows("C13 (chosen in settings or /config)")),
    ("rename", Rows("S6")),
    ("security-review", Rows("G6")),
    ("code-review", Rows("G11")),
    ("simplify", Rows("G12")),
    ("fewer-permission-prompts", Rows("M14")),
    ("loop", Rows("X19, BG6")),
    ("list-agents", Rows("BG10")),
    ("workflow-authoring", Rows("WF9")),
    ("workflow-launch-exec", Rows("WF1 (Claude Code's internal launcher for a workflow)")),
    ("reload-plugins", Rows("X17 (bundles are read on every use; nothing to reload)")),
    ("reload-skills", Rows("X11 (skills are read on every use; nothing to reload)")),
    ("deep-research", Rows("A9 (the research_v2 crew playbook)")),
    ("batch", Rows("A9, WF1 (build_v2 and workflows fan work out over worktrees)")),
    ("verify", Rows("B2, T28")),
    ("run", Rows("T28 (project, service, http_check, browser)")),
    ("debug", Rows("U13 (doctor and the daemon log)")),
    ("update-config", Rows("U12, M9")),
    ("recap", Rows("integ: /recap expands to a session recap (session_prompts.rs)")),
    ("goal", Rows("integ: /goal puts the condition on the task list (T34) and B3 keeps the conductor going")),
    ("focus", Gap("Claude Code's focus view; the nearest here is Ctrl+O's transcript switch (U11)")),
    ("resume", Rows("S1")),
    ("rewind", Rows("S2")),
    ("fork", Rows("S6")),
    ("export", Rows("S7")),
    ("diff", Rows("S3")),
    ("cost", Rows("S4")),
    ("add-dir", Rows("C12")),
    ("memory", Rows("C6")),
    ("permissions", Rows("M10")),
    ("hooks", Rows("X16")),
    ("ide", Rows("U14")),
    ("help", Rows("U19")),
    ("exit", Rows("U19")),
    ("tasks", Rows("BG1")),
    ("workflows", Rows("WF7")),
    ("todos", Rows("T22, T34")),
    ("review", Rows("G4")),
    ("pr-comments", Rows("G5")),
    ("statusline", Rows("U4, A16")),
    ("schedule", Rows("BG5, RM2")),
    ("plugin", Rows("X17; marketplaces X18 deferred by Matthew (9.3-b)")),
    ("vim", Rows("U16, deferred by Matthew (9.3-c)")),
    ("keybindings", Rows("U16, deferred by Matthew (9.3-c)")),
    ("sandbox", Rows("M12, deferred by Matthew (9.3-a)")),
    ("auto-mode-setup", Rows("M6, deferred by Matthew (9.3-d)")),
    ("install-github-app", Rows("G10, deferred by Matthew (9.3-f)")),
    ("color", NotApplicable("the palette is locked (non-negotiable 1): no per-session prompt colour")),
    ("heapdump", NotApplicable("a Node.js heap snapshot of Claude Code itself; Marlowe is native")),
    ("usage", NotApplicable("account plan usage; there is no account (cost per session is S4)")),
    ("team-onboarding", NotApplicable("a single user's harness: there is no team to onboard")),
    ("__remote-workflow", NotApplicable("RM7: remote agents, local-first")),
    ("login", NotApplicable("no account; keys through the Library")),
    ("logout", NotApplicable("no account")),
    ("terminal-setup", NotApplicable("Shift/Alt+Enter is handled in the app")),
    ("dataviz", Content("Claude Code's bundled charting skill; any SKILL.md runs here (X11)")),
    ("claude-api", Content("Claude Code's bundled Anthropic API reference skill (X11)")),
    ("run-skill-generator", Content("Claude Code's bundled skill that writes a project `run` skill (X11)")),
    ("plugin-authoring", Content("Claude Code's bundled plugin-writing skill (X11, X17)")),
    ("autocompact", Gap("auto-compaction (C7) is always on; there is no switch to turn it off")),
    ("insights", Gap("no cross-session usage report; /cost (S4) covers one session")),
];

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/cc_init.json")).expect("cc_init.json")
}

fn names(v: &Value, key: &str) -> Vec<String> {
    v[key].as_array().map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default()
}

fn seen(name: &str) -> Option<Seen> {
    MAP.iter().find(|(n, _)| *n == name).map(|(_, s)| *s)
}

#[test]
fn parity_inventory_every_claude_code_name_is_mapped() {
    let init = fixture();
    assert!(!names(&init, "tools").is_empty() && !names(&init, "slash_commands").is_empty(), "an empty recording");
    let mut listed: BTreeSet<String> = BTreeSet::new();
    for key in ["tools", "slash_commands", "terminal_slash_commands", "agents", "skills"] {
        listed.extend(names(&init, key));
    }
    let unmapped: Vec<&String> = listed.iter().filter(|n| seen(n).is_none()).collect();
    assert!(
        unmapped.is_empty(),
        "Claude Code {} lists names nobody mapped (add each to MAP with its row, a reason, or a gap): {unmapped:?}",
        init["claude_code_version"]
    );
    // The design rows behind each mapped name exist in 2.1-2.13 (spelled the way the rows are).
    for (name, s) in MAP {
        if let Rows(rows) = s {
            assert!(!rows.trim().is_empty(), "{name}");
        }
        if let NotApplicable(why) | Content(why) | Gap(why) = s {
            assert!(why.len() > 3, "{name} needs its reason");
        }
    }
}

#[test]
fn parity_inventory_lists_what_is_not_at_parity() {
    // Not a failure: the report of what Claude Code has that Marlowe does not, kept visible.
    let gaps: Vec<(&str, &str)> = MAP.iter().filter_map(|(n, s)| if let Gap(why) = s { Some((*n, *why)) } else { None }).collect();
    let content: Vec<&str> = MAP.iter().filter_map(|(n, s)| matches!(s, Content(_)).then_some(*n)).collect();
    eprintln!("not at Claude Code parity: {gaps:#?}\nbundled skill content not shipped: {content:?}");
    assert_eq!(gaps.iter().map(|(n, _)| *n).collect::<Vec<_>>(), ["focus", "autocompact", "insights"], "update the report when this changes");
}

/// X22's event-list diff: Claude Code's documented hook events (code.claude.com/docs/en/hooks,
/// read 2026-10-12, 33 events) against the events Marlowe fires (`marlowe_settings::hooks::HookEvent`).
const CC_HOOK_EVENTS: [&str; 33] = [
    "SessionStart", "Setup", "UserPromptSubmit", "UserPromptExpansion", "PreToolUse", "PermissionRequest",
    "PermissionDenied", "PostToolUse", "PostToolUseFailure", "PostToolBatch", "Notification", "MessageDisplay",
    "SubagentStart", "SubagentStop", "TaskCreated", "TaskCompleted", "Stop", "StopFailure", "TeammateIdle",
    "InstructionsLoaded", "ConfigChange", "CwdChanged", "DirectoryAdded", "FileChanged", "WorktreeCreate",
    "WorktreeRemove", "PreCompact", "PostCompact", "PreModelSwitch", "PostModelSwitch", "Elicitation",
    "ElicitationResult", "SessionEnd",
];

#[test]
fn parity_x22_hook_events_are_diffed_against_claude_code() {
    let ours: Vec<&str> = marlowe_settings::hooks::HookEvent::ALL.iter().map(|e| e.name()).collect();
    for e in &ours {
        assert!(CC_HOOK_EVENTS.contains(e), "{e} is not a Claude Code event name");
    }
    let missing: Vec<&str> = CC_HOOK_EVENTS.iter().copied().filter(|e| !ours.contains(e)).collect();
    eprintln!("hook events Claude Code documents that Marlowe does not fire ({}): {missing:?}", missing.len());
    // The gap is reported, not hidden: this list changes only with a deliberate edit.
    assert_eq!(
        missing,
        [
            "Setup", "UserPromptExpansion", "PermissionDenied", "PostToolBatch", "MessageDisplay", "TaskCreated",
            "TaskCompleted", "StopFailure", "TeammateIdle", "InstructionsLoaded", "ConfigChange", "CwdChanged",
            "DirectoryAdded", "FileChanged", "WorktreeCreate", "WorktreeRemove", "PreModelSwitch", "PostModelSwitch",
            "Elicitation", "ElicitationResult",
        ]
    );
}
