//! The files group (track A, design 2.1 T1-T9, T19, B1, B6): `read` (text windows, images,
//! PDFs by page, notebooks), `write`, `edit`, `multi_edit`, `edit_lines` (a bench arm, offered
//! only when the host turns it on) and `notebook_edit`. The engines live in `marlowe-code`; this
//! file is their tool surface, and every byte goes through `BuiltinTools::workspace_port`.
//!
//! Read-before-edit: `read` records a `FileState` in the conversation's extensions, and the
//! edit tools record what they wrote. [`FileGuard`] (this group's middleware) refuses an edit of
//! an existing file the agent has not read, or that changed since, before the approval gate asks;
//! the tools re-check staleness under the write lock; after every step, and before each new user
//! turn, it polls the files the agent knows and adds a reminder for any changed by something else
//! (a short diff; a one-line note for an append or a change too large to show, which then needs a
//! re-read before an edit; each file at most once per turn). Calls outside a session (no
//! middleware, empty extensions) keep the old free-form behaviour.
//!
//! Off this machine (UNC hosts, device paths; [`crate::text::off_machine`]) nothing touches the
//! filesystem before approval: `read` and the writers ask, naming the host as egress, the
//! middleware skips its stat, and a tool opens such a path only when `FileGuard::before_call`
//! cleared it, which happens only after the gate.
//!
//! Secret hygiene (B6): writing a secret-shaped file (`.env`, keys, credential stores, judged as
//! Win32 spells the name) or content that looks like a credential asks.
//!
//! Not yet sound outside this file (integrator, see the track report): a session allow for a tool
//! name frees every later ask of that tool, these off-machine and secret asks included (needs a
//! fixed ask in `run_tools`); `CrewTools` does not consult `consequence_in_sandbox` nor forward
//! `call_scoped`/`middleware`, so crew runs have no secret asks on `write`/`edit` and no FileState.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use marlowe_agent::{
    CallCtx, CallFlow, Consequence, Conversation, Effects, Exposure, Mutation, SandboxView, StepInfo, ToolCallReq,
    ToolCtx, ToolGroup, ToolMiddleware, ToolOutcome, TurnFlow,
};
use marlowe_code::edit::{self, FileEdit, MultiError, Planned, TextDoc, Tier};
use marlowe_code::editlines::{self, Anchor, Op};
use marlowe_code::filestate::{self, FileStates, Freshness};
use marlowe_code::notebook::{self, media, pdf, CellEdit, EditMode};
use marlowe_model::{ImageRef, Message, ResultPart, ToolCall, ToolSpec};
use marlowe_os::workspace::{Meta, Workspace};
use serde_json::{json, Value};

use super::{clip, describe_by, spec, ToolDef};
use crate::text::*;
use crate::{int_arg, paths, str_arg, BuiltinTools};

/// Tools that change files and therefore need a fresh read of any existing file they touch.
const WRITE_TOOLS: [&str; 5] = ["write", "edit", "multi_edit", "edit_lines", "notebook_edit"];

/// Files above this size are not hashed: read through a prefix, tracked by size and mtime.
const MAX_HASH_BYTES: u64 = 64 * 1024 * 1024;

/// Largest image `read` attaches (the provider downscales it; track G).
const MAX_IMAGE_BYTES: u64 = 20 * 1024 * 1024;

/// Bytes of rendered notebook or PDF text one `read` returns.
const DOCUMENT_TEXT_MAX: usize = 2 * READ_WINDOW_BYTES;

/// Lines of context around an edit that `edit` shows back.
const SNIPPET_CONTEXT: usize = 3;

/// Lines of a snippet at most.
const SNIPPET_MAX: usize = 16;

pub fn tools() -> Vec<ToolDef> {
    vec![
        ToolDef::new(read_spec(), ToolGroup::Files, Effects::READ, read)
            .path_args(&["path"])
            .consequence(read_consequence)
            .egress(off_machine_host)
            .describe(|c| describe_by(c, "path"))
            .taints(untrusted_read),
        writer(write_spec(), write),
        writer(edit_spec(), edit_tool),
        writer(multi_edit_spec(), multi_edit).describe(describe_multi),
        writer(edit_lines_spec(), edit_lines).enabled(|t| t.state::<EditFormats>().edit_lines),
        writer(notebook_edit_spec(), notebook_edit),
    ]
}

fn writer(spec: ToolSpec, call: super::CallFn) -> ToolDef {
    ToolDef::new(spec, ToolGroup::Files, Effects::WRITE, call)
        .path_args(&["path"])
        .consequence(write_consequence)
        .crew_consequence(crew_write_consequence)
        .egress(off_machine_host)
        .describe(|c| describe_by(c, "path"))
        .mutation(written_paths)
}

fn describe_multi(c: &ToolCallReq) -> String {
    let n = c.args.get("edits").and_then(Value::as_array).map_or(0, Vec::len);
    match targets(&c.args).len() {
        0 | 1 => format!("{} ({n} edits)", describe_by(c, "path")),
        files => format!("multi_edit {n} edits in {files} files"),
    }
}

/// Which edit formats this host offers beyond `edit`/`write` (design 3.2). `edit_lines` is a
/// bench arm: off unless the bench or the model library turns it on with [`with_edit_lines`].
#[derive(Debug, Default, Clone, Copy)]
pub struct EditFormats {
    pub edit_lines: bool,
}

/// Offers `edit_lines`, and makes `read` show `N:hhh|` anchors instead of `cat -n` numbers.
pub fn with_edit_lines(tools: BuiltinTools) -> BuiltinTools {
    tools.with_state(EditFormats { edit_lines: true })
}

/// Where `read` stores image bytes (`<home>\blobs\<blake3>`; track G's store reads them).
#[derive(Debug, Clone)]
pub struct BlobDir(pub PathBuf);

impl Default for BlobDir {
    fn default() -> Self {
        BlobDir(std::env::temp_dir().join("marlowe-blobs"))
    }
}

// ---------------------------------------------------------------- policy

/// Reading outside the user's own folders brings untrusted content in (read, glob, grep). A path
/// off this machine is untrusted without asking the filesystem (that would reach the host).
pub(crate) fn untrusted_read(tools: &BuiltinTools, call: &ToolCallReq) -> bool {
    let raw = str_arg(&call.args, "path").unwrap_or(".");
    off_machine_of(tools.root.as_deref(), raw).is_some() || !tools.trusted_read(raw)
}

/// Where `raw` leaves this machine (a UNC host, a device), judged on strings only: its own
/// spelling, then resolved against `root` (a workspace on a share).
fn off_machine_of(root: Option<&Path>, raw: &str) -> Option<OffMachine> {
    off_machine(raw).or_else(|| off_machine(&paths::resolve(root, raw).to_string_lossy()))
}

/// Asks before a call reaches a network host the user has not allowed in this session, or a
/// device. Runs before anything that touches the filesystem, which for a UNC path is already an
/// SMB connection (non-negotiable 5: new network hosts need approval).
fn off_machine_ask(root: Option<&Path>, call: &ToolCallReq, exposure: &Exposure) -> Option<Consequence> {
    for raw in targets(&call.args) {
        let Some(om) = off_machine_of(root, &raw) else { continue };
        if om.host().is_some_and(|h| exposure.approved_hosts.contains(h)) {
            continue;
        }
        return Some(Consequence::Ask {
            summary: format!("{} {} ({}; nothing leaves this machine without approval)", call.name, clip(&raw, 100), om.describe()),
        });
    }
    None
}

/// The network host a call reaches, recorded as allowed for the session when the user approves.
fn off_machine_host(tools: &BuiltinTools, call: &ToolCallReq) -> Option<String> {
    targets(&call.args)
        .iter()
        .find_map(|raw| off_machine_of(tools.root.as_deref(), raw).and_then(|om| om.host().map(str::to_string)))
}

