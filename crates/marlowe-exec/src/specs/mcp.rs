//! The mcp group (track K): tool_search (activates when MCP tools exceed the threshold or the prefix is
//! over budget). The `mcp__srv__tool` tools themselves are served by the MCP host the daemon
//! composes, not registered here.
//!
//! Empty until K lands: add a [`ToolDef`] per tool to `tools()`, in model-visible order, with
//! `ToolGroup::Mcp`. Nothing outside this file changes.

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

/// Whether a local model gets this group in `root`. K: configured MCP servers (behind `tool_search` over its threshold).
pub fn detect(_root: Option<&std::path::Path>) -> bool {
    false
}
