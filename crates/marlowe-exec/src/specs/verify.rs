//! The verify group (track D): project, service, http_check and browser, backed by `marlowe-verify`.
//! Services start through `marlowe_os::child` in their own Job Object with a per-launch
//! `env_allow`; `http_check` goes through `marlowe-net` (localhost exempt); `browser` reuses
//! `crate::render::EdgeRenderer`.
//!
//! Empty until D lands: add a [`ToolDef`] per tool to `tools()`, in model-visible order, with
//! `ToolGroup::Verify`. Nothing outside this file changes.

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

/// Whether a local model gets this group in `root`. D: a detected project profile.
pub fn detect(_root: Option<&std::path::Path>) -> bool {
    false
}