/// `read` is free on this machine; off it, it asks.
fn read_consequence(tools: &BuiltinTools, call: &ToolCallReq, exposure: &Exposure) -> Consequence {
    off_machine_ask(tools.root.as_deref(), call, exposure).unwrap_or(Consequence::Free)
}

/// The key an approved off-machine target is cleared under (see [`Cleared`]).
fn clear_key(abs: &Path) -> String {
    abs.to_string_lossy().replace('/', "\\").to_lowercase()
}

/// Off-machine targets whose call passed the approval gate: [`FileGuard::before_call`] (which
/// runs only for a call the gate let through) adds one per target, and the tool spends it before
/// it opens the path. Without one (no session, a crew host without the middleware) the tool
/// refuses, so an off-machine path is never opened unapproved.
#[derive(Debug, Default, Clone)]
struct Cleared(std::collections::HashMap<String, usize>);

impl Cleared {
    fn spend(&mut self, key: &str) -> bool {
        match self.0.get_mut(key) {
            Some(n) if *n > 0 => {
                *n -= 1;
                true
            }
            _ => false,
        }
    }
}

/// The raw paths a call names: `path`, then each `edits[].path` (multi_edit), deduplicated.
fn targets(args: &Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let top = str_arg(args, "path").filter(|p| !p.trim().is_empty());
    let mut push = |p: &str| {
        if !out.iter().any(|q| q == p) {
            out.push(p.to_string());
        }
    };
    match args.get("edits").and_then(Value::as_array) {
        Some(edits) => {
            for e in edits {
                if let Some(p) = str_arg(e, "path").filter(|p| !p.trim().is_empty()).or(top) {
                    push(p);
                }
            }
        }
        None => {
            if let Some(p) = top {
                push(p);
            }
        }
    }
    out
}

/// The text a call would put into files (for the credential check).
fn written_text(args: &Value) -> Vec<&str> {
    let mut out: Vec<&str> =
        ["content", "new_string", "text", "new_source"].iter().filter_map(|k| str_arg(args, k)).collect();
    if let Some(edits) = args.get("edits").and_then(Value::as_array) {
        out.extend(edits.iter().filter_map(|e| str_arg(e, "new_string").or_else(|| str_arg(e, "content"))));
    }
    out
}

/// Why the file `raw` names is secret-shaped: judged on its spelling, on the path it resolves to
/// (`cfg/.env/.`), and on what an existing local file really is (a link, an 8.3 short name such
/// as `ENV~1`). Off-machine paths are judged on the strings only.
fn secret_target(root: Option<&Path>, raw: &str) -> Option<&'static str> {
    let abs = paths::resolve(root, raw);
    secret_path(raw).or_else(|| secret_path(&abs.to_string_lossy())).or_else(|| {
        if off_machine_of(root, raw).is_some() {
            return None;
        }
        std::fs::canonicalize(&abs).ok().and_then(|real| secret_path(&real.to_string_lossy()))
    })
}

/// Asks for a secret-shaped target or credential-shaped content (design 2.1 B6).
fn secret_ask(root: Option<&Path>, call: &ToolCallReq) -> Option<Consequence> {
    let ts = targets(&call.args);
    for p in &ts {
        if let Some(why) = secret_target(root, p) {
            return Some(Consequence::Ask {
                summary: format!("{} {} ({why}; secrets need approval)", call.name, crate::first_line(p, 100)),
            });
        }
    }
    for t in written_text(&call.args) {
        if let Some(why) = secret_content(t) {
            let p = ts.first().map(String::as_str).unwrap_or("");
            return Some(Consequence::Ask {
                summary: format!("{} {} (writes what looks like {why})", call.name, crate::first_line(p, 100)),
            });
        }
    }
    None
}

/// Writes inside a root that frees writes are free; anywhere else, off this machine, or a secret,
/// asks.
fn write_consequence(tools: &BuiltinTools, call: &ToolCallReq, exposure: &Exposure) -> Consequence {
    let root = tools.root.as_deref();
    if let Some(ask) = off_machine_ask(root, call, exposure).or_else(|| secret_ask(root, call)) {
        return ask;
    }
    match targets(&call.args).into_iter().find(|p| !tools.inside_root(p)) {
        Some(p) => Consequence::Ask {
            summary: format!("{} {} (outside the workspace)", call.name, crate::first_line(&p, 100)),
        },
        None => Consequence::Free, // a missing path is refused by `call`
    }
}

/// In a crew sandbox: free inside it, ask outside or for a secret.
fn crew_write_consequence(
    _tools: &BuiltinTools,
    call: &ToolCallReq,
    exposure: &Exposure,
    sandbox: &dyn SandboxView,
) -> Consequence {
    let root = Some(sandbox.root());
    if let Some(ask) = off_machine_ask(root, call, exposure).or_else(|| secret_ask(root, call)) {
        return ask;
    }
    match targets(&call.args).into_iter().find(|p| !sandbox.allows_write(Path::new(p))) {
        Some(p) => Consequence::Ask { summary: format!("{} {} (outside the sandbox)", call.name, clip(&p, 100)) },
        None => Consequence::Free, // refused by the inner host for the missing argument
    }
}

fn written_paths(tools: &BuiltinTools, call: &ToolCallReq, ctx: &ToolCtx) -> Mutation {
    let root = tools.root_for_ref(ctx);
    let ps: Vec<PathBuf> = targets(&call.args).iter().map(|p| paths::resolve(root, p)).collect();
    if ps.is_empty() {
        Mutation::None
    } else {
        Mutation::Paths(ps)
    }
}

// ---------------------------------------------------------------- shared call plumbing

/// One call's view of the host: root, port, session file states.
struct Files<'a> {
    tools: &'a BuiltinTools,
    cx: &'a CallCtx,
    root: Option<&'a Path>,
    /// Off-machine targets this call already spent its clearance on.
    spent: std::cell::RefCell<Vec<String>>,
}

impl<'a> Files<'a> {
    fn new(tools: &'a BuiltinTools, cx: &'a CallCtx) -> Self {
        Self { tools, cx, root: tools.root_for_ref(&cx.tool), spent: Default::default() }
    }

    fn port(&self) -> &dyn Workspace {
        self.tools.workspace_port()
    }

    /// `raw` resolved, refused before any filesystem access when it leaves this machine and no
    /// approval cleared it (see [`Cleared`]).
    fn target(&self, raw: &str) -> Result<PathBuf, ToolOutcome> {
        let abs = paths::resolve(self.root, raw);
        let Some(om) = off_machine_of(self.root, raw) else { return Ok(abs) };
        let key = clear_key(&abs);
        if self.spent.borrow().contains(&key) {
            return Ok(abs);
        }
        if self.cx.scope.with_ext(|e| e.get_mut::<Cleared>().is_some_and(|c| c.spend(&key))) {
            self.spent.borrow_mut().push(key);
            return Ok(abs);
        }
        Err(ToolOutcome::error(format!(
            "{} {}, so it was not opened: a path off this machine is opened only after the user \
             approves it in a session.",
            clip(raw, 100),
            om.describe()
        )))
    }

    fn path(&self, args: &Value) -> Result<PathBuf, ToolOutcome> {
        match str_arg(args, "path") {
            Some(p) if !p.trim().is_empty() => self.target(p),
            _ => Err(ToolOutcome::error("`path` is required")),
        }
    }

    fn shown(&self, abs: &Path) -> String {
        paths::display(self.root, abs)
    }

