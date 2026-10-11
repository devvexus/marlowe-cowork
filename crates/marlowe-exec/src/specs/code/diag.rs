//! Diagnostics after edits (design 2.1 T24): a [`ToolMiddleware`] that, around any call that
//! declares the files it will change (`Mutation::Paths`: write, edit, multi_edit, edit_lines,
//! notebook_edit), opens those files on their language server before the call
//! (`pre_mutation`) and, after it succeeded, sends the new text, waits for the server to settle
//! (800 ms quiet, 5 s at most) and appends the errors that were not there before to the result.
//! Edited files are re-tagged in the index either way. Shell commands (`Mutation::Under`) are
//! not followed: what they changed is not known.
//!
//! Errors that arrive after that window (rust-analyzer's `cargo check`: borrowck, lifetimes,
//! trait bounds) are not lost. Per session and edited file the middleware keeps what the model
//! was last shown; `after_step` diffs any later publish against it and appends a
//! `<new-diagnostics>` reminder as its own message, `before_final` does the same before the model
//! may finish (the turn takes another step), and the next edit of the file is diffed against it
//! too, so a late error is never absorbed into a new baseline. The user's own text is never
//! touched: it is stored in memory at user trust and is the retrieval query.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use marlowe_agent::{CallCtx, Conversation, FinalFlow, Mutation, StepInfo, ToolMiddleware, ToolOutcome};
use marlowe_code::index::{norm_key, Lang};
use marlowe_code::lsp::intel::{CodeIntel, EditSnapshot, FileDiags};
use marlowe_code::lsp::Diag;
use marlowe_model::{Message, ToolCall};
use marlowe_os::workspace::LocalFs;

use super::CodeHost;
use crate::BuiltinTools;

/// Most new errors listed per edit.
const MAX_LISTED: usize = 20;
/// A snapshot whose call never finished (refused by a later middleware) is dropped after this.
const STALE: Duration = Duration::from_secs(600);

/// An unfinished call: when it started, the files it will change, what their servers knew
/// before, and each file's latest publish (server instance, generation) before it.
struct Pending {
    at: Instant,
    files: Vec<PathBuf>,
    snap: EditSnapshot,
    before: HashMap<String, (u64, u64)>,
}

/// What the model was last shown for one edited file: that publish (server instance and
/// generation) and its diagnostics.
#[derive(Debug, Clone)]
struct Shown {
    path: PathBuf,
    instance: u64,
    gen: u64,
    known: Vec<Diag>,
}

/// The middleware for one host's workspace.
pub struct DiagnosticsMiddleware {
    intel: Arc<CodeIntel>,
    root: PathBuf,
    /// Writes in the root are free (it is the user's own workspace): servers may run trusted
    /// there while the session is untainted.
    free: bool,
    /// call id -> pending.
    pending: Mutex<HashMap<String, Pending>>,
    /// (session, file key) -> what the model was last shown.
    shown: Mutex<HashMap<(String, String), Shown>>,
}

impl DiagnosticsMiddleware {
    pub fn new(host: CodeHost, root: &Path) -> Self {
        Self {
            intel: host.intel(root),
            root: root.to_path_buf(),
            free: false,
            pending: Mutex::new(HashMap::new()),
            shown: Mutex::new(HashMap::new()),
        }
    }

    /// The middleware for `tools`' workspace `root`, trusting servers where its writes are free.
    pub fn for_host(tools: &BuiltinTools, root: &Path) -> Self {
        Self { free: super::trusted_servers(tools, root, false), ..Self::new(tools.state::<CodeHost>().as_ref().clone(), root) }
    }

    /// Under the root once `..` is applied and links are resolved.
    fn inside(&self, p: &Path) -> bool {
        marlowe_code::lsp::uri::contained(&self.root, p)
    }

    fn shown(&self) -> std::sync::MutexGuard<'_, HashMap<(String, String), Shown>> {
        self.shown.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Publishes for `session`'s edited files that the model has not been shown, as new errors
    /// against what it was shown; what it is shown now is remembered.
    fn late(&self, session: &str) -> Vec<FileDiags> {
        let mut out = Vec::new();
        let mut shown = self.shown();
        for (_, seen) in shown.iter_mut().filter(|((s, _), _)| s == session) {
            let Some(latest) = self.intel.latest(&seen.path) else { continue };
            let newer = latest.instance != seen.instance || latest.gen > seen.gen;
            if !newer {
                continue;
            }
            let text = self.intel.text_of(&LocalFs, &seen.path);
            let d = FileDiags::diff(&seen.path, &seen.known, latest.items.clone(), &text, latest.encoding, latest.instance, latest.gen);
            *seen = Shown { path: seen.path.clone(), instance: latest.instance, gen: latest.gen, known: latest.items };
            if !d.new_errors.is_empty() {
                out.push(d);
            }
        }
        out
    }

