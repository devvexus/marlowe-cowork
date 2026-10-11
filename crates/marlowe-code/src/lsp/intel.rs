//! Code intelligence by *name* (design 2.1 T24-T27): the model says `Engine::start`, not
//! `src/engine.rs:40:12`.
//!
//! A name resolves through the tags index first (exact name; `Container::name` or
//! `module::name` narrows by container or file; a `path` prefers definitions in that file, and a
//! name only *used* in that file is resolved by the language server's go-to-definition from the
//! use). With no tag (no grammar for the language, or an aarch64 build) the server's
//! `workspace/symbol` answers. References, hover and rename then ask the server at the resolved
//! definition; without a server, references and hover fall back to the index and say so, and
//! rename refuses (a name-based rename is not safe).
//!
//! Rename turns the server's WorkspaceEdit into per-file byte edits, renders each file's change
//! as a unified diff, checks that replaying the diff onto the original reproduces the new text
//! exactly, then writes every file; a failed write puts back the ones already written, so the
//! rename lands whole or not at all. Edits outside the root (after `..` and links), file
//! create/rename/delete operations, and edits to a file whose disk text is not the copy the
//! server answered from, are refused before anything is written. Every request first brings the
//! server's open files up to the disk, since the shell or an editor may have changed them.
//!
//! Post-edit diagnostics: [`CodeIntel::before_edit`] opens the files on their server (starting
//! it only when the workspace has its project marker) and remembers what it last published;
//! [`CodeIntel::after_edit`] sends the new text, waits for a publish for that version (settle
//! 800 ms quiet, all files within 5 s; pulled where the server takes pulls) and returns the
//! errors that were not there before. Publishes that land later (rust-analyzer's `cargo check`)
//! are read by [`CodeIntel::latest`]; `marlowe-exec`'s diagnostics middleware diffs them against
//! what the model was last shown.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use marlowe_os::workspace::Workspace;
use serde_json::{json, Value};

use super::client::{Diag, LspClient};
use super::manager::LspManager;
use super::uri::{offset_of, path_to_uri, position_of, uri_to_path, Encoding};
use super::LspError;
use crate::index::{norm_key, Index, IndexError, Lang, Symbol};
use crate::repomap::{self, MapOptions};
use crate::udiff::{self, TextEdit};

/// Diagnostics settle: no new publish for this long.
pub const SETTLE: Duration = Duration::from_millis(800);
/// Longest wait for post-edit diagnostics, all files together.
pub const DIAG_MAX: Duration = Duration::from_secs(5);
/// Longest wait for a server to finish loading the workspace before asking it something.
pub const READY_MAX: Duration = Duration::from_secs(45);
/// One request's timeout.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// A tool call refreshes the index when the last refresh is older than this.
pub const INDEX_FRESH: Duration = Duration::from_secs(10);
/// Lines of a definition's body shown.
pub const BODY_LINES: usize = 60;
/// A server younger than this that answers "nothing" is asked again: servers without a
/// readiness signal answer before they have scanned the workspace (pyright's first `references`
/// comes back empty for about a second).
pub const WARMUP: Duration = Duration::from_secs(5);

/// Who is asking: whether the call may use a trusted server (the user's free-write workspace, in a
/// session no untrusted content has entered; see [`super::servers`]), whether it may use a server
/// at all, and its interrupt.
#[derive(Clone, Copy)]
pub struct Caller<'a> {
    pub cancelled: &'a dyn Fn() -> bool,
    pub trusted: bool,
    /// `false` for a session that took in untrusted content: it starts and uses no server (a
    /// server reads workspace config that can name programs to run, and such a session writes
    /// the workspace freely), so the tools answer from the index and `rename_symbol` refuses.
    pub servers: bool,
}

impl<'a> Caller<'a> {
    pub fn new(cancelled: &'a dyn Fn() -> bool, trusted: bool) -> Self {
        Self { cancelled, trusted, servers: true }
    }

    /// A caller from a tainted session: no language servers.
    pub fn tainted(cancelled: &'a dyn Fn() -> bool) -> Self {
        Self { cancelled, trusted: false, servers: false }
    }
}

/// `s` when it is shaped like a name (identifier characters and `::`, `.`, `<>`, `$`, `#`, at most
/// 80 bytes): what may be shown of a name that comes from a file outside the user's folders.
pub fn plain_name(s: &str) -> Option<&str> {
    let ok = !s.is_empty()
        && s.len() <= 80
        && s.chars().all(|c| c.is_alphanumeric() || matches!(c, '_' | ':' | '.' | '<' | '>' | '$' | '#'));
    ok.then_some(s)
}

/// The errors in `now` that are not in `known`, matched by (code, message) as a multiset, so
/// errors that only moved lines are not new.
pub fn new_errors(known: &[Diag], now: &[Diag]) -> Vec<Diag> {
    let mut known: Vec<(Option<&str>, &str)> =
        known.iter().filter(|d| d.severity == 1).map(|d| (d.code.as_deref(), d.message.as_str())).collect();
    let mut out = Vec::new();
    for d in now.iter().filter(|d| d.severity == 1) {
        match known.iter().position(|k| *k == (d.code.as_deref(), d.message.as_str())) {
            Some(i) => {
                known.swap_remove(i);
            }
            None => out.push(d.clone()),
        }
    }
    out
}