    /// Records what the agent now knows of `abs`.
    fn record(&self, abs: &Path, bytes: &[u8]) {
        let meta = self.port().metadata(abs).ok();
        self.cx.scope.with_ext(|e| e.get_or_default::<FileStates>().record(abs, bytes, meta.as_ref()));
    }

    fn record_meta(&self, abs: &Path, meta: &Meta) {
        self.cx.scope.with_ext(|e| e.get_or_default::<FileStates>().record_meta(abs, meta));
    }

    fn record_planned(&self, planned: &[Planned]) {
        for p in planned {
            self.record(&p.path, &p.after);
        }
    }

    /// Refuses when the agent read `abs` and it changed since (checked under the write lock, so
    /// a change between approval and the write is caught too). Files the agent never read pass:
    /// the middleware refuses those before approval, and calls outside a session have no states.
    /// Off this machine the middleware did not look (that would have reached the host before
    /// approval), so an unread existing file is refused here instead.
    fn guard_fresh(&self, abs: &Path) -> Result<(), ToolOutcome> {
        let port = self.port();
        let strict = off_machine(&abs.to_string_lossy()).is_some();
        let fresh = self.cx.scope.with_ext(|e| {
            let states = e.get_or_default::<FileStates>();
            if !strict {
                states.get(abs)?;
            }
            Some(states.check(port, abs))
        });
        match fresh {
            Some(Freshness::Stale { diff }) => Err(stale(&self.shown(abs), &diff)),
            Some(Freshness::Unread) => Err(unread(&self.shown(abs))),
            _ => Ok(()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ()> {
        self.tools.writes.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn unread(shown: &str) -> ToolOutcome {
    ToolOutcome::error(format!(
        "{shown} exists and you have not read it in this session, so it was not changed. `read` it \
         first: an edit made without seeing the current file can overwrite what is there."
    ))
}

fn stale(shown: &str, diff: &str) -> ToolOutcome {
    let what = if diff.trim().is_empty() { String::new() } else { format!(" What changed:\n{}\n", diff.trim_end()) };
    ToolOutcome::error(format!(
        "{shown} changed since you last read it (something other than your file tools wrote \
         it), so it was not changed.{what} `read` it again and make the edit against what is there now."
    ))
}

fn bool_arg(args: &Value, key: &str) -> bool {
    match args.get(key) {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => s.trim().eq_ignore_ascii_case("true"),
        _ => false,
    }
}

/// `+a −r`, the summary the UI shows.
fn plus_minus(added: usize, removed: usize) -> String {
    format!("+{added} −{removed}")
}

fn numbered_refusal(field: &str) -> ToolOutcome {
    ToolOutcome::error(format!(
        "`{field}` carries `read`'s line-number prefix ({LINE_NUMBER_WIDTH} characters then a tab \
         in front of every line). Writing that would put the numbers in the source, so it is \
         refused — strip the prefixes and call `edit` again. If you really do mean to write \
         numbered text, `write` does that and says so."
    ))
}

fn looks_numbered(s: &str) -> bool {
    count_lines(s) >= NUMBERED_CONTENT_LINES && strip_line_numbers(s).is_some()
}

fn multi_error(f: &Files<'_>, e: MultiError, total: usize) -> ToolOutcome {
    let lead = |i: usize, p: &Path| {
        if total > 1 {
            format!("edit {} of {total} ({}) failed: ", i + 1, f.shown(p))
        } else {
            String::new()
        }
    };
    let tail = if total > 1 { "\nNothing was written: `multi_edit` applies all of its edits or none." } else { "" };
    let body = match e {
        MultiError::Edit { index, path, error } => format!("{}{}", lead(index, &path), error.message()),
        MultiError::Missing { index, path } => format!(
            "{}{} does not exist. Use `write` with `path` and `content` to create it (or an empty \
             `old_string`).",
            lead(index, &path),
            f.shown(&path)
        ),
        MultiError::NotText { path } => format!(
            "{} is not UTF-8 text, so it cannot be edited as text. Use `write` to replace it whole.",
            f.shown(&path)
        ),
        MultiError::Read { path, error } => format!("cannot read {}: {error}", f.shown(&path)),
    };
    ToolOutcome::error(format!("{body}{tail}"))
}

fn commit(f: &Files<'_>, planned: &[Planned]) -> Result<(), ToolOutcome> {
    edit::commit(f.port(), planned).map_err(|e| {
        let list = |v: &[PathBuf]| v.iter().map(|p| f.shown(p)).collect::<Vec<_>>().join(", ");
        let mut m = format!("cannot write {}: {}.", f.shown(&e.path), e.error);
        if !e.restored.is_empty() {
            m.push_str(&format!(" The files already written were put back: {}.", list(&e.restored)));
        }
        if !e.unrestored.is_empty() {
            m.push_str(&format!(" These could NOT be put back and are changed: {}.", list(&e.unrestored)));
        }
        ToolOutcome::error(m)
    })
}

/// Up to [`SNIPPET_MAX`] numbered lines around `line` (1-based) of `text`.
fn snippet(text: &str, line: usize, span: usize) -> String {
    let from = line.saturating_sub(SNIPPET_CONTEXT).max(1);
    let to = (line + span.max(1) - 1 + SNIPPET_CONTEXT).min(from + SNIPPET_MAX - 1);
    let body: String = text.split_inclusive('\n').skip(from - 1).take(to + 1 - from).collect();
    number_lines(&body, from).trim_end().to_string()
}

fn tier_note(t: Tier) -> String {
    if t == Tier::Exact {
        String::new()
    } else {
        format!(" ({})", t.note())
    }
}

// ---------------------------------------------------------------- read

fn read(tools: &BuiltinTools, args: &Value, cx: &CallCtx) -> ToolOutcome {
    let f = Files::new(tools, cx);
    let abs = match f.path(args) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let shown = f.shown(&abs);
    let meta = match f.port().metadata(&abs) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return ToolOutcome::error(format!("cannot read {shown}: it does not exist."))
        }
        Err(e) => return ToolOutcome::error(format!("cannot read {shown}: {e}")),
    };
    if meta.is_dir {
        return ToolOutcome::error(format!(
            "{shown} is a directory. `read` reads one file; use `glob` to list a directory."
        ));
    }
    let name = abs.file_name().map(|n| n.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    // Huge files on this machine are streamed a window at a time and tracked by size and mtime.
    if meta.len > MAX_HASH_BYTES && f.port().kind() == "local" {
        f.record_meta(&abs, &meta);
        return read_huge(tools, args, &abs, &shown, meta.len);
    }
    let bytes = match f.port().read(&abs) {
        Ok(b) => b,
        Err(e) => return ToolOutcome::error(format!("cannot read {shown}: {e}")),
    };
    f.record(&abs, &bytes);
    if name.ends_with(".ipynb") {
        if let Ok(nb) = notebook::parse(&bytes) {
            return read_notebook(tools, args, &shown, &nb);
        }
    }
    if let Some((mime, w, h)) = media::sniff_image(&bytes) {
        return read_image(tools, &shown, &bytes, meta.len, mime, w, h);
    }
    if pdf::is_pdf(&bytes) {
        return read_pdf(&shown, &bytes, str_arg(args, "pages"));
    }
    if let Some(e) = pages_not_pdf(args, &shown) {
        return e;
    }
    read_text(tools, args, &shown, &bytes)
}

fn pages_not_pdf(args: &Value, shown: &str) -> Option<ToolOutcome> {
    args.get("pages").is_some_and(|p| !p.is_null()).then(|| {
        ToolOutcome::error(format!("`pages` is for PDFs, and {shown} is not one. Use `offset` and `limit`."))
    })
}

/// The whole text of `bytes` (BOM and UTF-16 decoded) and whether invalid UTF-8 was replaced;
/// `None` for binary. A large file with a stray invalid byte (a log) still reads as text.
fn decode_text(bytes: &[u8]) -> Option<(String, bool)> {
    if let Some(d) = TextDoc::decode(bytes) {
        return Some((d.text, false));
    }
    let body = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    let head = &body[..body.len().min(8 * 1024)];
    // An invalid sequence in the head means binary; one cut off at the head's end does not.
    let invalid_head = std::str::from_utf8(head).is_err_and(|e| e.error_len().is_some());
    if (bytes.len() as u64) <= MAX_READ_BYTES || head.contains(&0) || invalid_head {
        return None;
    }
    Some((String::from_utf8_lossy(body).into_owned(), true))
}

/// Which lines a text read asks for: an explicit `range`, else `offset` (1-based, default 1)
/// and `limit` (default the window).
#[derive(Clone, Copy)]
struct Want<'a> {
    range: Option<&'a str>,
    first: usize,
    /// Lines asked for (the window may still cut them short).
    n: usize,
    limit: Option<usize>,
}

impl<'a> Want<'a> {
    fn of(args: &'a Value) -> Result<Self, ToolOutcome> {
        let limit = int_arg(args, "limit").filter(|&l| l > 0).map(|l| usize::try_from(l).unwrap_or(usize::MAX));
        if let Some(range) = str_arg(args, "range") {
            let (a, b) = parse_range(range).map_err(ToolOutcome::error)?;
            return Ok(Want { range: Some(range), first: a, n: (b - a).saturating_add(1), limit });
        }
        let first = usize::try_from(int_arg(args, "offset").unwrap_or(1).max(1)).unwrap_or(usize::MAX);
        Ok(Want { range: None, first, n: limit.unwrap_or(READ_WINDOW_LINES), limit })
    }

    /// The refusal for a window that starts past the last line.
    fn past_end(&self, shown: &str, total: usize) -> ToolOutcome {
        match self.range {
            Some(range) => ToolOutcome::error(format!(
                "`range` \"{range}\" selected no lines: the file has {total}. Ask for a range inside \
                 1-{total}, or omit `range` for the whole file."
            )),
            None => ToolOutcome::error(format!(
                "`offset` {} is past the end: {shown} has {total} lines. Use an offset from 1 to {total}.",
                self.first
            )),
        }
    }
}

fn read_text(tools: &BuiltinTools, args: &Value, shown: &str, bytes: &[u8]) -> ToolOutcome {
    let size = bytes.len();
    let Some((text, lossy)) = decode_text(bytes) else {
        return ToolOutcome::ok(
            format!("{shown} is not UTF-8 text ({size} bytes), so its bytes were not returned."),
            format!("binary · {size} B"),
        );
    };
    if text.is_empty() {
        return ToolOutcome::ok(format!("{shown} is empty (0 bytes)."), "0 lines · 0 B");
    }
    let want = match Want::of(args) {
        Ok(w) => w,
        Err(e) => return e,
    };
    let total = count_lines(&text);
    if want.first > total {
        return want.past_end(shown, total);
    }
    // Only the lines the window can show are copied, however many were asked for.
    let window: String = text.split_inclusive('\n').skip(want.first - 1).take(want.n.min(READ_WINDOW_LINES)).collect();
    let note = (lossy && window.contains('\u{fffd}'))
        .then(|| "[the file is not valid UTF-8 throughout; invalid bytes show as \u{fffd}.]".to_string());
    let whole_file_tokens = estimate_tokens_of(text.len() + total * (LINE_NUMBER_WIDTH + 1));
    numbered_window(tools, &want, window, total, whole_file_tokens, note)
}

/// A file above [`MAX_HASH_BYTES`] on this machine: the asked-for window is streamed from disk
/// (any offset, never the whole file in memory) and the total line count is counted on the way.
fn read_huge(tools: &BuiltinTools, args: &Value, abs: &Path, shown: &str, size: u64) -> ToolOutcome {
    let head = match read_head(abs, 64 * 1024) {
        Ok(h) => h,
        Err(e) => return ToolOutcome::error(format!("cannot read {shown}: {e}")),
    };
    if let Some((mime, w, h)) = media::sniff_image(&head) {
        return read_image(tools, shown, &head, size, mime, w, h);
    }
    if pdf::is_pdf(&head) {
        return ToolOutcome::error(format!(
            "{shown} is a PDF of {}, larger than the {} `read` extracts text from.",
            kib(size),
            kib(MAX_HASH_BYTES)
        ));
    }
    if let Some(e) = pages_not_pdf(args, shown) {
        return e;
    }
    let sample = head.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&head);
    if sample.contains(&0) {
        return ToolOutcome::ok(
            format!("{shown} is not UTF-8 text ({size} bytes), so its bytes were not returned."),
            format!("binary · {size} B"),
        );
    }
    let want = match Want::of(args) {
        Ok(w) => w,
        Err(e) => return e,
    };
    let (raw, total) = match stream_lines(abs, want.first, want.n.min(READ_WINDOW_LINES)) {
        Ok(r) => r,
        Err(e) => return ToolOutcome::error(format!("cannot read {shown}: {e}")),
    };
    if want.first > total {
        return want.past_end(shown, total);
    }
    let raw = if want.first == 1 { raw.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&raw).to_vec() } else { raw };
    let window = String::from_utf8_lossy(&raw).into_owned();
    let whole_file_tokens = estimate_tokens_of(size as usize + total * (LINE_NUMBER_WIDTH + 1));
    numbered_window(tools, &want, window, total, whole_file_tokens, None)
}

/// The first `n` bytes of a file.
fn read_head(abs: &Path, n: u64) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut buf = Vec::new();
    std::fs::File::open(abs)?.take(n).read_to_end(&mut buf)?;
    Ok(buf)
}

