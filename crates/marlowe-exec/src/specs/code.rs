//! The code group (track C): symbols, definition, references, hover, diagnostics and rename_symbol, backed by
//! `marlowe-code` (`index/`, `repomap/`, `lsp/`, `udiff/`). The LSP manager lives in
//! `BuiltinTools::state`; servers start through `marlowe_os::child`. Post-edit diagnostics are a
//! `ToolMiddleware::after_call`, not a tool. tree-sitter stays behind
//! `cfg(not(target_arch = "aarch64"))` until 9.1-i.
//!
//! Empty until C lands: add a [`ToolDef`] per tool to `tools()`, in model-visible order, with
//! `ToolGroup::Code`. Nothing outside this file changes.

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

/// Whether a local model gets this group in `root`. C: a workspace with an index or a running LSP.
pub fn detect(_root: Option<&std::path::Path>) -> bool {
    false
}