/// A request whose empty answer from a warming-up server is retried every 500 ms until the
/// server is [`WARMUP`] old. Servers with `serverStatus` were waited for already.
fn ask(client: &LspClient, method: &str, params: Value, cancelled: &dyn Fn() -> bool) -> std::result::Result<Value, LspError> {
    loop {
        let r = client.request(method, params.clone(), REQUEST_TIMEOUT, cancelled)?;
        let empty = r.is_null() || r.as_array().is_some_and(|a| a.is_empty());
        if !empty || client.spec().server_status || client.age() >= WARMUP || cancelled() {
            return Ok(r);
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// What failed, in words for the model.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IntelError {
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Ambiguous(String),
    #[error("{0}")]
    Refused(String),
    #[error(transparent)]
    Lsp(#[from] LspError),
    #[error("{0}")]
    Io(String),
}

impl From<IndexError> for IntelError {
    fn from(e: IndexError) -> Self {
        IntelError::Io(e.to_string())
    }
}

impl From<std::io::Error> for IntelError {
    fn from(e: std::io::Error) -> Self {
        IntelError::Io(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, IntelError>;

/// Where an answer came from (shown to the model, so it knows how much to trust it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The tags index (by name; same-named symbols are not told apart).
    Index,
    /// The language server.
    Server,
}

/// One resolved definition. Lines are 1-based; `col` is a 1-based byte column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub path: PathBuf,
    pub name: String,
    pub kind: String,
    pub container: Option<String>,
    pub line: u32,
    pub col: u32,
    pub end_line: u32,
    pub sig: String,
}

impl Target {
    fn from_symbol(s: Symbol) -> Self {
        Target {
            path: s.path,
            name: s.name,
            kind: s.kind,
            container: s.container,
            line: s.line,
            col: s.col,
            end_line: s.end_line.max(s.line),
            sig: s.sig,
        }
    }

    pub fn qualified(&self) -> String {
        match &self.container {
            Some(c) => format!("{c}::{}", self.name),
            None => self.name.clone(),
        }
    }

    /// [`Self::qualified`] with only name-shaped parts ([`plain_name`]): for a definition outside
    /// the user's folders, whose container a server reports from text there.
    pub fn plain_qualified(&self) -> String {
        let name = plain_name(&self.name).unwrap_or("(unnamed)");
        match self.container.as_deref().and_then(plain_name) {
            Some(c) => format!("{c}::{name}"),
            None => name.to_string(),
        }
    }

    /// The kind when it is name-shaped, else `symbol`.
    pub fn plain_kind(&self) -> &str {
        plain_name(&self.kind).unwrap_or("symbol")
    }
}

/// A source location with its line's text. `line` 1-based, `col` 1-based bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loc {
    pub path: PathBuf,
    pub line: u32,
    pub col: u32,
    pub text: String,
}

/// One file of a rename.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    pub path: PathBuf,
    pub old: String,
    pub new: String,
    pub edits: usize,
    /// Unified diff, root-relative labels.
    pub diff: String,
}

/// A rename, computed and checked, not yet written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenamePlan {
    pub target: Target,
    pub new_name: String,
    pub files: Vec<FileChange>,
}

impl RenamePlan {
    pub fn edits(&self) -> usize {
        self.files.iter().map(|f| f.edits).sum()
    }
}

/// What [`CodeIntel::before_edit`] saw: the diagnostics each file had before the edit, or the
/// generation at which it opened the file (its first publish is then awaited before the change).
#[derive(Debug, Clone, Default)]
pub struct EditSnapshot {
    before: HashMap<String, (Option<Vec<Diag>>, Option<u64>)>,
}

impl EditSnapshot {
    /// Files the snapshot covers (those with a server).
    pub fn len(&self) -> usize {
        self.before.len()
    }

    pub fn is_empty(&self) -> bool {
        self.before.is_empty()
    }

    /// Whether the snapshot covers `path`.
    pub fn covers(&self, path: &Path) -> bool {
        self.before.contains_key(&norm_key(path))
    }

    /// What `path` had before the edit, as far as its server had said.
    pub fn known(&self, path: &Path) -> Vec<Diag> {
        self.before.get(&norm_key(path)).and_then(|e| e.0.clone()).unwrap_or_default()
    }

    /// Diffs `path` against `known` instead of what its server last published: what the model
    /// was last shown, so a publish that landed unseen since then (a late `cargo check`) is new.
    pub fn assume_known(&mut self, path: &Path, known: Vec<Diag>) {
        if let Some(entry) = self.before.get_mut(&norm_key(path)) {
            entry.0 = Some(known);
        }
    }
}

/// Diagnostics for one file after an edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiags {
    pub path: PathBuf,
    /// Errors that were not there before the edit (all errors when nothing was known before).
    pub new_errors: Vec<Diag>,
    /// Every error the file has now.
    pub errors: usize,
    pub warnings: usize,
    /// The server's line text for each new error, for display.
    pub lines: Vec<String>,
    /// Columns are in the server's units; this says which.
    pub encoding: Encoding,
    /// Everything the file has now, and the publish it came from (server instance, generation).
    pub now: Vec<Diag>,
    pub instance: u64,
    pub gen: u64,
}

impl FileDiags {
    /// `now` diffed against `known`, with the error lines read from `text`.
    pub fn diff(path: &Path, known: &[Diag], now: Vec<Diag>, text: &str, encoding: Encoding, instance: u64, gen: u64) -> Self {
        let new_errors = new_errors(known, &now);
        let lines = new_errors.iter().map(|d| text.lines().nth(d.line as usize).unwrap_or("").trim().to_string()).collect();
        FileDiags {
            path: path.to_path_buf(),
            errors: now.iter().filter(|d| d.severity == 1).count(),
            warnings: now.iter().filter(|d| d.severity == 2).count(),
            new_errors,
            lines,
            encoding,
            now,
            instance,
            gen,
        }
    }
}

/// A file's latest publish on its running server, read without waiting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Latest {
    pub instance: u64,
    pub gen: u64,
    pub items: Vec<Diag>,
    pub encoding: Encoding,
}

/// The index and the servers of one workspace root.
pub struct CodeIntel {
    root: PathBuf,
    index: Index,
    lsp: LspManager,
    refreshing: Mutex<()>,
}

impl std::fmt::Debug for CodeIntel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodeIntel").field("root", &self.root).field("lsp", &self.lsp).finish()
    }
}

/// `Container::name` / `module.name` / `name()` split into (container, name).
pub fn split_symbol(symbol: &str) -> (Option<String>, String) {
    let s = symbol.trim().trim_end_matches("()").trim();
    let s = s.trim_start_matches("::");
    let cut = s.rfind("::").map(|i| (i, 2)).or_else(|| s.rfind('.').map(|i| (i, 1))).or_else(|| s.rfind('#').map(|i| (i, 1)));
    match cut {
        Some((i, w)) if i > 0 && i + w < s.len() => {
            let container = &s[..i];
            let container = container.rsplit(['.', ':']).find(|p| !p.is_empty()).unwrap_or(container);
            (Some(container.to_string()), s[i + w..].to_string())
        }
        _ => (None, s.to_string()),
    }
}

