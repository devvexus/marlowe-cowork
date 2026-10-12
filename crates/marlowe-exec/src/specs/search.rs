//! The search group (track B): glob, grep, on `ignore` + `grep-searcher` with rg semantics
//! (`marlowe-exec/src/search/`).

use marlowe_agent::{Effects, ToolGroup};
use marlowe_model::ToolSpec;
use serde_json::json;

use super::files::untrusted_read;
use super::{describe_by, spec, ToolDef};
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
    spec(
        "glob",
        "List files by gitignore-style pattern, newest first (`*.rs`, `src/*.rs`, `**/x`, `!x` excludes). Hidden files are listed; ignored files are not.",
        json!({
            "pattern": {"type": "string"},
            "path": {"type": "string"},
            "head_limit": {"type": "integer"},
            "offset": {"type": "integer"}
        }),
        &[],
    )
}

fn grep_spec() -> ToolSpec {
    spec(
        "grep",
        "Search contents with ripgrep (Rust regex). `output_mode`: files_with_matches (default), content (`path:line:text`), count. Hidden files are searched, gitignored ones are not. `head_limit` default 250, 0 for all.",
        json!({
            "pattern": {"type": "string"},
            "path": {"type": "string"},
            "glob": {"type": "string"},
            "type": {"type": "string"},
            "output_mode": {"type": "string", "enum": ["files_with_matches", "content", "count"]},
            "-A": {"type": "integer"},
            "-B": {"type": "integer"},
            "-C": {"type": "integer"},
            "-i": {"type": "boolean"},
            "multiline": {"type": "boolean"},
            "head_limit": {"type": "integer"},
            "offset": {"type": "integer"}
        }),
        &["pattern"],
    )
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
