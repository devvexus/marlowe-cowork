//! The tasks group (track N): `task_list`, `task_output`, `task_stop`, `monitor`, `cron_create`, `cron_list`, `cron_delete`, `schedule_wakeup`, `list_agents`.
//!
//! Empty until N lands: add a [`ToolDef`] per tool to `tools()`, in model-visible order, with
//! `ToolGroup::Tasks`. Nothing outside this file changes. The background registry the tools
//! reach is the [`marlowe_agent::Tasks`] service (design 3.12); the daemon installs it.

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

/// Whether a local model gets this group in `root`. N: every workspace (background work is part of coding).
pub fn detect(_root: Option<&std::path::Path>) -> bool {
    false
}