/// Whether `t` is in `container`: its container is it, or its file (or a `mod.rs`/`__init__.py`
/// folder) is named it.
fn in_container(t: &Target, container: &str) -> bool {
    if t.container.as_deref().is_some_and(|c| c == container || c.rsplit("::").next() == Some(container)) {
        return true;
    }
    let stem = t.path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    if stem == container {
        return true;
    }
    matches!(stem, "mod" | "__init__" | "index" | "lib")
        && t.path.parent().and_then(|p| p.file_name()).and_then(|n| n.to_str()) == Some(container)
}

fn is_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(|c| c.is_alphabetic() || c == '_' || c == '$')
        && chars.all(|c| c.is_alphanumeric() || c == '_' || c == '$')
}

/// The text of a hover result.
fn hover_text(v: &Value) -> String {
    fn one(v: &Value) -> String {
        match v {
            Value::String(s) => s.clone(),
            Value::Object(o) => {
                let value = o.get("value").and_then(Value::as_str).unwrap_or("");
                match o.get("language").and_then(Value::as_str) {
                    Some(lang) => format!("```{lang}\n{value}\n```"),
                    None => value.to_string(),
                }
            }
            Value::Array(a) => a.iter().map(one).filter(|s| !s.trim().is_empty()).collect::<Vec<_>>().join("\n\n"),
            _ => String::new(),
        }
    }
    v.get("contents").map(one).unwrap_or_default().trim().to_string()
}

/// (path, start, end) of an LSP `Location` or `LocationLink`.
fn location_of(v: &Value) -> Option<(PathBuf, Value)> {
    let uri = v.get("uri").or_else(|| v.get("targetUri"))?.as_str()?;
    let range = v.get("targetSelectionRange").or_else(|| v.get("range")).or_else(|| v.get("targetRange"))?;
    Some((uri_to_path(uri)?, range.clone()))
}

