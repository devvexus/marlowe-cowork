//! The permission decision for one call (design 2.3 M1-M5, 3.5): modes, rules, tripwires, hooks.
//!
//! In order, the first that applies decides:
//!
//! | # | What | Verdict |
//! |---|---|---|
//! | 1 | hard deny (force push, `--no-verify`, protected paths) | Deny, every mode |
//! | 2 | a deny rule; a PreToolUse hook's `deny` | Deny |
//! | 3 | plan mode and the call writes or runs (a read-only shell line excepted) | Deny |
//! | 4 | tripwire: the shell analysis asks, an unapproved host, a taint side effect, a fixed approval from the tool | Ask in every mode, `bypass` included |
//! | 5 | an ask rule; a hook's `ask` | Ask |
//! | 6 | an allow rule; a hook's `allow` | Free |
//! | 7 | the mode: `bypass` Free; `accept_edits` frees file operations inside the workspace, read-only lines and project commands; `default`/`plan` keep the tool's own answer | |
//!
//! `dont_ask` turns every Ask into a Deny whose reason goes back to the model, and the run goes
//! on. Pure: the caller gathers the facts (`marlowe-agent`'s approval module).

use super::Matched;
use super::Action;
use crate::shell::Analysis;

/// The permission mode (mirrors `marlowe_proto::PermissionMode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Mode {
    Plan,
    #[default]
    Default,
    AcceptEdits,
    Bypass,
    DontAsk,
}

/// A PreToolUse hook's `permissionDecision`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookPermission {
    Allow(Option<String>),
    Deny(String),
    Ask(Option<String>),
}

/// What the tool itself says when asked (its `Consequence`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolSays {
    Free,
    Ask(String),
}

/// Everything the decision needs about one call.
#[derive(Debug, Clone)]
pub struct Facts<'a> {
    pub mode: Mode,
    /// The call changes files.
    pub writes: bool,
    /// The call runs code or commands (shells, python, project commands).
    pub runs: bool,
    /// An MCP tool: its own Ask is about the server, so modes may lift it.
    pub mcp: bool,
    /// A shell call's analysis.
    pub shell: Option<&'a Analysis>,
    /// The tool's own consequence.
    pub tool: ToolSays,
    /// Untrusted content entered the session.
    pub tainted: bool,
    /// Network hosts the call reaches that are neither allowed by settings nor approved this
    /// session.
    pub new_hosts: Vec<String>,
    /// Another tripwire the caller found (python code that pushes, PowerShell's own parser
    /// disagreeing).
    pub extra_ask: Option<String>,
    /// The deciding rule, if any (`RuleSet::evaluate`).
    pub rule: Option<Matched>,
    /// Every command of the line is one of the project's commands (`accept_edits` frees them).
    pub project_command: bool,
    /// A PreToolUse hook's decision.
    pub hook: Option<HookPermission>,
}

impl<'a> Facts<'a> {
    pub fn new(mode: Mode, tool: ToolSays) -> Self {
        Facts {
            mode,
            writes: false,
            runs: false,
            mcp: false,
            shell: None,
            tool,
            tainted: false,
            new_hosts: Vec::new(),
            extra_ask: None,
            rule: None,
            project_command: false,
            hook: None,
        }
    }
}

/// The decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Free,
    /// Ask the user. `tripwire` asks are never widened into a rule, and no mode lifts them.
    Ask { reason: String, tripwire: bool },
    /// Refused; the reason goes back to the model.
    Deny(String),
}

impl Verdict {
    fn ask(reason: impl Into<String>, tripwire: bool) -> Verdict {
        Verdict::Ask { reason: reason.into(), tripwire }
    }
}

/// Decides one call (see the module table).
pub fn decide(f: &Facts<'_>) -> Verdict {
    let v = decide_inner(f);
    match (f.mode, v) {
        (Mode::DontAsk, Verdict::Ask { reason, .. }) => {
            Verdict::Deny(format!("not allowed without asking, and the session is in dont_ask mode: {reason}"))
        }
        (_, v) => v,
    }
}

fn decide_inner(f: &Facts<'_>) -> Verdict {
    // 1. Hard denies.
    if let Some(why) = f.shell.and_then(|a| a.deny.as_deref()) {
        return Verdict::Deny(format!("refused in every mode: {why}"));
    }
    // 2. Deny rules and hooks.
    if let Some(m) = f.rule.as_ref().filter(|m| m.action == Action::Deny) {
        return Verdict::Deny(format!("denied by the rule `{}`", m.rule));
    }
    if let Some(HookPermission::Deny(why)) = &f.hook {
        return Verdict::Deny(why.clone());
    }
    // 3. Plan mode: nothing that writes or runs, except a read-only shell line.
    if f.mode == Mode::Plan && (f.writes || f.runs) {
        let read_only_shell = f.shell.is_some_and(Analysis::read_only);
        if !read_only_shell {
            return Verdict::Deny(
                "plan mode is read-only: present the plan with exit_plan and wait for approval before changing anything"
                    .into(),
            );
        }
    }
    // 4. Tripwires.
    if let Some(why) = tripwire(f) {
        return Verdict::ask(why, true);
    }
    // 5. Ask rules and hooks.
    if let Some(m) = f.rule.as_ref().filter(|m| m.action == Action::Ask) {
        return Verdict::ask(format!("asked by the rule `{}`", m.rule), false);
    }
    if let Some(HookPermission::Ask(why)) = &f.hook {
        return Verdict::ask(why.clone().unwrap_or_else(|| "a hook asked for confirmation".into()), false);
    }
    // 6. Allow rules and hooks.
    if f.rule.as_ref().is_some_and(|m| m.action == Action::Allow) || matches!(f.hook, Some(HookPermission::Allow(_))) {
        return Verdict::Free;
    }
    // 7. The mode.
    let own = match &f.tool {
        ToolSays::Free => Verdict::Free,
        ToolSays::Ask(why) => Verdict::ask(why.clone(), false),
    };
    match f.mode {
        Mode::Bypass => Verdict::Free,
        Mode::AcceptEdits => match f.shell {
            Some(a) if a.fs_inside() || f.project_command => Verdict::Free,
            _ => own,
        },
        Mode::Plan | Mode::Default | Mode::DontAsk => own,
    }
}

/// A fixed approval or tripwire: asks in every mode.
fn tripwire(f: &Facts<'_>) -> Option<String> {
    if let Some(a) = f.shell {
        if let Some(why) = a.asks.first() {
            return Some(why.clone());
        }
    }
    if let Some(h) = f.new_hosts.first() {
        return Some(format!("first contact with {h}"));
    }
    if let Some(why) = &f.extra_ask {
        return Some(why.clone());
    }
    if f.tainted && f.runs {
        return Some("runs commands after untrusted content entered the session".into());
    }
    // A tool that neither runs code nor is an MCP tool asks only for fixed reasons (a write
    // outside the workspace, a new host, untrusted content): no mode lifts those.
    match &f.tool {
        ToolSays::Ask(why) if !f.runs && !f.mcp => Some(why.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
