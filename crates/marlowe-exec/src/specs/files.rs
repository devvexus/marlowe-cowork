//! The files group (track A): read, write, edit. A adds multi_edit, edit_lines, notebook_edit
//! and moves the bodies to `marlowe-code` (`edit/`, `filestate/`, `notebook/`, `editlines/`) going
//! through `BuiltinTools::workspace_port`.

use marlowe_agent::{Consequence, Effects, Exposure, Mutation, SandboxView, ToolCallReq, ToolCtx, ToolGroup};
use marlowe_model::ToolSpec;
use serde_json::json;

use super::{clip, describe_by, spec, ToolDef};
use crate::{fs_tools::Fs, paths, str_arg, BuiltinTools};

pub fn tools() -> Vec<ToolDef> {
    vec![
        ToolDef::new(read_spec(), ToolGroup::Files, Effects::READ, |t, a, c| fs(t, c).read(a))
            .path_args(&["path"])
            .describe(|c| describe_by(c, "path"))
            .taints(untrusted_read),
        ToolDef::new(write_spec(), ToolGroup::Files, Effects::WRITE, |t, a, c| fs(t, c).write(a))
            .path_args(&["path"])
            .consequence(write_consequence)
            .crew_consequence(crew_write_consequence)
            .describe(|c| describe_by(c, "path"))
            .mutation(written_path),
        ToolDef::new(edit_spec(), ToolGroup::Files, Effects::WRITE, |t, a, c| fs(t, c).edit(a))
            .path_args(&["path"])
            .consequence(write_consequence)
            .crew_consequence(crew_write_consequence)
            .describe(|c| describe_by(c, "path"))
            .mutation(written_path),
    ]
}

fn fs<'a>(tools: &'a BuiltinTools, ctx: &'a ToolCtx) -> Fs<'a> {
    Fs { tools, root: tools.root_for_ref(ctx) }
}

/// Reading outside the user's own folders brings untrusted content in (read, glob, grep).
pub(crate) fn untrusted_read(tools: &BuiltinTools, call: &ToolCallReq) -> bool {
    !tools.trusted_read(str_arg(&call.args, "path").unwrap_or("."))
}

/// Writes inside a root that frees writes are free; anywhere else asks.
fn write_consequence(tools: &BuiltinTools, call: &ToolCallReq, _exposure: &Exposure) -> Consequence {
    match str_arg(&call.args, "path") {
        Some(p) if tools.inside_root(p) => Consequence::Free,
        Some(p) => Consequence::Ask {
            summary: format!("{} {} (outside the workspace)", call.name, crate::first_line(p, 100)),
        },
        None => Consequence::Free, // refused by `call` for the missing argument
    }
}

/// In a crew sandbox: free inside it, ask outside.
fn crew_write_consequence(
    _tools: &BuiltinTools,
    call: &ToolCallReq,
    _exposure: &Exposure,
    sandbox: &dyn SandboxView,
) -> Consequence {
    match str_arg(&call.args, "path") {
        Some(p) if sandbox.allows_write(std::path::Path::new(p)) => Consequence::Free,
        Some(p) => Consequence::Ask { summary: format!("{} {} (outside the sandbox)", call.name, clip(p, 100)) },
        None => Consequence::Free, // refused by the inner host for the missing argument
    }
}

fn written_path(tools: &BuiltinTools, call: &ToolCallReq, ctx: &ToolCtx) -> Mutation {
    match str_arg(&call.args, "path").filter(|p| !p.trim().is_empty()) {
        Some(p) => Mutation::Paths(vec![paths::resolve(tools.root_for_ref(ctx), p)]),
        None => Mutation::None,
    }
}

fn read_spec() -> ToolSpec {
    spec(
        "read",
        "Read a text file. Returns numbered lines (`cat -n` style: right-aligned number, tab, \
         text). Long files come back in windows; the result says how to continue with `range`. \
         Do not copy the number prefix into `edit`.",
        json!({
            "path": {"type": "string", "description": "File path, relative to the workspace or absolute."},
            "range": {"type": "string", "description": "Optional 1-based inclusive line range, e.g. \"20-60\"."}
        }),
        &["path"],
    )
}

fn write_spec() -> ToolSpec {
    spec(
        "write",
        "Create a file or overwrite it entirely with `content`. Parent directories are created.",
        json!({
            "path": {"type": "string"},
            "content": {"type": "string", "description": "The complete new file content."}
        }),
        &["path", "content"],
    )
}

fn edit_spec() -> ToolSpec {
    spec(
        "edit",
        "Replace one exact snippet in an existing file. `replacing` must match byte for byte \
         (including indentation) and appear exactly once; copy it from a `read` without the \
         line-number prefixes.",
        json!({
            "path": {"type": "string"},
            "replacing": {"type": "string", "description": "The exact text to replace."},
            "content": {"type": "string", "description": "The replacement text."}
        }),
        &["path", "replacing", "content"],
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
