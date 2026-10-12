//! The search group (track B): glob, grep, on `ignore` + `grep-searcher` with rg semantics
//! (`marlowe-exec/src/search/`).

use marlowe_agent::{Effects, ToolGroup};
use marlowe_model::ToolSpec;

use super::files::untrusted_read;
use super::{describe_by, ToolDef};
use crate::search::{self, SearchEnv};

pub fn tools() -> Vec<ToolDef> {
    vec![
        ToolDef::new(glob_spec(), ToolGroup::Search, Effects::READ, |t, a, c| {
            search::glob(&SearchEnv { root: t.root_for_ref(c), port: t.workspace_port() }, a)
        })
        .path_args(&["path"])
        .describe(|c| describe_by(c, "pattern"))
        .taints(untrusted_read),
        ToolDef::new(grep_spec(), ToolGroup::Search, Effects::READ, |t, a, c| {
            search::grep(&SearchEnv { root: t.root_for_ref(c), port: t.workspace_port() }, a)
        })
        .path_args(&["path"])
        .describe(|c| describe_by(c, "pattern"))
        .taints(untrusted_read),
    ]
}

fn glob_spec() -> ToolSpec {
    marlowe_agent::compact_specs::spec("glob")
}

fn grep_spec() -> ToolSpec {
    marlowe_agent::compact_specs::spec("grep")
}

// Loop middleware for this group, built once per host next to its state (see `specs/mod.rs`).
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
