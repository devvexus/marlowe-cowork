//! The search group (track B): glob, grep. B moves them onto `ignore` + `grep-searcher`
//! (`marlowe-exec/src/search/`) with rg semantics; the defs stay here.

use marlowe_agent::{Effects, ToolGroup};
use marlowe_model::ToolSpec;
use serde_json::json;

use super::files::untrusted_read;
use super::{describe_by, spec, ToolDef};
use crate::fs_tools::Fs;

pub fn tools() -> Vec<ToolDef> {
    vec![
        ToolDef::new(glob_spec(), ToolGroup::Search, Effects::READ, |t, a, c| {
            Fs { tools: t, root: t.root_for_ref(c) }.glob(a)
        })
        .path_args(&["path"])
        .describe(|c| describe_by(c, "pattern"))
        .taints(untrusted_read),
        ToolDef::new(grep_spec(), ToolGroup::Search, Effects::READ, |t, a, c| {
            Fs { tools: t, root: t.root_for_ref(c) }.grep(a)
        })
        .path_args(&["path"])
        .describe(|c| describe_by(c, "pattern"))
        .taints(untrusted_read),
    ]
}

fn glob_spec() -> ToolSpec {
    spec(
        "glob",
        "List files under a directory, optionally filtered. `pattern` supports `*` and `?` \
         (within one path segment), `**` (any depth, so `**/Cargo.toml` finds every one, the \
         top-level file included) and `{a,b}`; a pattern without `/` matches file names \
         anywhere beneath, one with `/` matches the path relative to `path`. Build output and \
         VCS directories are skipped.",
        json!({
            "path": {"type": "string", "description": "Directory to list; default is the workspace."},
            "pattern": {"type": "string", "description": "e.g. \"*.rs\", \"src/*.rs\" or \"**/Cargo.toml\"."}
        }),
        &[],
    )
}

fn grep_spec() -> ToolSpec {
    spec(
        "grep",
        "Search file contents with a regular expression (Rust regex syntax; start with `(?i)` \
         for case-insensitive). Returns `path:line:text` for matches and `path-line-text` for \
         context lines.",
        json!({
            "pattern": {"type": "string"},
            "path": {"type": "string", "description": "Directory to search; default is the workspace."},
            "glob": {"type": "string", "description": "Only search files matching this, e.g. \"*.rs\"."},
            "context": {"type": "integer", "description": "Lines of context around each match (0-20)."}
        }),
        &["pattern"],
    )
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