/// Lines `first..first + n` (1-based) of a file, each kept to what [`clip_long_lines`] can still
/// mark as cut, and the file's line count. One pass, a buffer at a time.
fn stream_lines(abs: &Path, first: usize, n: usize) -> std::io::Result<(Vec<u8>, usize)> {
    use std::io::BufRead;
    // Enough bytes for MAX_LINE_CHARS characters of any width, plus one more to show the cut.
    const LINE_KEEP: usize = (MAX_LINE_CHARS + 1) * 4;
    let mut r = std::io::BufReader::with_capacity(1 << 20, std::fs::File::open(abs)?);
    let (mut line, mut kept, mut ended) = (1usize, 0usize, true);
    let mut out = Vec::new();
    let end = first.saturating_add(n);
    loop {
        let buf = r.fill_buf()?;
        if buf.is_empty() {
            break;
        }
        let len = buf.len();
        let mut i = 0;
        while i < len {
            let rest = &buf[i..];
            let nl = rest.iter().position(|&b| b == b'\n');
            let seg = nl.map_or(rest.len(), |p| p + 1);
            if line >= first && line < end {
                let take = seg.min(LINE_KEEP.saturating_sub(kept));
                out.extend_from_slice(&rest[..take]);
                kept += take;
                if nl.is_some() && take < seg {
                    out.push(b'\n');
                }
            }
            ended = nl.is_some();
            if ended {
                line += 1;
                kept = 0;
            }
            i += seg;
        }
        r.consume(len);
    }
    Ok((out, if ended { line - 1 } else { line }))
}

