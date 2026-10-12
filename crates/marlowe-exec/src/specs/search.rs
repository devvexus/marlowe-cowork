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
        "List files by name pattern, newest first. `pattern` is a gitignore-style glob: `*.rs` matches that \
        name at any depth, `src/*.rs` is relative to `path`, `**/Cargo.toml` matches at every depth (the top \
        level too), `*.{ts,tsx}` alternates, `!x` excludes. Hidden files are listed; files ignored by \
        .gitignore and VCS directories are not. Omit `pattern` to list every file.",
        json!({
            "pattern": {"type": "string", "description": "e.g. \"*.rs\", \"src/**/*.ts\" or \"**/Cargo.toml\"."},
            "path": {"type": "string", "description": "Directory to search; default is the workspace."},
            "head_limit": {"type": "integer", "description": "Most paths to list; default 250, 0 for all."},
            "offset": {"type": "integer", "description": "Skip this many paths first (to page)."}
        }),
        &[],
    )
}

fn grep_spec() -> ToolSpec {
    spec(
        "grep",
        "Search file contents with ripgrep (Rust regex syntax). `output_mode` picks the result: \
        files_with_matches (default) lists the paths, newest first; content prints `path:line:text` for \
        matches and `path-line-text` for context lines, `--` between groups; count prints `path:N`. Hidden \
        files are searched; files ignored by .gitignore and VCS directories are not (name an ignored \
        directory as `path` to search it). `path` may be a directory or one file.",
        json!({
            "pattern": {"type": "string", "description": "Regular expression; backslash-escape metacharacters to match them literally."},
            "path": {"type": "string", "description": "Directory or file to search; default is the workspace."},
            "glob": {"type": "string", "description": "Only search files matching this, e.g. \"*.rs\" or \"*.{ts,tsx}\"."},
            "type": {"type": "string", "description": "Only search this ripgrep file type: rust, py, js, ts, go, c, cpp, java, md, json..."},
            "output_mode": {"type": "string", "enum": ["files_with_matches", "content", "count"]},
            "-A": {"type": "integer", "description": "Lines of context after each match (content mode)."},
            "-B": {"type": "integer", "description": "Lines of context before each match (content mode)."},
            "-C": {"type": "integer", "description": "Lines of context around each match (content mode)."},
            "-i": {"type": "boolean", "description": "Ignore case."},
            "-n": {"type": "boolean", "description": "Show line numbers in content mode; default true."},
            "multiline": {"type": "boolean", "description": "Let the pattern span lines (`.` matches newlines)."},
            "head_limit": {"type": "integer", "description": "Most result lines (paths, rows or counts); default 250, 0 for all."},
            "offset": {"type": "integer", "description": "Skip this many result lines first (to page)."}
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
