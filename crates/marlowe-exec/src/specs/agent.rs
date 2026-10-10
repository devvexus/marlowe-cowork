//! The agent group (track F): todo, ask_user, skill, agent, send_message, exit_plan, enter_worktree and
//! exit_worktree. Loop-side behaviour (todo reminders, the verify-before-final check) is
//! `ToolMiddleware`; worktrees come from `marlowe_os::workspace::WorktreeProvider`; per-session
//! state lives in `Conversation::ext`.
//!
//! Empty until F lands: add a [`ToolDef`] per tool to `tools()`, in model-visible order, with
//! `ToolGroup::Agent`. Nothing outside this file changes.

use super::ToolDef;

pub fn tools() -> Vec<ToolDef> {
    Vec::new()
}

/// Loop middleware for this group, built once per host next to its state (see `specs/mod.rs`).
pub fn middleware(tools: &crate::BuiltinTools) -> Vec<std::sync::Arc<dyn marlowe_agent::ToolMiddleware>> {
    super::no_middleware(tools)
}

/// This group's `persona/blocks/<group>.md` (`include_str!`) once it exists; appended only while
/// the group is exposed and has a live tool.
pub fn persona_block() -> Option<&'static str> {
    None
}

/// Per-host setup from the profile home (state via `BuiltinTools::with_state`).
pub fn configure(tools: crate::BuiltinTools, setup: &super::HostSetup<'_>) -> crate::BuiltinTools {
    super::no_configure(tools, setup)
}

/// Whether a local model gets this group in `root`. Always exposed to local models (a core group).
pub fn detect(_root: Option<&std::path::Path>) -> bool {
    false
}