/// Numbers a selected window, applies the line and byte ceilings, and says how to continue.
fn numbered_window(
    tools: &BuiltinTools,
    want: &Want<'_>,
    window: String,
    total: usize,
    whole_file_tokens: u32,
    note: Option<String>,
) -> ToolOutcome {
    let first = want.first;
    let wanted = want.n.min(total - first + 1);
    let mut kept = wanted.min(READ_WINDOW_LINES);
    let window = if wanted > READ_WINDOW_LINES { take_lines(&window, READ_WINDOW_LINES) } else { window };
    let (window, clipped) = clip_long_lines(&window);
    // Shown with LF endings (as `cat -n` in a terminal reads): a CR is noise to the model, and an
    // LF snippet copied from here still lands on a CRLF file (the edit's EOL tier).
    let window = window.replace("\r\n", "\n");
    let anchors = tools.state::<EditFormats>().edit_lines;
    let mut out = if anchors { editlines::anchored(&window, first) } else { number_lines(&window, first) };
    let cap = window_bytes(tools.context_tokens);
    if out.len() > cap {
        let (cut, lines) = cut_to_whole_lines(&out, cap);
        out = cut;
        kept = lines;
    }
    if clipped > 0 {
        out.push_str(&format!(
            "\n\n[{clipped} line(s) longer than {MAX_LINE_CHARS} characters were cut; `grep` finds \
             text inside them.]"
        ));
    }
    if let Some(note) = note {
        out.push_str(&format!("\n\n{note}"));
    }
    let last = first + kept.saturating_sub(1);
    // An explicit range or limit continues only if the window cut it short; a default window
    // continues whenever the file goes on.
    let more = if want.range.is_some() || want.limit.is_some() { kept < wanted } else { last < total };
    if more {
        let cost = if whole_file_tokens > tools.context_tokens / 4 {
            format!(
                " The whole file is about {whole_file_tokens} tokens; to find something specific, \
                 `grep` searches inside files and returns `path:line:text`."
            )
        } else {
            String::new()
        };
        let next = if want.range.is_some() {
            format!("range \"{}-{}\"", last + 1, (last + READ_WINDOW_LINES).min(total))
        } else {
            format!("offset {}", last + 1)
        };
        out.push_str(&format!("\n\n[showing lines {first}-{last} of {total}. Continue with {next}.{cost}]"));
    }
    let summary = format!("{kept} lines · {} B", out.len());
    ToolOutcome::ok(out, summary)
}

/// Bytes one text window may take: [`READ_WINDOW_BYTES`] on a small context, up to an eighth of a
/// larger window (about 4 bytes a token), never more than [`READ_WINDOW_BYTES_MAX`] (Claude
/// Code's 25k-token read cap).
fn window_bytes(context_tokens: u32) -> usize {
    (context_tokens as usize / 8 * 4).clamp(READ_WINDOW_BYTES, READ_WINDOW_BYTES_MAX)
}

/// Stores `bytes` in the blob store under their blake3 and returns the reference.
fn store_blob(tools: &BuiltinTools, bytes: &[u8], mime: &str, w: u32, h: u32) -> Result<ImageRef, String> {
    let hex = filestate::hex(&filestate::digest(bytes));
    let dir = tools.state::<BlobDir>().0.clone();
    let path = dir.join(&hex);
    if !path.exists() {
        std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create the image store {}: {e}", dir.display()))?;
        let tmp = dir.join(format!(".{hex}.{}.tmp", std::process::id()));
        std::fs::write(&tmp, bytes).and_then(|_| std::fs::rename(&tmp, &path)).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("cannot store the image: {e}")
        })?;
    }
    Ok(ImageRef { mime: mime.to_string(), blob: hex, w, h })
}

fn kib(n: u64) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{:.1} KB", n as f64 / 1024.0)
    } else {
        format!("{:.1} MB", n as f64 / (1024.0 * 1024.0))
    }
}

fn read_image(tools: &BuiltinTools, shown: &str, bytes: &[u8], size: u64, mime: &str, w: u32, h: u32) -> ToolOutcome {
    if size > MAX_IMAGE_BYTES {
        return ToolOutcome::error(format!(
            "{shown} is an image of {} ({w}x{h}), larger than the {} `read` attaches.",
            kib(size),
            kib(MAX_IMAGE_BYTES)
        ));
    }
    match store_blob(tools, bytes, mime, w, h) {
        Ok(img) => ToolOutcome::ok(
            format!("{shown}: {mime}, {w}x{h}, {} (attached as an image).", kib(size)),
            format!("image · {w}x{h}"),
        )
        .with_part(ResultPart::Image(img)),
        Err(e) => ToolOutcome::error(e),
    }
}

fn cap_document(mut text: String) -> String {
    if text.len() > DOCUMENT_TEXT_MAX {
        let cut = floor_boundary(&text, DOCUMENT_TEXT_MAX);
        let more = text.len() - cut;
        text.truncate(cut);
        text.push_str(&format!("\n\n[the text continues for {more} more bytes; ask for fewer `pages`.]"));
    }
    text
}

fn read_pdf(shown: &str, bytes: &[u8], pages: Option<&str>) -> ToolOutcome {
    let texts = match pdf::page_texts(bytes) {
        Ok(t) => t,
        Err(e) => return ToolOutcome::error(format!("{shown}: {e}")),
    };
    let total = texts.len();
    let selected: Vec<usize> = match pages.map(str::trim).filter(|p| !p.is_empty()) {
        Some(spec) => match pdf::parse_pages(spec, total) {
            Ok(s) => s,
            Err(e) => return ToolOutcome::error(e),
        },
        None if total > pdf::PAGES_REQUIRED_OVER => {
            return ToolOutcome::error(format!(
                "{shown} is a PDF of {total} pages, more than the {} `read` returns without `pages`. \
                 Ask for up to {} at a time, e.g. `pages: \"1-{}\"`.",
                pdf::PAGES_REQUIRED_OVER,
                pdf::MAX_PAGES_PER_READ,
                pdf::MAX_PAGES_PER_READ.min(total)
            ))
        }
        None => (1..=total).collect(),
    };
    let mut out = String::new();
    let mut empty = 0usize;
    for &p in &selected {
        let t = texts[p - 1].trim();
        if t.is_empty() {
            empty += 1;
        }
        out.push_str(&format!("--- page {p} of {total} ---\n{t}\n\n"));
    }
    if empty == selected.len() {
        out.push_str(
            "[no text layer on these pages: they are probably scanned images, which this build cannot read.]",
        );
    }
    let out = cap_document(out.trim_end().to_string());
    let summary = format!("pdf · {} of {total} pages", selected.len());
    ToolOutcome::ok(out, summary)
}

/// A notebook as cells with their outputs. `offset` is the first cell's 0-based index (as
/// `<cell index=..>` and `notebook_edit` count) and `limit` a number of cells; whole cells come
/// back until the window's byte cap, and a cut says exactly where to continue.
fn read_notebook(tools: &BuiltinTools, args: &Value, shown: &str, nb: &Value) -> ToolOutcome {
    let from = usize::try_from(int_arg(args, "offset").unwrap_or(0).max(0)).unwrap_or(usize::MAX);
    let limit = int_arg(args, "limit").filter(|&l| l > 0).map_or(usize::MAX, |l| usize::try_from(l).unwrap_or(usize::MAX));
    let cells = nb["cells"].as_array().map_or(0, Vec::len);
    if from > 0 && from >= cells {
        return ToolOutcome::error(format!(
            "`offset` {from} is past the last cell: {shown} has {cells} cells, numbered 0-{}. For a \
             notebook, `offset` is a 0-based cell index.",
            cells.saturating_sub(1)
        ));
    }
    let r = notebook::render_range(nb, from, limit, window_bytes(tools.context_tokens));
    let mut text = format!("{shown}: Jupyter notebook, {cells} cells.\n");
    let partial = r.first > 0 || r.end < cells;
    if partial && r.end > r.first {
        text.push_str(&format!("Cells {}-{}:\n", r.first, r.end - 1));
    }
    text.push_str(r.text.trim_end());
    let asked_end = from.saturating_add(limit).min(cells);
    if r.end < asked_end {
        let why = if r.cut { "the cell above is larger than one read returns" } else { "the read's size cap" };
        text.push_str(&format!(
            "\n\n[showing cells {}-{} of {cells} ({why}). Continue with offset {} (cells are 0-based).]",
            r.first,
            r.end - 1,
            r.end
        ));
    }
    let mut out = ToolOutcome::ok(text, format!("notebook · {} of {cells} cells", r.end - r.first));
    for img in &r.images {
        match store_blob(tools, &img.bytes, img.mime, img.width, img.height) {
            Ok(i) => out = out.with_part(ResultPart::Image(i)),
            Err(e) => out.content.push_str(&format!("\n[an image output of cell {} was not attached: {e}]", img.cell)),
        }
    }
    out
}