    fn late_block(&self, session: &str) -> String {
        let found = self.late(session);
        if found.is_empty() {
            return String::new();
        }
        let body = render_body(&found, |p| self.intel.rel(p));
        format!("<new-diagnostics>\nThe language server reported these after the edits returned:\n{body}</new-diagnostics>")
    }
}

impl ToolMiddleware for DiagnosticsMiddleware {
    fn name(&self) -> &str {
        "code-diagnostics"
    }

    fn pre_mutation(&self, call: &ToolCall, cx: &CallCtx, mutation: &Mutation) -> Result<(), ToolOutcome> {
        let Mutation::Paths(paths) = mutation else { return Ok(()) };
        let files: Vec<PathBuf> = paths.iter().filter(|p| Lang::of(p).is_some() && self.inside(p)).cloned().collect();
        if files.is_empty() {
            return Ok(());
        }
        let cancel = cx.tool.cancel.clone();
        let cancelled = move || cancel.is_cancelled();
        let caller = super::caller(&cancelled, cx.tool.tainted, self.free);
        let mut snap = self.intel.before_edit(&LocalFs, &files, &caller);
        let before = files
            .iter()
            .filter_map(|f| self.intel.latest(f).map(|l| (norm_key(f), (l.instance, l.gen))))
            .collect();
        // Diff against what the model was shown, not the latest publish: a late error it has not
        // seen yet is still new.
        {
            let shown = self.shown();
            for f in &files {
                let key = (cx.tool.session.0.clone(), norm_key(f));
                if let (Some(seen), Some(latest)) = (shown.get(&key), self.intel.latest(f)) {
                    if seen.instance == latest.instance && snap.covers(f) {
                        snap.assume_known(f, seen.known.clone());
                    }
                }
            }
        }
        let mut pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
        pending.retain(|_, p| p.at.elapsed() < STALE);
        pending.insert(call.id.clone(), Pending { at: Instant::now(), files, snap, before });
        Ok(())
    }

    fn after_call(&self, call: &ToolCall, cx: &CallCtx, outcome: &mut ToolOutcome) {
        let Some(Pending { files, snap, before, .. }) = self.pending.lock().unwrap_or_else(|p| p.into_inner()).remove(&call.id) else {
            return;
        };
        if outcome.is_error {
            return;
        }
        let cancel = cx.tool.cancel.clone();
        let cancelled = move || cancel.is_cancelled();
        let caller = super::caller(&cancelled, cx.tool.tainted, self.free);
        let found = self.intel.after_edit(&LocalFs, &files, &snap, &caller);
        {
            let mut shown = self.shown();
            let session = &cx.tool.session.0;
            for f in &files {
                let seen = match found.iter().find(|d| norm_key(&d.path) == norm_key(f)) {
                    Some(d) => Shown { path: f.clone(), instance: d.instance, gen: d.gen, known: d.now.clone() },
                    // No publish in the window: what was known before stands, and anything the
                    // server published since the edit is checked against it.
                    None if snap.covers(f) => {
                        let (instance, gen) = before.get(&norm_key(f)).copied().unwrap_or((0, 0));
                        Shown { path: f.clone(), instance, gen, known: snap.known(f) }
                    }
                    None => continue,
                };
                shown.insert((session.clone(), norm_key(f)), seen);
            }
        }
        let text = render(&found, |p| self.intel.rel(p));
        if !text.is_empty() {
            outcome.content.push_str(&text);
        }
    }

    fn after_step(&self, _info: StepInfo, convo: &mut Conversation) -> Vec<Message> {
        let block = self.late_block(&convo.session.0);
        if block.is_empty() {
            Vec::new()
        } else {
            vec![Message::user(block)]
        }
    }

    /// Errors that landed since the last step reach the model before it finishes (also those
    /// from a previous turn's edits that landed after its final answer).
    fn before_final(&self, _info: StepInfo, convo: &mut Conversation, _text: &str) -> FinalFlow {
        let block = self.late_block(&convo.session.0);
        if block.is_empty() {
            FinalFlow::Accept
        } else {
            FinalFlow::Continue(block)
        }
    }
}

/// The new errors as a block appended to a tool result; empty when there are none.
pub fn render(found: &[FileDiags], rel: impl Fn(&Path) -> String) -> String {
    let body = render_body(found, rel);
    if body.is_empty() {
        return String::new();
    }
    format!("\n\n<new-diagnostics>\n{body}</new-diagnostics>")
}

/// One line per file with new errors, then each error and its source line.
fn render_body(found: &[FileDiags], rel: impl Fn(&Path) -> String) -> String {
    let mut out = String::new();
    let mut listed = 0;
    for d in found.iter().filter(|d| !d.new_errors.is_empty()) {
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
    out
}