impl CodeIntel {
    pub fn new(root: &Path, index: Index, lsp: LspManager) -> Self {
        Self { root: root.to_path_buf(), index, lsp, refreshing: Mutex::new(()) }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn index(&self) -> &Index {
        &self.index
    }

    pub fn lsp(&self) -> &LspManager {
        &self.lsp
    }

    /// Refreshes the index when it is older than [`INDEX_FRESH`] (one refresh at a time).
    pub fn ensure_index(&self, ws: &dyn Workspace) -> Result<()> {
        if self.index.fresh_within(INDEX_FRESH) {
            return Ok(());
        }
        let _one = self.refreshing.lock().unwrap_or_else(|p| p.into_inner());
        if !self.index.fresh_within(INDEX_FRESH) {
            self.index.refresh(ws)?;
        }
        Ok(())
    }

    /// Whether `p` is under the root, with `..` applied and links resolved.
    fn inside(&self, p: &Path) -> bool {
        super::uri::contained(&self.root, p)
    }

    /// `p` relative to the root, `/`-separated (absolute when outside).
    pub fn rel(&self, p: &Path) -> String {
        self.index.rel(p).unwrap_or_else(|| p.display().to_string())
    }

    fn read_text(ws: &dyn Workspace, p: &Path) -> Result<String> {
        let bytes = ws.read(p)?;
        String::from_utf8(bytes).map_err(|_| IntelError::Io(format!("{} is not UTF-8 text", p.display())))
    }

    /// Byte offset of a target's name in `text`.
    fn target_offset(text: &str, t: &Target) -> usize {
        let starts = super::uri::line_starts(text);
        let start = starts.get(t.line.saturating_sub(1) as usize).copied().unwrap_or(text.len());
        (start + t.col.saturating_sub(1) as usize).min(text.len())
    }

    /// The server for `path` (started if needed, waited for), with every file it has open brought
    /// up to the disk: once a file is open the server goes by Marlowe's copy, so a change made
    /// since by the shell (`cargo fmt`, `git checkout`) or an editor would leave its answers
    /// pointing at old positions. Returns whether it was ready.
    fn connect(&self, ws: &dyn Workspace, path: &Path, caller: &Caller) -> Result<(LspClient, bool)> {
        if !caller.servers {
            // A trusted server running for the root is stopped too: the tainted session may have
            // written what it would run.
            self.lsp.distrust(&self.root);
            return Err(IntelError::Lsp(LspError::Withheld));
        }
        let client = self.lsp.client(path, &self.root, caller.trusted)?;
        let ready = client.wait_ready(READY_MAX, caller.cancelled);
        self.resync(ws, &client);
        Ok((client, ready))
    }

    /// Sends the disk's text for every open file whose text differs from the server's copy.
    fn resync(&self, ws: &dyn Workspace, client: &LspClient) {
        for p in client.open_docs() {
            if let Ok(text) = Self::read_text(ws, &p) {
                client.sync(&p, &text);
            }
        }
    }

    /// [`Self::connect`] with `path` itself opened and synced; its text.
    fn server_at(&self, ws: &dyn Workspace, path: &Path, caller: &Caller) -> Result<(LspClient, String, bool)> {
        let (client, ready) = self.connect(ws, path, caller)?;
        let text = Self::read_text(ws, path)?;
        client.sync(path, &text);
        Ok((client, text, ready))
    }

    fn position(client: &LspClient, path: &Path, text: &str, offset: usize) -> Value {
        let (line, character) = position_of(text, offset, client.encoding());
        json!({"textDocument": {"uri": path_to_uri(path)}, "position": {"line": line, "character": character}})
    }

    /// A server location as a [`Target`]-shaped (path, line, col) with the line's text.
    fn loc(&self, ws: &dyn Workspace, cache: &mut HashMap<String, String>, path: &Path, range: &Value, enc: Encoding) -> Option<Loc> {
        let key = norm_key(path);
        if !cache.contains_key(&key) {
            cache.insert(key.clone(), Self::read_text(ws, path).ok()?);
        }
        let text = &cache[&key];
        let start = range.get("start")?;
        let line = start.get("line")?.as_u64()? as u32;
        let off = offset_of(text, line, start.get("character")?.as_u64()? as u32, enc)?;
        let starts = super::uri::line_starts(text);
        let ls = starts[line as usize];
        let le = starts.get(line as usize + 1).map_or(text.len(), |&n| n - 1);
        Some(Loc {
            path: path.to_path_buf(),
            line: line + 1,
            col: (off - ls) as u32 + 1,
            text: text[ls..le].trim_end().to_string(),
        })
    }

    /// Definitions named `symbol`, best first. See the module doc for the rules.
    pub fn resolve(
        &self,
        ws: &dyn Workspace,
        symbol: &str,
        path: Option<&Path>,
        kind: Option<&str>,
        caller: &Caller,
    ) -> Result<(Vec<Target>, Source)> {
        let (container, name) = split_symbol(symbol);
        if name.is_empty() {
            return Err(IntelError::NotFound("give a symbol name".into()));
        }
        let _ = self.ensure_index(ws);
        let mut found: Vec<Target> = self.index.defs(&name).into_iter().map(Target::from_symbol).collect();
        if let Some(c) = &container {
            found.retain(|t| in_container(t, c));
        }
        if let Some(k) = kind {
            found.retain(|t| t.kind.eq_ignore_ascii_case(k));
        }
        if let Some(p) = path {
            let key = norm_key(p);
            let here: Vec<Target> = found.iter().filter(|t| norm_key(&t.path) == key).cloned().collect();
            if !here.is_empty() {
                return Ok((here, Source::Index));
            }
            // Used in that file but defined elsewhere: ask the server from the use.
            if let Ok(t) = self.definition_from_use(ws, p, &name, caller) {
                if !t.is_empty() {
                    return Ok((t, Source::Server));
                }
            }
        }
        if !found.is_empty() {
            return Ok((found, Source::Index));
        }
        // No tag: the servers' workspace symbols.
        let t = self.workspace_symbols(ws, &name, container.as_deref(), path, caller)?;
        if t.is_empty() {
            let hint = match path {
                Some(p) => format!(" (looked in {} and the whole workspace)", self.rel(p)),
                None => String::new(),
            };
            return Err(IntelError::NotFound(format!("no definition of `{symbol}` found{hint}")));
        }
        Ok((t, Source::Server))
    }

    /// Go-to-definition from the first whole-word use of `name` in `path`.
    fn definition_from_use(&self, ws: &dyn Workspace, path: &Path, name: &str, caller: &Caller) -> Result<Vec<Target>> {
        let text = Self::read_text(ws, path)?;
        let Some(off) = first_word(&text, name) else { return Ok(Vec::new()) };
        let (client, text, _) = self.server_at(ws, path, caller)?;
        let r = ask(&client, "textDocument/definition", Self::position(&client, path, &text, off), caller.cancelled)?;
        let locs: Vec<Value> = match r {
            Value::Array(a) => a,
            Value::Null => Vec::new(),
            one => vec![one],
        };
        let mut cache = HashMap::new();
        let mut out = Vec::new();
        for v in locs {
            let Some((p, range)) = location_of(&v) else { continue };
            let Some(l) = self.loc(ws, &mut cache, &p, &range, client.encoding()) else { continue };
            out.push(self.target_at(l, name));
        }
        Ok(out)
    }

    /// A target at a server location, filled from the index when it has the tag.
    fn target_at(&self, l: Loc, name: &str) -> Target {
        let tagged = self.index.file_tags(&l.path, true).into_iter().find(|s| s.name == name && s.line == l.line);
        match tagged {
            Some(s) => Target::from_symbol(s),
            None => Target {
                path: l.path,
                name: name.to_string(),
                kind: String::new(),
                container: None,
                line: l.line,
                col: l.col,
                end_line: l.line,
                sig: l.text.trim().to_string(),
            },
        }
    }

    /// `workspace/symbol` on each server that serves a language present in the index (or the
    /// language of `path`), exact name matches.
    fn workspace_symbols(
        &self,
        ws: &dyn Workspace,
        name: &str,
        container: Option<&str>,
        path: Option<&Path>,
        caller: &Caller,
    ) -> Result<Vec<Target>> {
        let probe: Vec<PathBuf> = match path {
            Some(p) => vec![p.to_path_buf()],
            None => self.sample_files(),
        };
        let mut out = Vec::new();
        let mut asked = Vec::new();
        for file in probe {
            let Some(spec) = Lang::of(&file).and_then(|l| self.lsp.spec_for(l, &self.root, caller.trusted)) else { continue };
            if asked.contains(&spec.id) {
                continue;
            }
            asked.push(spec.id);
            let Ok((client, _)) = self.connect(ws, &file, caller) else { continue };
            let Ok(r) = ask(&client, "workspace/symbol", json!({"query": name}), caller.cancelled) else { continue };
            let mut cache = HashMap::new();
            for s in r.as_array().into_iter().flatten() {
                if s.get("name").and_then(Value::as_str) != Some(name) {
                    continue;
                }
                let cname = s.get("containerName").and_then(Value::as_str).filter(|c| !c.is_empty());
                let Some((p, range)) = s.get("location").and_then(location_of) else { continue };
                let Some(l) = self.loc(ws, &mut cache, &p, &range, client.encoding()) else { continue };
                let mut t = self.target_at(l, name);
                if t.container.is_none() {
                    t.container = cname.map(str::to_string);
                }
                if t.kind.is_empty() {
                    t.kind = symbol_kind(s.get("kind").and_then(Value::as_u64).unwrap_or(0)).to_string();
                }
                if container.is_none_or(|c| in_container(&t, c)) {
                    out.push(t);
                }
            }
        }
        Ok(out)
    }

    /// One file per language present in the index (to pick which servers to ask).
    fn sample_files(&self) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = Vec::new();
        let mut langs = Vec::new();
        for s in self.index.search("", None, 2000) {
            if let Some(l) = Lang::of(&s.path) {
                if !langs.contains(&l) {
                    langs.push(l);
                    out.push(s.path);
                }
            }
        }
        out
    }