// ---------------------------------------------------------------- write

fn write(tools: &BuiltinTools, args: &Value, cx: &CallCtx) -> ToolOutcome {
    let f = Files::new(tools, cx);
    let abs = match f.path(args) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let Some(content) = str_arg(args, "content") else {
        return ToolOutcome::error("`content` is required");
    };
    let shown = f.shown(&abs);
    let _guard = f.lock();
    if let Err(o) = f.guard_fresh(&abs) {
        return o;
    }
    if f.port().metadata(&abs).is_ok_and(|m| m.is_dir) {
        return ToolOutcome::error(format!("{shown} is a directory; `write` needs a file path."));
    }
    let planned = match edit::plan_write(f.port(), &abs, content) {
        Ok(p) => p,
        Err(e) => return multi_error(&f, e, 1),
    };
    if let Err(o) = commit(&f, std::slice::from_ref(&planned)) {
        return o;
    }
    f.record_planned(std::slice::from_ref(&planned));
    let r = &planned.edits[0];
    let mut body = format!("wrote {shown} (+{} -{})", r.added, r.removed);
    if let Some(before) = planned.before.as_deref().and_then(TextDoc::decode) {
        let mut kept = Vec::new();
        if before.has_bom() {
            kept.push("its byte-order mark");
        }
        if before.eol == edit::Eol::Crlf && !content.contains("\r\n") && content.contains('\n') {
            kept.push("CRLF line endings");
        }
        if !kept.is_empty() {
            body.push_str(&format!("; kept {}", kept.join(" and ")));
        }
    }
    if r.added >= NUMBERED_CONTENT_LINES && strip_line_numbers(content).is_some() {
        body.push_str(&format!(
            "\nthe file was written, and its lines carry `read`'s line-number prefix \
             ({LINE_NUMBER_WIDTH} characters then a tab). If that came from pasting a `read` \
             result, the numbers are now IN the file — write it again without them."
        ));
    }
    ToolOutcome::ok(body, plus_minus(r.added, r.removed))
}

// ---------------------------------------------------------------- edit

