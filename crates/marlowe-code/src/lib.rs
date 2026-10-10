//! Code tools behind the `files` and `code` tool groups (M4.6). Seam skeleton: every module is
//! empty and owned by the track named on it; the tool definitions that call into this crate live
//! in `marlowe-exec/src/specs/{files,code}.rs`.
//!
//! | Module | Track | What goes here |
//! |---|---|---|
//! | [`edit`] | A | `edit` / `multi_edit` / `write`: exact -> EOL-normalised -> whitespace tiers (unique only), did-you-mean, atomic temp+rename keeping BOM/EOL, cross-file all-or-nothing |
//! | [`filestate`] | A | `FileState{blake3, mtime, size}` per agent; read-before-edit and stale checks as a `ToolMiddleware`; the watcher that turns external changes into a next-step reminder |
//! | [`notebook`] | A | `.ipynb` read (cells, outputs, image outputs as `ResultPart::Image`) and `notebook_edit` |
//! | [`editlines`] | A | `edit_lines{path, anchor:"line:hash", op, text}` (bench arm) |
//! | [`index`] | C | tree-sitter tags into `<home>\index\<repo-hash>.db`, keyed (path, blake3), incremental |
//! | [`repomap`] | C | PageRank over def/ref, budgeted (1.2k tokens at 32k, 4k frontier) |
//! | [`lsp`] | C | `lsp-server` + `lsp-types` client: reader thread per server, Job Object via `marlowe_os::child`, idle stop, max 2 live; diagnostics settle |
//! | [`udiff`] | C | unified-diff parse for `rename_symbol` / WorkspaceEdit only |
//!
//! File access goes through `marlowe_os::workspace::Workspace` (so `DockerExec` gets the same
//! tools); every child through `marlowe_os::child`. tree-sitter (a C compile) stays under
//! `[target.'cfg(not(target_arch = "aarch64"))'.dependencies]` until approval 9.1-i.

#![forbid(unsafe_code)]

pub mod edit;
pub mod editlines;
pub mod filestate;
pub mod index;
pub mod lsp;
pub mod notebook;
pub mod repomap;
pub mod udiff;