    /// Definitions whose name contains `query` (index), or a file's own symbols when `path` is a
    /// file. Falls back to the servers when the index has nothing.
    pub fn symbols(
        &self,
        ws: &dyn Workspace,
        query: &str,
        path: Option<&Path>,
        kind: Option<&str>,
        limit: usize,
        caller: &Caller,
    ) -> Result<(Vec<Target>, Source)> {
        self.ensure_index(ws)?;
        if let Some(p) = path.filter(|p| ws.metadata(p).is_ok_and(|m| m.is_file)) {
            let mut tags: Vec<Target> = self.index.file_tags(p, true).into_iter().map(Target::from_symbol).collect();
            if !query.is_empty() {
                let q = query.to_lowercase();
                tags.retain(|t| t.name.to_lowercase().contains(&q));
            }
            if let Some(k) = kind {
                tags.retain(|t| t.kind.eq_ignore_ascii_case(k));
            }
            if !tags.is_empty() || crate::index::tags::supported() && Lang::of(p).is_some() {
                tags.truncate(limit);
                return Ok((tags, Source::Index));
            }
            return self.document_symbols(ws, p, query, limit, caller).map(|t| (t, Source::Server));
        }
        // The folder is part of the index query, so the limit counts only its definitions.
        let hits: Vec<Target> = self.index.search_in(query, kind, path, limit).into_iter().map(Target::from_symbol).collect();
        if !hits.is_empty() || query.is_empty() {
            return Ok((hits, Source::Index));
        }
        let (container, name) = split_symbol(query);
        let mut t = self.workspace_symbols(ws, &name, container.as_deref(), None, caller)?;
        if let Some(dir) = path {
            t.retain(|t| super::uri::contained(dir, &t.path));
        }
        t.truncate(limit);
        Ok((t, Source::Server))
    }

    fn document_symbols(&self, ws: &dyn Workspace, path: &Path, query: &str, limit: usize, caller: &Caller) -> Result<Vec<Target>> {
        let (client, text, _) = self.server_at(ws, path, caller)?;
        let r = client.request(
            "textDocument/documentSymbol",
            json!({"textDocument": {"uri": path_to_uri(path)}}),
            REQUEST_TIMEOUT,
            caller.cancelled,
        )?;
        let enc = client.encoding();
        let mut out = Vec::new();
        fn walk(v: &Value, container: Option<&str>, text: &str, path: &Path, enc: Encoding, out: &mut Vec<Target>) {
            for s in v.as_array().into_iter().flatten() {
                let name = s.get("name").and_then(Value::as_str).unwrap_or("").to_string();
                let range = s.get("selectionRange").or_else(|| s.pointer("/location/range"));
                let full = s.get("range").or_else(|| s.pointer("/location/range"));
                let (Some(range), Some(full)) = (range, full) else { continue };
                let line = range.pointer("/start/line").and_then(Value::as_u64).unwrap_or(0) as u32;
                let ch = range.pointer("/start/character").and_then(Value::as_u64).unwrap_or(0) as u32;
                let end = full.pointer("/end/line").and_then(Value::as_u64).unwrap_or(line as u64) as u32;
                let off = offset_of(text, line, ch, enc).unwrap_or(0);
                let starts = super::uri::line_starts(text);
                let ls = starts.get(line as usize).copied().unwrap_or(0);
                let le = starts.get(line as usize + 1).map_or(text.len(), |&n| n - 1);
                let cname = s.get("containerName").and_then(Value::as_str).or(container);
                out.push(Target {
                    path: path.to_path_buf(),
                    name: name.clone(),
                    kind: symbol_kind(s.get("kind").and_then(Value::as_u64).unwrap_or(0)).to_string(),
                    container: cname.map(str::to_string),
                    line: line + 1,
                    col: (off.saturating_sub(ls)) as u32 + 1,
                    end_line: end + 1,
                    sig: text.get(ls..le).unwrap_or("").trim().to_string(),
                });
                if let Some(children) = s.get("children") {
                    walk(children, Some(&name), text, path, enc, out);
                }
            }
        }
        walk(&r, None, &text, path, enc, &mut out);
        if !query.is_empty() {
            let q = query.to_lowercase();
            out.retain(|t| t.name.to_lowercase().contains(&q));
        }
        out.truncate(limit);
        Ok(out)
    }

    /// The body of a definition: its lines, at most [`BODY_LINES`].
    pub fn body(&self, ws: &dyn Workspace, t: &Target) -> Vec<(u32, String)> {
        let Ok(text) = Self::read_text(ws, &t.path) else { return Vec::new() };
        let last = t.end_line.max(t.line);
        text.lines()
            .enumerate()
            .skip(t.line.saturating_sub(1) as usize)
            .take(((last - t.line) as usize + 1).min(BODY_LINES))
            .map(|(i, l)| (i as u32 + 1, l.to_string()))
            .collect()
    }

    /// References to the one definition `symbol` resolves to (server; index by name without one).
    /// Ambiguous names list the candidates.
    pub fn references(
        &self,
        ws: &dyn Workspace,
        symbol: &str,
        path: Option<&Path>,
        limit: usize,
        caller: &Caller,
    ) -> Result<(Target, Vec<Loc>, Source)> {
        let target = self.one(ws, symbol, path, caller)?;
        match self.server_at(ws, &target.path, caller) {
            Ok((client, text, _)) if client.has("referencesProvider") => {
                let mut params = Self::position(&client, &target.path, &text, Self::target_offset(&text, &target));
                params["context"] = json!({"includeDeclaration": false});
                let r = ask(&client, "textDocument/references", params, caller.cancelled)?;
                let mut cache = HashMap::new();
                let mut locs: Vec<Loc> = r
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(location_of)
                    .filter_map(|(p, range)| self.loc(ws, &mut cache, &p, &range, client.encoding()))
                    .collect();
                locs.sort_by_key(|l| (norm_key(&l.path), l.line, l.col));
                locs.dedup();
                locs.truncate(limit);
                Ok((target, locs, Source::Server))
            }
            Err(IntelError::Lsp(LspError::Cancelled)) => Err(IntelError::Lsp(LspError::Cancelled)),
            _ => {
                let mut cache = HashMap::new();
                let locs: Vec<Loc> = self
                    .index
                    .refs(&target.name, limit)
                    .into_iter()
                    .map(|s| {
                        let key = norm_key(&s.path);
                        if !cache.contains_key(&key) {
                            cache.insert(key.clone(), Self::read_text(ws, &s.path).unwrap_or_default());
                        }
                        let line = cache[&key].lines().nth(s.line as usize - 1).unwrap_or("").trim_end().to_string();
                        Loc { path: s.path, line: s.line, col: s.col, text: line }
                    })
                    .collect();
                Ok((target, locs, Source::Index))
            }
        }
    }