fn edit_tool(tools: &BuiltinTools, args: &Value, cx: &CallCtx) -> ToolOutcome {
    let f = Files::new(tools, cx);
    let abs = match f.path(args) {
        Ok(p) => p,
        Err(e) => return e,
    };
    // `replacing`/`content` are the pre-M4.6 names, still accepted.
    let legacy = (args.get("replacing").is_some() || args.get("content").is_some())
        && args.get("old_string").is_none()
        && args.get("new_string").is_none();
    let (old_key, new_key) = if legacy { ("replacing", "content") } else { ("old_string", "new_string") };
    let Some(new) = str_arg(args, new_key) else {
        return ToolOutcome::error(format!("`{new_key}` is required"));
    };
    let Some(old) = str_arg(args, old_key) else {
        return ToolOutcome::error(format!(
            "`{old_key}` is required: `edit` replaces one exact snippet. To create a file or replace \
             all of it, use `write` instead."
        ));
    };
    let replace_all = bool_arg(args, "replace_all");
    let shown = f.shown(&abs);
    if looks_numbered(new) {
        return numbered_refusal(new_key);
    }
    let _guard = f.lock();
    if let Err(o) = f.guard_fresh(&abs) {
        return o;
    }
    match f.port().read(&abs) {
        Ok(b) if b.is_empty() && !old.is_empty() => {
            return ToolOutcome::error(
                "the file is empty, so there is no snippet to replace. Use `write` with `path` and \
                 `content` to fill it.",
            )
        }
        Ok(b) if old.is_empty() && !b.is_empty() => {
            return ToolOutcome::error(edit::EditError::EmptyOld.message().replace("old_string", old_key))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !old.is_empty() => {
            return ToolOutcome::error(format!("{shown} does not exist. Use `write` with `path` and `content` to create it."))
        }
        _ => {}
    }
    let edits = [FileEdit { path: abs.clone(), old: old.to_string(), new: new.to_string(), replace_all }];
    let planned = match edit::plan(f.port(), &edits) {
        Ok(p) => p,
        Err(e) => return multi_error(&f, e, 1),
    };
    if let Err(o) = commit(&f, &planned) {
        return o;
    }
    f.record_planned(&planned);
    let p = &planned[0];
    let r = &p.edits[0];
    let verb = if r.created { "created" } else { "edited" };
    let mut body = format!("{verb} {shown} (+{} -{})", r.added, r.removed);
    if r.count > 1 {
        body.push_str(&format!(", replacing {} occurrences at lines {}", r.count, join_lines(&r.lines)));
    }
    body.push_str(&tier_note(r.tier));
    if !r.created && !p.text.is_empty() {
        let span = count_lines(new).max(1);
        body.push_str(&format!(":\n{}", snippet(&p.text, r.lines[0], span)));
    }
    ToolOutcome::ok(body, plus_minus(r.added, r.removed))
}

fn join_lines(lines: &[usize]) -> String {
    let shown: Vec<String> = lines.iter().take(MAX_EDIT_SITES_NAMED).map(usize::to_string).collect();
    let more = lines.len().saturating_sub(MAX_EDIT_SITES_NAMED);
    if more > 0 {
        format!("{} and {more} more", shown.join(", "))
    } else {
        shown.join(", ")
    }
}

// ---------------------------------------------------------------- multi_edit

fn multi_edit(tools: &BuiltinTools, args: &Value, cx: &CallCtx) -> ToolOutcome {
    let f = Files::new(tools, cx);
    let Some(list) = args.get("edits").and_then(Value::as_array).filter(|l| !l.is_empty()) else {
        return ToolOutcome::error(
            "`edits` is required: a list of {old_string, new_string} objects (each may name its own \
             `path`; otherwise the top-level `path` is used).",
        );
    };
    let top = str_arg(args, "path").filter(|p| !p.trim().is_empty());
    let mut edits = Vec::with_capacity(list.len());
    for (i, e) in list.iter().enumerate() {
        let n = i + 1;
        let Some(raw) = str_arg(e, "path").filter(|p| !p.trim().is_empty()).or(top) else {
            return ToolOutcome::error(format!("edit {n} has no `path`, and there is no top-level `path`."));
        };
        let old = str_arg(e, "old_string").or_else(|| str_arg(e, "replacing"));
        let new = str_arg(e, "new_string").or_else(|| str_arg(e, "content"));
        let (Some(old), Some(new)) = (old, new) else {
            return ToolOutcome::error(format!("edit {n} needs `old_string` and `new_string`. Nothing was written."));
        };
        if looks_numbered(new) {
            return ToolOutcome::error(format!("edit {n}: {}", numbered_refusal("new_string").content));
        }
        let path = match f.target(raw) {
            Ok(p) => p,
            Err(o) => return o,
        };
        edits.push(FileEdit { path, old: old.into(), new: new.into(), replace_all: bool_arg(e, "replace_all") });
    }
    let _guard = f.lock();
    let mut seen: Vec<&Path> = Vec::new();
    for e in &edits {
        if !seen.contains(&e.path.as_path()) {
            seen.push(&e.path);
            if let Err(o) = f.guard_fresh(&e.path) {
                return o;
            }
        }
    }
    let planned = match edit::plan(f.port(), &edits) {
        Ok(p) => p,
        Err(e) => return multi_error(&f, e, edits.len()),
    };
    if let Err(o) = commit(&f, &planned) {
        return o;
    }
    f.record_planned(&planned);
    let (mut added, mut removed) = (0usize, 0usize);
    let mut lines = Vec::new();
    for p in &planned {
        let (a, r) = p.edits.iter().fold((0usize, 0usize), |(a, r), e| (a + e.added, r + e.removed));
        added += a;
        removed += r;
        let created = if p.edits.iter().any(|e| e.created) { ", created" } else { "" };
        lines.push(format!("- {}: {} edit(s){created} (+{a} -{r})", f.shown(&p.path), p.edits.len()));
        for e in p.edits.iter().filter(|e| e.tier != Tier::Exact) {
            lines.push(format!("  edit {}: {}", e.index + 1, e.tier.note()));
        }
    }
    ToolOutcome::ok(
        format!("applied {} edits to {} file(s), all or nothing:\n{}", edits.len(), planned.len(), lines.join("\n")),
        plus_minus(added, removed),
    )
}

// ---------------------------------------------------------------- edit_lines

fn edit_lines(tools: &BuiltinTools, args: &Value, cx: &CallCtx) -> ToolOutcome {
    let f = Files::new(tools, cx);
    let abs = match f.path(args) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let shown = f.shown(&abs);
    let anchor = match Anchor::parse(str_arg(args, "anchor").unwrap_or("")) {
        Ok(a) => a,
        Err(e) => return ToolOutcome::error(e.message()),
    };
    let end = match str_arg(args, "end").filter(|e| !e.trim().is_empty()) {
        Some(e) => match Anchor::parse(e) {
            Ok(a) => Some(a),
            Err(e) => return ToolOutcome::error(e.message()),
        },
        None => None,
    };
    let Some(op) = Op::parse(str_arg(args, "op").unwrap_or("replace")) else {
        return ToolOutcome::error("`op` must be replace, insert_after, insert_before or delete.");
    };
    if op != Op::Delete && str_arg(args, "text").is_none() {
        return ToolOutcome::error("`text` is required (the new lines).");
    }
    let text = str_arg(args, "text").unwrap_or("");
    let _guard = f.lock();
    if let Err(o) = f.guard_fresh(&abs) {
        return o;
    }
    let (before, doc) = match f.port().read(&abs) {
        Ok(b) => match TextDoc::decode(&b) {
            Some(d) => (Some(b), d),
            None => return ToolOutcome::error(format!("{shown} is not UTF-8 text.")),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && anchor.line == 0 => (None, TextDoc::utf8("")),
        Err(e) => return ToolOutcome::error(format!("cannot read {shown}: {e}")),
    };
    let applied = match editlines::apply(&doc.text, &anchor, end.as_ref(), op, text) {
        Ok(a) => a,
        Err(e) => return ToolOutcome::error(e.message()),
    };
    let planned = Planned {
        path: abs.clone(),
        before,
        after: doc.encode_text(&applied.text),
        text: applied.text.clone(),
        edits: Vec::new(),
    };
    if let Err(o) = commit(&f, std::slice::from_ref(&planned)) {
        return o;
    }
    f.record_planned(std::slice::from_ref(&planned));
    let from = applied.first.saturating_sub(2).max(1);
    let view: String = applied.text.split_inclusive('\n').skip(from - 1).take(applied.added + 4).collect();
    ToolOutcome::ok(
        format!(
            "edited {shown} (+{} -{}). Fresh anchors around the change:\n{}",
            applied.added,
            applied.removed,
            editlines::anchored(&view, from).trim_end()
        ),
        plus_minus(applied.added, applied.removed),
    )
}

// ---------------------------------------------------------------- notebook_edit

fn notebook_edit(tools: &BuiltinTools, args: &Value, cx: &CallCtx) -> ToolOutcome {
    let f = Files::new(tools, cx);
    let abs = match f.path(args) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let shown = f.shown(&abs);
    let Some(mode) = EditMode::parse(str_arg(args, "mode").unwrap_or("replace")) else {
        return ToolOutcome::error("`mode` must be replace, insert or delete.");
    };
    let cell = match args.get("cell") {
        Some(Value::String(s)) if !s.trim().is_empty() => Some(s.trim().to_string()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    };
    let source = str_arg(args, "new_source");
    if mode != EditMode::Delete && source.is_none() {
        return ToolOutcome::error("`new_source` is required to replace or insert a cell.");
    }
    let _guard = f.lock();
    if let Err(o) = f.guard_fresh(&abs) {
        return o;
    }
    let bytes = match f.port().read(&abs) {
        Ok(b) => b,
        Err(e) => return ToolOutcome::error(format!("cannot read {shown}: {e}")),
    };
    let mut nb = match notebook::parse(&bytes) {
        Ok(nb) => nb,
        Err(e) => return ToolOutcome::error(format!("{shown}: {e}")),
    };
    let cell_edit = CellEdit {
        cell,
        source: source.unwrap_or("").to_string(),
        cell_type: str_arg(args, "cell_type").map(str::to_string),
        mode,
    };
    let done = match notebook::edit(&mut nb, &cell_edit) {
        Ok(d) => d,
        Err(e) => return ToolOutcome::error(format!("{shown}: {e}. The notebook is unchanged.")),
    };
    let original = String::from_utf8_lossy(&bytes).into_owned();
    let after = notebook::serialize(&nb, notebook::indent_of(&original), original.contains("\r\n"));
    let planned = Planned { path: abs.clone(), before: Some(bytes), after, text: String::new(), edits: Vec::new() };
    if let Err(o) = commit(&f, std::slice::from_ref(&planned)) {
        return o;
    }
    f.record(&abs, &planned.after);
    let id = done.id.as_deref().map(|i| format!(" (id {i})")).unwrap_or_default();
    let what = match mode {
        EditMode::Replace => format!("replaced cell {}{id}", done.index),
        EditMode::Insert => format!("inserted cell {}{id}", done.index),
        EditMode::Delete => format!("deleted cell {}{id}", done.index),
    };
    ToolOutcome::ok(format!("{what} in {shown}; it now has {} cells.", done.cells), what)
}

// ---------------------------------------------------------------- middleware

/// Read-before-edit, stale-file refusals and the external-change reminder (design 2.1 T7, B1).
pub struct FileGuard {
    port: Arc<dyn Workspace>,
    root: Option<PathBuf>,
}

impl ToolMiddleware for FileGuard {
    fn name(&self) -> &str {
        "files"
    }

    /// Before turn one of a new user message: files the agent knows that changed since its last
    /// step (the user edited them between turns). The reminder goes in as its own message ahead
    /// of the user's, never into `text`: the user's words are what memory ingests at user trust.
    fn before_turn(&self, convo: &mut Conversation, _text: &mut String) -> TurnFlow {
        if let Some(states) = convo.ext.get_mut::<FileStates>() {
            states.begin_turn();
        }
        if let Some(m) = self.reminder(convo) {
            convo.push(m);
        }
        TurnFlow::Continue
    }

    fn before_approval(&self, call: &mut ToolCall, cx: &CallCtx) -> CallFlow {
        if !WRITE_TOOLS.contains(&call.name.as_str()) {
            return CallFlow::Continue;
        }
        let root = self.root.as_deref().or(cx.workspace.as_deref());
        for raw in targets(&call.args) {
            // Off this machine, even a stat reaches the host: the gate asks first, and the tool
            // applies read-before-edit itself once approved.
            if off_machine_of(root, &raw).is_some() {
                continue;
            }
            let abs = paths::resolve(root, &raw);
            let fresh = cx.scope.with_ext(|e| e.get_or_default::<FileStates>().check(self.port.as_ref(), &abs));
            let shown = paths::display(root, &abs);
            match fresh {
                Freshness::Unread => return CallFlow::Done(unread(&shown)),
                Freshness::Stale { diff } => return CallFlow::Done(stale(&shown, &diff)),
                Freshness::Absent | Freshness::Fresh => {}
            }
        }
        CallFlow::Continue
    }

    /// Runs only for a call the approval gate let through: clears its off-machine targets for
    /// the tool (see [`Cleared`]).
    fn before_call(&self, call: &ToolCall, cx: &CallCtx) -> CallFlow {
        if call.name != "read" && !WRITE_TOOLS.contains(&call.name.as_str()) {
            return CallFlow::Continue;
        }
        let root = self.root.as_deref().or(cx.workspace.as_deref());
        for raw in targets(&call.args) {
            if off_machine_of(root, &raw).is_some() {
                let key = clear_key(&paths::resolve(root, &raw));
                cx.scope.with_ext(|e| *e.get_or_default::<Cleared>().0.entry(key).or_default() += 1);
            }
        }
        CallFlow::Continue
    }

    fn after_step(&self, _info: StepInfo, convo: &mut Conversation) -> Vec<Message> {
        self.reminder(convo).into_iter().collect()
    }
}

impl FileGuard {
    /// The reminder for files the agent knows that changed outside its tools, if any.
    fn reminder(&self, convo: &mut Conversation) -> Option<Message> {
        let root = self.root.clone().or_else(|| convo.workspace.clone());
        let changes = convo.ext.get_mut::<FileStates>()?.poll(self.port.as_ref());
        if changes.is_empty() {
            return None;
        }
        Some(Message::user(filestate::reminder(&changes, &|p| paths::display(root.as_deref(), p))))
    }
}

/// Loop middleware for this group, built once per host next to its state (see `specs/mod.rs`).
pub fn middleware(tools: &crate::BuiltinTools) -> Vec<Arc<dyn ToolMiddleware>> {
    vec![Arc::new(FileGuard { port: Arc::clone(&tools.port), root: tools.root.clone() })]
}

/// This group's `persona/blocks/<group>.md` (`include_str!`) once it exists; appended only while
/// the group is exposed and has a live tool.
pub fn persona_block() -> Option<&'static str> {
    None
}

/// Per-host setup from the profile home: images go to `<home>\blobs`.
pub fn configure(tools: crate::BuiltinTools, setup: &super::HostSetup<'_>) -> crate::BuiltinTools {
    tools.with_state(BlobDir(setup.home.join("blobs")))
}

/// Whether a local model gets this group in `root`. Always exposed to local models (a core group).
pub fn detect(_root: Option<&std::path::Path>) -> bool {
    false
}

// ---------------------------------------------------------------- schemas

fn read_spec() -> ToolSpec {
    spec(
        "read",
        "Read a file (relative to the workspace, or absolute). Text comes back numbered `cat -n` \
         style (line number, tab, text): up to 2000 lines from `offset` (1-based, default 1), or \
         `limit` lines; a result that stops early says how to continue. Lines over 2000 characters \
         are cut. Images (png, jpg, gif, webp) come back as images, PDFs as text per page (`pages`, \
         e.g. \"1-5\"; at most 20 per call, required above 10 pages), Jupyter notebooks as cells \
         with their outputs (for a notebook, `offset` is a 0-based cell index and `limit` a \
         number of cells). Read a file before you edit or overwrite it, and never copy the \
         line-number prefix into an edit.",
        json!({
            "path": {"type": "string", "description": "File path, relative to the workspace or absolute."},
            "offset": {"type": "integer", "description": "1-based line to start from (for a notebook, a 0-based cell index)."},
            "limit": {"type": "integer", "description": "Number of lines (for a notebook, cells) to return."},
            "pages": {"type": "string", "description": "PDF pages, e.g. \"3\", \"1-5\" or \"1,4-6\"."}
        }),
        &["path"],
    )
}

fn write_spec() -> ToolSpec {
    spec(
        "write",
        "Create a file, or replace an existing one entirely with `content`; parent directories are \
         created. Overwriting an existing file needs a `read` of it first in this session; it \
         keeps its byte-order mark and CRLF line endings. Prefer `edit` for a change to part of a \
         file.",
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
        "Replace `old_string` with `new_string` in one file. `old_string` must occur exactly once \
         (copy it from `read` without the line-number prefix, with enough surrounding lines to be \
         unique), or set `replace_all` to change every occurrence. A match that differs only in \
         line endings or by one uniform indentation still applies when it is unique, and the \
         result says so. `read` the file first in this session. To create a file use `write`.",
        json!({
            "path": {"type": "string"},
            "old_string": {"type": "string", "description": "The exact text to replace."},
            "new_string": {"type": "string", "description": "The replacement text."},
            "replace_all": {"type": "boolean", "description": "Replace every occurrence (default false)."}
        }),
        &["path", "old_string", "new_string"],
    )
}

fn multi_edit_spec() -> ToolSpec {
    spec(
        "multi_edit",
        "Several `edit`s at once, in one or more files, applied all together or not at all. Edits \
         run in order, each seeing the result of the ones before it. `path` is the default file; \
         an edit may name its own. An empty `old_string` creates a file that does not exist. \
         `read` each existing file first.",
        json!({
            "path": {"type": "string", "description": "The file the edits apply to unless an edit names its own."},
            "edits": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "old_string": {"type": "string"},
                        "new_string": {"type": "string"},
                        "replace_all": {"type": "boolean"}
                    },
                    "required": ["old_string", "new_string"],
                    "additionalProperties": false
                }
            }
        }),
        &["edits"],
    )
}

