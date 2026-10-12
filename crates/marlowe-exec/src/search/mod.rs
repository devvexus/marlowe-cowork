//! `glob` and `grep` with ripgrep semantics (design 2.1 T10, T11; A9). Track B.
//!
//! One walker, [`walk::collect`], backs both tools: the `ignore` crate configured as
//! `rg --hidden --glob '!.git'` (hidden files are searched, VCS directories are not; `.gitignore`,
//! `.ignore`, `.rgignore`, `.git/info/exclude` and the global excludes file apply inside a git
//! repository; links are not followed). Naming an ignored directory as `path` searches it, as
//! `rg pattern dir` does. Matching is `grep-searcher` + `grep-regex`, the engine ripgrep itself is
//! built on, so line splitting, context groups, BOM decoding, binary detection and multiline
//! matching are rg's. The deliberate differences are all pinned by `tests::rg_fixture`:
//!
//! - CRLF files: `$` matches before `\r\n` (rg `--crlf`; most Windows repos need it).
//! - Lines over 500 characters are cut and say how much was dropped (rg `--max-columns` omits them).
//! - `files_with_matches` is newest-first (Claude Code's order); `content` and `count` are in path
//!   order (rg `--sort path`), so output is deterministic.
//! - `glob` (grep's file filter) is relative to `path`, where `rg -g` is relative to the working
//!   directory; a glob that repeats `path` as its prefix has the prefix removed.
//! - Directories named `target`, `node_modules`, `.venv`, `__pycache__`, `dist` are skipped even
//!   when nothing ignores them, and the result says so (name one as `path` to look inside).
//!
//! Local workspaces go through the walker and `search_path`; any other [`Workspace`] port (K's
//! container) goes through its `walk` and `read`, without ignore files.

mod glob;
mod grep;
mod walk;

#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};

use marlowe_agent::ToolOutcome;
use marlowe_os::workspace::Workspace;
use serde_json::Value;

pub use glob::glob;
pub use grep::grep;

/// Bytes of one tool result before the output is cut (the model's `limit` comes first).
pub(crate) const RESULT_BYTES: usize = 60_000;

/// What a search call runs against.
pub struct SearchEnv<'a> {
    /// The workspace root: relative paths resolve against it and results are shown relative to it.
    pub root: Option<&'a Path>,
    /// The machine the files are on.
    pub port: &'a dyn Workspace,
}

impl SearchEnv<'_> {
    /// Whether the files are on this machine (the `ignore` walker applies).
    pub(crate) fn is_local(&self) -> bool {
        self.port.kind() == "local"
    }

    pub(crate) fn resolve(&self, raw: &str) -> PathBuf {
        crate::paths::resolve(self.root, raw)
    }

    pub(crate) fn shown(&self, abs: &Path) -> String {
        crate::paths::display(self.root, abs)
    }

    /// A directory for a message: `.` for the workspace root itself.
    pub(crate) fn shown_dir(&self, abs: &Path) -> String {
        if self.root.is_some_and(|r| r == abs) {
            ".".to_string()
        } else {
            self.shown(abs)
        }
    }

    /// The search base from `args.path`: the workspace root (or the process cwd) by default.
    pub(crate) fn base(&self, args: &Value) -> Result<PathBuf, ToolOutcome> {
        match args.get("path") {
            None | Some(Value::Null) => Ok(self.resolve(".")),
            Some(Value::String(p)) if !p.trim().is_empty() => Ok(self.resolve(p)),
            Some(Value::String(_)) => Err(ToolOutcome::error(
                "`path` is empty. Omit it to search the workspace, or give a directory or file.",
            )),
            Some(_) => Err(ToolOutcome::error("`path` must be a string")),
        }
    }
}

/// A bool read leniently, the way small models send them: `true` or `"true"`.
pub(crate) fn lenient_bool(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "yes" | "1" => Some(true),
            "false" | "no" | "0" => Some(false),
            _ => None,
        },
        Value::Number(n) => n.as_i64().map(|n| n != 0),
        _ => None,
    }
}

/// An integer read leniently: `3` or `"3"`.
pub(crate) fn lenient_int(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// `limit` (the older `head_limit` still works) and `offset`, shared by both tools. A `limit` of 0 means no limit.
pub(crate) struct Page {
    pub limit: usize,
    pub offset: usize,
}

pub(crate) const DEFAULT_HEAD_LIMIT: usize = 250;

impl Page {
    pub fn parse(args: &Value) -> Result<Page, ToolOutcome> {
        let one = |key: &str, default: i64| -> Result<usize, ToolOutcome> {
            match args.get(key) {
                None | Some(Value::Null) => Ok(default as usize),
                Some(v) => match lenient_int(v) {
                    Some(n) if n >= 0 => Ok(n as usize),
                    _ => Err(ToolOutcome::error(format!("`{key}` must be a whole number, 0 or more"))),
                },
            }
        };
        let key = if args.get("limit").is_some_and(|v| !v.is_null()) { "limit" } else { "head_limit" };
        let limit = one(key, DEFAULT_HEAD_LIMIT as i64)?;
        let offset = one("offset", 0)?;
        Ok(Page { limit: if limit == 0 { usize::MAX } else { limit }, offset })
    }

    /// The slice of `all` this page shows.
    pub fn slice<'a, T>(&self, all: &'a [T]) -> &'a [T] {
        let start = self.offset.min(all.len());
        let end = start.saturating_add(self.limit).min(all.len());
        &all[start..end]
    }

    /// What follows a cut listing: how to see the rest.
    pub fn more(&self, shown: usize, total: usize, at_least: bool) -> Option<String> {
        let next = self.offset + shown;
        if next >= total && !at_least {
            return None;
        }
        let of = if at_least { format!("at least {total}") } else { total.to_string() };
        Some(format!("[showing {shown} of {of}; pass offset={next} for the next page]"))
    }
}

/// Joins `lines` with newlines until `max` bytes; returns the text and how many lines fit.
pub(crate) fn join_within(lines: &[String], max: usize) -> (String, usize) {
    let mut out = String::new();
    let mut n = 0;
    for l in lines {
        if out.len() + l.len() + 1 > max && n > 0 {
            break;
        }
        if n > 0 {
            out.push('\n');
        }
        out.push_str(l);
        n += 1;
    }
    (out, n)
}