    /// The one definition `symbol` names, or an error listing the candidates.
    pub fn one(&self, ws: &dyn Workspace, symbol: &str, path: Option<&Path>, caller: &Caller) -> Result<Target> {
        let (mut found, _) = self.resolve(ws, symbol, path, None, caller)?;
        found.dedup_by(|a, b| norm_key(&a.path) == norm_key(&b.path) && a.line == b.line);
        if found.len() == 1 {
            return Ok(found.remove(0));
        }
        let list: Vec<String> = found
            .iter()
            .take(12)
            .map(|t| {
                // Outside the root a server's container text is shown only when it is a name.
                let (name, kind) = if super::uri::contained(&self.root, &t.path) {
                    (t.qualified(), if t.kind.is_empty() { "symbol" } else { t.kind.as_str() })
                } else {
                    (t.plain_qualified(), t.plain_kind())
                };
                format!("  {}:{}  {name}  ({kind})", self.rel(&t.path), t.line)
            })
            .collect();
        let more = if found.len() > 12 { format!("\n  ... and {} more", found.len() - 12) } else { String::new() };
        Err(IntelError::Ambiguous(format!(
            "`{symbol}` names {} definitions; say which with `Container::name` or `path`:\n{}{more}",
            found.len(),
            list.join("\n")
        )))
    }

    /// Hover text for the one definition `symbol` names (the server's; the signature without one).
    pub fn hover(&self, ws: &dyn Workspace, symbol: &str, path: Option<&Path>, caller: &Caller) -> Result<(Target, String, Source)> {
        let target = self.one(ws, symbol, path, caller)?;
        if let Ok((client, text, _)) = self.server_at(ws, &target.path, caller) {
            if client.has("hoverProvider") {
                let params = Self::position(&client, &target.path, &text, Self::target_offset(&text, &target));
                if let Ok(r) = client.request("textDocument/hover", params, REQUEST_TIMEOUT, caller.cancelled) {
                    let h = hover_text(&r);
                    if !h.is_empty() {
                        return Ok((target, h, Source::Server));
                    }
                }
            }
        }
        let text = format!("```\n{}\n```", target.sig);
        Ok((target, text, Source::Index))
    }

    /// Diagnostics for `path` (opened on its server, settled), or every file the running servers
    /// have published for.
    pub fn diagnostics(&self, ws: &dyn Workspace, path: Option<&Path>, caller: &Caller) -> Result<Vec<(PathBuf, Vec<Diag>, Encoding)>> {
        match path {
            Some(p) => {
                let (client, _) = self.connect(ws, p, caller)?;
                let text = Self::read_text(ws, p)?;
                let (version, gen) = client.sync(p, &text);
                let diags = client
                    .pull_diagnostics(p, REQUEST_TIMEOUT, caller.cancelled)
                    .or_else(|| client.wait_diagnostics(p, gen, version, SETTLE, DIAG_MAX, caller.cancelled))
                    .or_else(|| client.diagnostics(p))
                    .unwrap_or_default();
                Ok(vec![(p.to_path_buf(), diags, client.encoding())])
            }
            None => {
                // Only this root's servers, and only files under it: the manager is shared by
                // every session and workspace of the profile home.
                let live = self.lsp.live_under(&self.root);
                if live.is_empty() {
                    return Err(IntelError::NotFound(
                        "no language server is running; give `path` to check a file (that starts its server)".into(),
                    ));
                }
                let mut out = Vec::new();
                for c in live {
                    for (p, d) in c.all_diagnostics() {
                        if super::uri::contained(&self.root, &p) {
                            out.push((p, d, c.encoding()));
                        }
                    }
                }
                out.sort_by(|a, b| a.0.cmp(&b.0));
                Ok(out)
            }
        }
    }

    /// Plans a rename of the one definition `symbol` names to `new_name` through its server.
    pub fn plan_rename(
        &self,
        ws: &dyn Workspace,
        symbol: &str,
        new_name: &str,
        path: Option<&Path>,
        caller: &Caller,
    ) -> Result<RenamePlan> {
        let new_name = new_name.trim();
        if !is_identifier(new_name) {
            return Err(IntelError::Refused(format!("`{new_name}` is not an identifier")));
        }
        let target = self.one(ws, symbol, path, caller)?;
        if target.name == new_name {
            return Err(IntelError::Refused(format!("`{symbol}` is already called `{new_name}`")));
        }
        let (client, text, ready) = self.server_at(ws, &target.path, caller).map_err(|e| match e {
            IntelError::Lsp(l) => IntelError::Refused(format!(
                "rename_symbol needs a language server for {} ({l}); a rename by name alone is not safe",
                self.rel(&target.path)
            )),
            other => other,
        })?;
        if !client.has("renameProvider") {
            return Err(IntelError::Refused(format!("{} cannot rename", client.id())));
        }
        let pos = Self::position(&client, &target.path, &text, Self::target_offset(&text, &target));
        let prepare = client.capabilities().pointer("/renameProvider/prepareProvider").and_then(Value::as_bool) == Some(true);
        if prepare {
            let r = client.request("textDocument/prepareRename", pos.clone(), REQUEST_TIMEOUT, caller.cancelled)?;
            if r.is_null() {
                return Err(IntelError::Refused(format!("{} says `{}` cannot be renamed here", client.id(), target.qualified())));
            }
        }
        let mut params = pos;
        params["newName"] = json!(new_name);
        let edit = client.request("textDocument/rename", params, REQUEST_TIMEOUT, caller.cancelled)?;
        if edit.is_null() {
            let why = if ready { "" } else { " (it was still loading the workspace)" };
            return Err(IntelError::Refused(format!("{} returned no edits for `{}`{why}", client.id(), target.qualified())));
        }
        let files = self.changes(ws, &edit, &client)?;
        if files.is_empty() {
            return Err(IntelError::Refused(format!("{} returned no edits for `{}`", client.id(), target.qualified())));
        }
        Ok(RenamePlan { target, new_name: new_name.to_string(), files })
    }

