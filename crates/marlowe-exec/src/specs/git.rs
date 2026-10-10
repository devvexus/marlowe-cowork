//! The git group (track J): git{op}, gh{op} and ci_status (`marlowe-exec/src/git/`). Reads are free,
//! writes ask in default mode; force variants are hard-denied by E's rules. The `gh` child gets
//! `GH_TOKEN` only through `ChildSpec::env_allow`. Push and PR creation go through the daemon
//! (`Request::OpenPr`), never from a crew shell.
//!
//! Empty until J lands: add a [`ToolDef`] per tool to `tools()`, in model-visible order, with
//! `ToolGroup::Git`. Nothing outside this file changes.

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

/// Whether a local model gets this group in `root`. J: a git repository.
pub fn detect(_root: Option<&std::path::Path>) -> bool {
    false
}
