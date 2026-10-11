//! The workflow group (track W): `workflow` (inline script, saved name or `script_path`; `resume_from`), `structured_output` (workflow agents) and `report_findings`.
//!
//! Empty until W lands: add a [`ToolDef`] per tool to `tools()`, in model-visible order, with
//! `ToolGroup::Workflow`. Nothing outside this file changes. The background registry the tools
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

/// Whether a local model gets this group in `root`. W: any workspace; the tool is deferred behind `tool_search` on a 32k context (T31).
pub fn detect(_root: Option<&std::path::Path>) -> bool {
    false
}