    /// A WorkspaceEdit as checked per-file changes. The server's positions are only good for the
    /// text it saw: a file it has open must hold, on disk, exactly the copy the server has (and
    /// the version the edit names, when it names one), or the rename is refused.
    fn changes(&self, ws: &dyn Workspace, edit: &Value, client: &LspClient) -> Result<Vec<FileChange>> {
        let enc = client.encoding();
        // (path, edits, the document version the server says it edited)
        let mut per_file: Vec<(PathBuf, Vec<Value>, Option<i64>)> = Vec::new();
        let mut push = |uri: &str, edits: Vec<Value>, version: Option<i64>| -> Result<()> {
            let raw = uri_to_path(uri).ok_or_else(|| IntelError::Refused(format!("the server edits a non-file URI {uri}")))?;
            // `..` applied, so the path checked is the path written.
            let path = super::uri::normalize(&raw).ok_or_else(|| {
                IntelError::Refused(format!("the rename would edit {} outside the workspace; nothing was changed", raw.display()))
            })?;
            match per_file.iter_mut().find(|(p, ..)| norm_key(p) == norm_key(&path)) {
                Some((_, e, v)) => {
                    e.extend(edits);
                    *v = v.or(version);
                }
                None => per_file.push((path, edits, version)),
            }
            Ok(())
        };
        if let Some(dc) = edit.get("documentChanges").and_then(Value::as_array) {
            for c in dc {
                if let Some(kind) = c.get("kind").and_then(Value::as_str) {
                    let what = c.get("uri").or_else(|| c.get("oldUri")).and_then(Value::as_str).unwrap_or("");
                    return Err(IntelError::Refused(format!(
                        "the rename would also {kind} a file ({what}); Marlowe applies text edits only. Rename the file with the shell, then rename the symbol"
                    )));
                }
                let uri = c.pointer("/textDocument/uri").and_then(Value::as_str).unwrap_or("");
                let version = c.pointer("/textDocument/version").and_then(Value::as_i64);
                push(uri, c.get("edits").and_then(Value::as_array).cloned().unwrap_or_default(), version)?;
            }
        } else if let Some(ch) = edit.get("changes").and_then(Value::as_object) {
            for (uri, edits) in ch {
                push(uri, edits.as_array().cloned().unwrap_or_default(), None)?;
            }
        }
        let mut out = Vec::new();
        for (path, edits, version) in per_file {
            if !self.inside(&path) {
                return Err(IntelError::Refused(format!(
                    "the rename would edit {} outside the workspace; nothing was changed",
                    path.display()
                )));
            }
            let old = Self::read_text(ws, &path)?;
            let stale = || {
                IntelError::Refused(format!(
                    "{} changed since the language server saw it; nothing was changed. Try the rename again",
                    self.rel(&path)
                ))
            };
            let doc = client.doc(&path);
            if doc.is_some_and(|(_, hash)| hash != blake3::hash(old.as_bytes())) {
                return Err(stale());
            }
            if version.is_some() && doc.map(|d| d.0) != version {
                return Err(stale());
            }
            let mut byte_edits = Vec::new();
            for e in &edits {
                let r = e.get("range").ok_or_else(|| IntelError::Refused("an edit without a range".into()))?;
                let at = |k: &str| -> Option<usize> {
                    let p = r.get(k)?;
                    offset_of(&old, p.get("line")?.as_u64()? as u32, p.get("character")?.as_u64()? as u32, enc)
                };
                let (Some(start), Some(end)) = (at("start"), at("end")) else {
                    return Err(IntelError::Refused(format!("an edit in {} is outside the file", self.rel(&path))));
                };
                let new_text = e.get("newText").and_then(Value::as_str).unwrap_or("").to_string();
                byte_edits.push(TextEdit { start, end, new_text });
            }
            let new = udiff::apply_edits(&old, &byte_edits).map_err(|e| IntelError::Refused(format!("{}: {e}", self.rel(&path))))?;
            if new == old {
                continue;
            }
            let diff = udiff::unified(&self.rel(&path), &old, &new, 3);
            let replayed = udiff::parse(&diff)
                .ok()
                .and_then(|p| p.into_iter().next())
                .and_then(|p| udiff::apply(&old, &p).ok());
            if replayed.as_deref() != Some(new.as_str()) {
                return Err(IntelError::Refused(format!("the diff for {} does not replay; nothing was changed", self.rel(&path))));
            }
            out.push(FileChange { path, old, new, edits: byte_edits.len(), diff });
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    /// Writes a plan, all files or none: each file must still hold the text the plan was made
    /// from; a failed write restores the files already written. Then the servers and the index
    /// see the new text.
    pub fn apply_rename(&self, ws: &dyn Workspace, plan: &RenamePlan) -> Result<()> {
        for f in &plan.files {
            let now = Self::read_text(ws, &f.path)?;
            if now != f.old {
                return Err(IntelError::Refused(format!("{} changed since the rename was planned; nothing was changed", self.rel(&f.path))));
            }
        }
        let mut written: Vec<&FileChange> = Vec::new();
        for f in &plan.files {
            if let Err(e) = ws.write(&f.path, f.new.as_bytes()) {
                for w in written.iter().rev() {
                    let _ = ws.write(&w.path, w.old.as_bytes());
                }
                return Err(IntelError::Io(format!("writing {} failed ({e}); every file was put back", self.rel(&f.path))));
            }
            written.push(f);
        }
        for f in &plan.files {
            let _ = self.index.update_file(ws, &f.path);
            if let Some(c) = self.lsp.running(&f.path, &self.root) {
                c.sync(&f.path, &f.new);
            }
        }
        Ok(())
    }

    /// The server for an edited file ([`LspManager::for_edit`]); none for a tainted caller.
    fn for_edit(&self, p: &Path, caller: &Caller) -> Option<LspClient> {
        if !caller.servers {
            self.lsp.distrust(&self.root);
            return None;
        }
        self.lsp.for_edit(p, &self.root, caller.trusted)
    }

    /// Before an edit to `paths`: open each on its server (starting it when the workspace has its
    /// project marker) and remember its current diagnostics.
    pub fn before_edit(&self, ws: &dyn Workspace, paths: &[PathBuf], caller: &Caller) -> EditSnapshot {
        let mut snap = EditSnapshot::default();
        for p in paths {
            let Some(client) = self.for_edit(p, caller) else { continue };
            self.resync(ws, &client);
            let mut opened = None;
            if !client.is_open(p) {
                if let Ok(text) = Self::read_text(ws, p) {
                    opened = Some(client.sync(p, &text).1);
                }
            }
            let known = if client.pulls_diagnostics() {
                client.pull_diagnostics(p, Duration::from_secs(2), caller.cancelled)
            } else {
                client.diagnostics(p)
            };
            snap.before.insert(norm_key(p), (known, opened));
        }
        snap
    }

    /// After an edit to `paths`: re-tags them, sends their servers the new text and returns the
    /// new errors per file (files with a server only). All files share one [`DIAG_MAX`] wait.
    pub fn after_edit(&self, ws: &dyn Workspace, paths: &[PathBuf], snap: &EditSnapshot, caller: &Caller) -> Vec<FileDiags> {
        for p in paths {
            let _ = self.index.update_file(ws, p);
        }
        let deadline = Instant::now() + DIAG_MAX;
        let mut pending = Vec::new();
        for p in paths {
            let Some(client) = self.for_edit(p, caller) else { continue };
            let Ok(text) = Self::read_text(ws, p) else { continue };
            let (mut before, opened) = snap.before.get(&norm_key(p)).cloned().unwrap_or((None, None));
            if before.is_none() {
                // Opened just before the edit: its first publish (the pre-edit state) may still be
                // on its way. Wait a little for it, so old errors are not reported as new.
                before = match opened {
                    Some(gen) => {
                        let left = deadline.saturating_duration_since(Instant::now()).min(Duration::from_millis(1500));
                        client.wait_diagnostics(p, gen, 1, Duration::from_millis(100), left, caller.cancelled)
                    }
                    None => client.diagnostics(p),
                };
            }
            let (version, gen) = client.sync(p, &text);
            pending.push((p.clone(), client, text, version, gen, before));
        }
        let mut out = Vec::new();
        for (p, client, text, version, gen, before) in pending {
            let left = deadline.saturating_duration_since(Instant::now());
            let now = client
                .pull_published(&p, left.max(Duration::from_millis(500)), caller.cancelled)
                .or_else(|| client.wait_published(&p, gen, version, SETTLE, left, caller.cancelled));
            let Some((gen, now)) = now else { continue };
            let known = before.unwrap_or_default();
            out.push(FileDiags::diff(&p, &known, now, &text, client.encoding(), client.instance(), gen));
        }
        out
    }

    /// The latest publish for `path` on its running server, without waiting or starting one.
    pub fn latest(&self, path: &Path) -> Option<Latest> {
        let client = self.lsp.running(path, &self.root)?;
        let (gen, items) = client.published(path)?;
        Some(Latest { instance: client.instance(), gen, items, encoding: client.encoding() })
    }

    /// `p`'s text, for the lines of late diagnostics.
    pub fn text_of(&self, ws: &dyn Workspace, p: &Path) -> String {
        Self::read_text(ws, p).unwrap_or_default()
    }

    /// The repo map (design 3.4), refreshed first.
    pub fn repo_map(&self, ws: &dyn Workspace, opts: &MapOptions) -> String {
        let _ = self.ensure_index(ws);
        repomap::render(&self.index, opts)
    }
}

/// Byte offset of the first whole-word `name` in `text`.
fn first_word(text: &str, name: &str) -> Option<usize> {
    let ident = |c: char| c.is_alphanumeric() || c == '_';
    text.match_indices(name).map(|(i, _)| i).find(|&i| {
        !text[..i].chars().next_back().is_some_and(ident) && !text[i + name.len()..].chars().next().is_some_and(ident)
    })
}

/// LSP `SymbolKind` as the index's kind words.
pub fn symbol_kind(k: u64) -> &'static str {
    match k {
        2 => "module",
        3 => "namespace",
        4 => "package",
        5 => "class",
        6 => "method",
        7 => "property",
        8 => "field",
        9 => "constructor",
        10 => "enum",
        11 => "interface",
        12 => "function",
        13 => "variable",
        14 => "constant",
        22 => "variant",
        23 => "class",
        26 => "type",
        _ => "symbol",
    }
}

/// An index for `root` in `home` (on disk), or in memory without a home.
pub fn open_index(home: Option<&Path>, root: &Path) -> std::result::Result<Index, IndexError> {
    match home {
        Some(h) => Index::open(&crate::index::db_path(h, root), root),
        None => Index::in_memory(root),
    }
}

/// Shared per-root engines, for hosts that serve more than one root.
#[derive(Default)]
pub struct Roots {
    map: Mutex<HashMap<String, Arc<CodeIntel>>>,
}

impl Roots {
    /// The engine for `root`, made by `make` the first time.
    pub fn get(&self, root: &Path, make: impl FnOnce() -> Option<CodeIntel>) -> Option<Arc<CodeIntel>> {
        let mut map = self.map.lock().unwrap_or_else(|p| p.into_inner());
        let key = norm_key(root);
        if let Some(c) = map.get(&key) {
            return Some(Arc::clone(c));
        }
        let c = Arc::new(make()?);
        map.insert(key, Arc::clone(&c));
        Some(c)
    }
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn symbols_split_on_rust_python_and_js_separators() {
        assert_eq!(split_symbol("Engine::start"), (Some("Engine".into()), "start".into()));
        assert_eq!(split_symbol("crate::index::Index::refresh()"), (Some("Index".into()), "refresh".into()));
        assert_eq!(split_symbol("pkg.mod.func"), (Some("mod".into()), "func".into()));
        assert_eq!(split_symbol("Widget#render"), (Some("Widget".into()), "render".into()));
        assert_eq!(split_symbol("plain"), (None, "plain".into()));
        assert_eq!(split_symbol("::rooted"), (None, "rooted".into()));
        assert!(is_identifier("new_name") && is_identifier("$el") && !is_identifier("9x") && !is_identifier("a-b"));
        assert_eq!(first_word("restart(); start()", "start"), Some(11));
    }

    #[test]
    fn hover_text_reads_every_shape() {
        assert_eq!(hover_text(&json!({"contents": {"kind": "markdown", "value": " doc "}})), "doc");
        assert_eq!(hover_text(&json!({"contents": [{"language": "rust", "value": "fn a()"}, "text"]})), "```rust\nfn a()\n```\n\ntext");
        assert_eq!(hover_text(&json!({"contents": "plain"})), "plain");
        assert_eq!(hover_text(&Value::Null), "");
    }
}