fn edit_lines_spec() -> ToolSpec {
    spec(
        "edit_lines",
        "Edit lines by anchor. `read` shows each line as `N:hhh|text`; name a line by its anchor \
         `N:hhh` (and the last line of a range by `end`). `op`: replace (default), insert_after, \
         insert_before or delete. An anchor whose line changed since you read it is refused with \
         fresh anchors. `anchor` \"0\" with insert_after writes at the top of the file.",
        json!({
            "path": {"type": "string"},
            "anchor": {"type": "string", "description": "`N:hhh` as `read` printed it."},
            "end": {"type": "string", "description": "Anchor of the last line of the range (default: `anchor`)."},
            "op": {"type": "string", "enum": ["replace", "insert_after", "insert_before", "delete"]},
            "text": {"type": "string", "description": "The new lines."}
        }),
        &["path", "anchor"],
    )
}

fn notebook_edit_spec() -> ToolSpec {
    spec(
        "notebook_edit",
        "Edit one cell of a Jupyter notebook (.ipynb). `cell` is the cell's id or 0-based index as \
         `read` shows it. `mode`: replace (default) the cell's source, insert a new cell after \
         `cell` (at the top without one), or delete it. `cell_type` sets the new cell's type and \
         changes it on replace. Replacing a code cell clears its outputs. `read` the notebook first.",
        json!({
            "path": {"type": "string"},
            "cell": {"type": "string", "description": "Cell id, or 0-based index."},
            "new_source": {"type": "string", "description": "The cell's new source."},
            "cell_type": {"type": "string", "enum": ["code", "markdown"]},
            "mode": {"type": "string", "enum": ["replace", "insert", "delete"]}
        }),
        &["path"],
    )
}

#[cfg(test)]
mod tests;
