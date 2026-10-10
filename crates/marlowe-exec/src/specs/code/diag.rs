//! Diagnostics after edits (design 2.1 T24): a [`ToolMiddleware`] that, around any call that
//! declares the files it will change (`Mutation::Paths`: write, edit, multi_edit, edit_lines,
//! notebook_edit), opens those files on their language server before the call
//! (`pre_mutation`) and, after it succeeded, sends the new text, waits for the server to settle
//! (800 ms quiet, 5 s at most) and appends the errors that were not there before to the result.
//! Edited files are re-tagged in the index either way. Shell commands (`Mutation::Under`) are
//! not followed: what they changed is not known.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use marlowe_agent::{CallCtx, Mutation, ToolMiddleware, ToolOutcome};
use marlowe_code::index::{norm_key, Lang};
use marlowe_code::lsp::intel::{CodeIntel, EditSnapshot, FileDiags};
use marlowe_model::ToolCall;
use marlowe_os::workspace::LocalFs;

use super::CodeHost;

/// Most new errors listed per edit.
const MAX_LISTED: usize = 20;
/// A snapshot whose call never finished (refused by a later middleware) is dropped after this.
const STALE: Duration = Duration::from_secs(600);

/// Files an unfinished call will change, and what their servers knew before: (when, files, snapshot).
type Pending = (Instant, Vec<PathBuf>, EditSnapshot);

/// The middleware for one host's workspace.
pub struct DiagnosticsMiddleware {
    intel: Arc<CodeIntel>,
    root: PathBuf,
    /// call id -> pending.
    pending: Mutex<HashMap<String, Pending>>,
}

impl DiagnosticsMiddleware {
    pub fn new(host: CodeHost, root: &Path) -> Self {
        Self { intel: host.intel(root), root: root.to_path_buf(), pending: Mutex::new(HashMap::new()) }
    }

    fn inside(&self, p: &Path) -> bool {
        let (k, r) = (norm_key(p), norm_key(&self.root));
        k.len() > r.len() && k.starts_with(&r) && k.as_bytes()[r.len()] == b'/'
    }
}

impl ToolMiddleware for DiagnosticsMiddleware {
    fn name(&self) -> &str {
        "code-diagnostics"
    }

    fn pre_mutation(&self, call: &ToolCall, _cx: &CallCtx, mutation: &Mutation) -> Result<(), ToolOutcome> {
        let Mutation::Paths(paths) = mutation else { return Ok(()) };
        let files: Vec<PathBuf> = paths.iter().filter(|p| Lang::of(p).is_some() && self.inside(p)).cloned().collect();
        if files.is_empty() {
            return Ok(());
        }
        let snap = self.intel.before_edit(&LocalFs, &files);
        let mut pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
        pending.retain(|_, (at, ..)| at.elapsed() < STALE);
        pending.insert(call.id.clone(), (Instant::now(), files, snap));
        Ok(())
    }

    fn after_call(&self, call: &ToolCall, cx: &CallCtx, outcome: &mut ToolOutcome) {
        let Some((_, files, snap)) = self.pending.lock().unwrap_or_else(|p| p.into_inner()).remove(&call.id) else {
            return;
        };
        if outcome.is_error {
            return;
        }
        let cancel = cx.tool.cancel.clone();
        let found = self.intel.after_edit(&LocalFs, &files, &snap, &move || cancel.is_cancelled());
        let text = render(&found, |p| self.intel.rel(p));
        if !text.is_empty() {
            outcome.content.push_str(&text);
        }
    }
}

/// The new errors as a block appended to a tool result; empty when there are none.
pub fn render(found: &[FileDiags], rel: impl Fn(&Path) -> String) -> String {
    let with_new: Vec<&FileDiags> = found.iter().filter(|d| !d.new_errors.is_empty()).collect();
    if with_new.is_empty() {
        return String::new();
    }
    let mut out = String::from("\n\n<new-diagnostics>\n");
    let mut listed = 0;
    for d in with_new {
        let n = d.new_errors.len();
        let _ = writeln!(
            out,
            "{}: {} new error{} ({} error{}, {} warning{} in the file now)",
            rel(&d.path),
            n,
            if n == 1 { "" } else { "s" },
            d.errors,
            if d.errors == 1 { "" } else { "s" },
            d.warnings,
            if d.warnings == 1 { "" } else { "s" },
        );
        for (e, line) in d.new_errors.iter().zip(&d.lines) {
            if listed == MAX_LISTED {
                break;
            }
            let code = e.code.as_deref().map(|c| format!("[{c}]")).unwrap_or_default();
            let _ = writeln!(out, "  {}:{} error{code}: {}", e.line + 1, e.col + 1, e.message.lines().next().unwrap_or(""));
            if !line.is_empty() {
                let _ = writeln!(out, "      │ {line}");
            }
            listed += 1;
        }
    }
    out.push_str("</new-diagnostics>");
    out
}
