//! One running language server: the JSON-RPC session over a [`Channel`].
//!
//! A dispatcher thread reads every incoming message: responses go to the waiting request,
//! `publishDiagnostics` goes to the per-file store (with a generation counter waiters watch),
//! `$/progress` and rust-analyzer's `experimental/serverStatus` track readiness, and the server's
//! own requests get answers (`workspace/configuration` gets the server's settings, registrations
//! are recorded, `workspace/applyEdit` is refused: only Marlowe's tools change files). The
//! thread holds a weak reference, so dropping the client kills the server and ends it.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use lsp_server::{Message, Notification, Request, RequestId, Response};
use serde_json::{json, Value};

use super::servers::ServerSpec;
use super::transport::{Channel, Launch, ProcessGuard};
use super::uri::{path_to_uri, uri_to_path, Encoding};
use super::LspError;
use crate::index::{norm_key, Lang};

/// JSON-RPC: the document changed under the request; retry it.
const CONTENT_MODIFIED: i32 = -32801;
/// JSON-RPC: the server cancelled the request.
const SERVER_CANCELLED: i32 = -32802;
const METHOD_NOT_FOUND: i32 = -32601;

/// One diagnostic. Lines are 0-based; `col` is in the server's units.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diag {
    pub line: u32,
    pub col: u32,
    pub end_line: u32,
    /// 1 error, 2 warning, 3 information, 4 hint (missing: error).
    pub severity: u8,
    pub code: Option<String>,
    pub source: Option<String>,
    pub message: String,
}

impl Diag {
    pub fn from_value(v: &Value) -> Option<Diag> {
        let start = v.get("range")?.get("start")?;
        let end = v.get("range").and_then(|r| r.get("end"));
        let code = match v.get("code") {
            Some(Value::String(s)) => Some(s.clone()),
            Some(Value::Number(n)) => Some(n.to_string()),
            _ => None,
        };
        Some(Diag {
            line: start.get("line")?.as_u64()? as u32,
            col: start.get("character").and_then(Value::as_u64).unwrap_or(0) as u32,
            end_line: end.and_then(|e| e.get("line")).and_then(Value::as_u64).unwrap_or(0) as u32,
            severity: v.get("severity").and_then(Value::as_u64).map_or(1, |s| s.clamp(1, 4) as u8),
            code,
            source: v.get("source").and_then(Value::as_str).map(str::to_string),
            message: v.get("message").and_then(Value::as_str).unwrap_or("").trim().to_string(),
        })
    }

    pub fn severity_name(&self) -> &'static str {
        match self.severity {
            1 => "error",
            2 => "warning",
            3 => "info",
            _ => "hint",
        }
    }
}

/// The diagnostics a server last published for one file.
#[derive(Debug, Clone)]
struct Published {
    path: PathBuf,
    version: Option<i64>,
    items: Vec<Diag>,
    gen: u64,
    at: Instant,
}

#[derive(Debug, Clone)]
struct Doc {
    path: PathBuf,
    version: i64,
    hash: blake3::Hash,
    /// The diagnostics generation just before the change that made `version`.
    changed_gen: u64,
}

/// Tells clients apart across restarts (each has its own generation counter).
static INSTANCES: AtomicU64 = AtomicU64::new(1);

#[derive(Default)]
struct State {
    alive: bool,
    caps: Value,
    encoding: Encoding,
    /// Dynamic registrations: id -> method.
    registrations: HashMap<String, String>,
    docs: HashMap<String, Doc>,
    diags: HashMap<String, Published>,
    /// Bumped on every publish.
    gen: u64,
    /// `$/progress` tokens between begin and end.
    progress: HashSet<String>,
    /// rust-analyzer's `serverStatus.quiescent`.
    quiescent: Option<bool>,
    /// The last non-ok `serverStatus` message.
    health: Option<String>,
}

struct Inner {
    spec: ServerSpec,
    root: PathBuf,
    /// Started with the workspace's own programs and build steps allowed (see `servers`).
    trusted: bool,
    instance: u64,
    tx: Sender<Message>,
    next_id: AtomicI32,
    pending: Mutex<HashMap<RequestId, Sender<Result<Value, lsp_server::ResponseError>>>>,
    state: Mutex<State>,
    changed: Condvar,
    guard: Mutex<Option<Box<dyn ProcessGuard>>>,
    stderr: Arc<Mutex<VecDeque<String>>>,
    last_used: Mutex<Instant>,
    started: Instant,
}

/// A running, initialized server.
#[derive(Clone)]
pub struct LspClient {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for LspClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LspClient").field("server", &self.inner.spec.id).field("root", &self.inner.root).finish()
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl LspClient {
    /// Starts `spec` for `root` and runs the initialize handshake (bounded by `timeout`).
    pub fn start(launch: &dyn Launch, spec: &ServerSpec, root: &Path, trusted: bool, timeout: Duration) -> Result<Self, LspError> {
        let Channel { tx, rx, guard, stderr } = launch.launch(spec, root, trusted)?;
        let inner = Arc::new(Inner {
            spec: spec.clone(),
            root: root.to_path_buf(),
            trusted,
            instance: INSTANCES.fetch_add(1, Ordering::Relaxed),
            tx,
            next_id: AtomicI32::new(1),
            pending: Mutex::new(HashMap::new()),
            state: Mutex::new(State { alive: true, ..State::default() }),
            changed: Condvar::new(),
            guard: Mutex::new(guard),
            stderr,
            last_used: Mutex::new(Instant::now()),
            started: Instant::now(),
        });
        let weak = Arc::downgrade(&inner);
        std::thread::Builder::new()
            .name(format!("lsp-{}", spec.id))
            .spawn(move || dispatch(weak, rx))
            .map_err(|e| LspError::Start(e.to_string()))?;
        let client = LspClient { inner };
        let root_uri = path_to_uri(root);
        let name = root.file_name().map_or_else(|| "workspace".to_string(), |n| n.to_string_lossy().into_owned());
        let params = json!({
            "processId": std::process::id(),
            "clientInfo": {"name": "marlowe", "version": env!("CARGO_PKG_VERSION")},
            "rootUri": root_uri,
            "rootPath": root.to_string_lossy(),
            "workspaceFolders": [{"uri": root_uri, "name": name}],
            "initializationOptions": spec.init_options_for(root, trusted),
            "capabilities": client_capabilities(),
        });
        let result = client.request_raw("initialize", params, timeout, &|| false).map_err(|e| match e {
            LspError::Timeout(_) | LspError::Gone(_) => LspError::Start(format!(
                "{} did not initialize: {e}{}",
                spec.id,
                client.stderr_hint()
            )),
            other => other,
        })?;
        {
            let mut st = lock(&client.inner.state);
            st.caps = result.get("capabilities").cloned().unwrap_or(Value::Null);
            st.encoding = Encoding::from_server(st.caps.get("positionEncoding").and_then(Value::as_str));
        }
        client.notify("initialized", json!({}));
        Ok(client)
    }

    pub fn id(&self) -> &'static str {
        self.inner.spec.id
    }

    pub fn spec(&self) -> &ServerSpec {
        &self.inner.spec
    }

    pub fn root(&self) -> &Path {
        &self.inner.root
    }

    /// Whether the server was started trusted (may run the workspace's build steps).
    pub fn trusted(&self) -> bool {
        self.inner.trusted
    }

    /// This client's identity: a restarted server is a new instance with new generations.
    pub fn instance(&self) -> u64 {
        self.inner.instance
    }

    pub fn alive(&self) -> bool {
        let alive = lock(&self.inner.state).alive;
        alive && lock(&self.inner.guard).as_mut().is_none_or(|g| g.alive())
    }

    pub fn encoding(&self) -> Encoding {
        lock(&self.inner.state).encoding
    }

    /// The server's capabilities from initialize.
    pub fn capabilities(&self) -> Value {
        lock(&self.inner.state).caps.clone()
    }

    /// Whether the server offers `provider` (`renameProvider`, `referencesProvider`, ...).
    pub fn has(&self, provider: &str) -> bool {
        let st = lock(&self.inner.state);
        match st.caps.get(provider) {
            None | Some(Value::Null) | Some(Value::Bool(false)) => false,
            Some(_) => true,
        }
    }

    /// Whether the server takes `textDocument/diagnostic` (statically or by registration).
    pub fn pulls_diagnostics(&self) -> bool {
        self.has("diagnosticProvider") || lock(&self.inner.state).registrations.values().any(|m| m == "textDocument/diagnostic")
    }

    /// Marks the client used now (idle stop counts from here).
    pub fn touch(&self) {
        *lock(&self.inner.last_used) = Instant::now();
    }

    /// How long ago the server started.
    pub fn age(&self) -> Duration {
        self.inner.started.elapsed()
    }

    pub fn idle_for(&self) -> Duration {
        lock(&self.inner.last_used).elapsed()
    }

    fn stderr_hint(&self) -> String {
        let tail = lock(&self.inner.stderr);
        match tail.back() {
            Some(last) => format!(" (stderr: {})", last.trim()),
            None => String::new(),
        }
    }

    /// The server's last stderr lines.
    pub fn stderr_tail(&self) -> Vec<String> {
        lock(&self.inner.stderr).iter().cloned().collect()
    }

    /// Why the server says it is unhealthy (rust-analyzer's `serverStatus` message), if it does.
    pub fn health(&self) -> Option<String> {
        lock(&self.inner.state).health.clone()
    }

    pub fn notify(&self, method: &str, params: Value) {
        let _ = self.inner.tx.send(Message::Notification(Notification::new(method.to_string(), params)));
    }

    /// A request, retried up to three times while the server says the content was modified.
    pub fn request(&self, method: &str, params: Value, timeout: Duration, cancelled: &dyn Fn() -> bool) -> Result<Value, LspError> {
        self.touch();
        let mut tries = 0;
        loop {
            match self.request_raw(method, params.clone(), timeout, cancelled) {
                Err(LspError::Server { code, .. }) if (code == CONTENT_MODIFIED || code == SERVER_CANCELLED) && tries < 3 => {
                    tries += 1;
                    std::thread::sleep(Duration::from_millis(250));
                }
                other => return other,
            }
        }
    }

    fn request_raw(&self, method: &str, params: Value, timeout: Duration, cancelled: &dyn Fn() -> bool) -> Result<Value, LspError> {
        if !lock(&self.inner.state).alive {
            return Err(LspError::Gone(self.id().to_string()));
        }
        let id = RequestId::from(self.inner.next_id.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = crossbeam_channel::bounded(1);
        lock(&self.inner.pending).insert(id.clone(), tx);
        if self.inner.tx.send(Message::Request(Request::new(id.clone(), method.to_string(), params))).is_err() {
            lock(&self.inner.pending).remove(&id);
            return Err(LspError::Gone(self.id().to_string()));
        }
        let deadline = Instant::now() + timeout;
        loop {
            let slice = deadline.saturating_duration_since(Instant::now()).min(Duration::from_millis(100));
            match rx.recv_timeout(slice) {
                Ok(Ok(v)) => return Ok(v),
                Ok(Err(e)) => return Err(LspError::Server { method: method.to_string(), code: e.code, message: e.message }),
                Err(RecvTimeoutError::Disconnected) => return Err(LspError::Gone(self.id().to_string())),
                Err(RecvTimeoutError::Timeout) => {
                    let interrupted = cancelled();
                    if interrupted || Instant::now() >= deadline {
                        lock(&self.inner.pending).remove(&id);
                        self.notify("$/cancelRequest", json!({ "id": id }));
                        return Err(if interrupted {
                            LspError::Cancelled
                        } else {
                            LspError::Timeout(format!("{} {method} after {:.1} s", self.id(), timeout.as_secs_f32()))
                        });
                    }
                }
            }
        }
    }

    /// Waits until the server has loaded the workspace: rust-analyzer's `serverStatus` reports
    /// quiescent, or (other servers) no work-done progress is open. `false` when `max` passed or
    /// the call was interrupted.
    pub fn wait_ready(&self, max: Duration, cancelled: &dyn Fn() -> bool) -> bool {
        let deadline = Instant::now() + max;
        let mut st = lock(&self.inner.state);
        loop {
            let ready = if self.inner.spec.server_status {
                st.quiescent == Some(true)
            } else {
                st.progress.is_empty()
            };
            if ready || !st.alive {
                return ready;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() || cancelled() {
                return false;
            }
            st = self.inner.changed.wait_timeout(st, left.min(Duration::from_millis(100))).unwrap_or_else(|p| p.into_inner()).0;
        }
    }

    /// Makes the server's copy of `path` equal `text`: opens it, or sends the full new text as a
    /// change (and a save, which is what starts rust-analyzer's `cargo check`). Returns the
    /// document version now and the diagnostics generation just before the change that made it,
    /// for [`Self::wait_diagnostics`]. Unchanged text sends nothing.
    pub fn sync(&self, path: &Path, text: &str) -> (i64, u64) {
        self.touch();
        let key = norm_key(path);
        let uri = path_to_uri(path);
        let hash = blake3::hash(text.as_bytes());
        let mut st = lock(&self.inner.state);
        let gen = st.gen;
        match st.docs.get_mut(&key) {
            Some(doc) if doc.hash == hash => (doc.version, doc.changed_gen),
            Some(doc) => {
                doc.version += 1;
                doc.hash = hash;
                doc.changed_gen = gen;
                let version = doc.version;
                drop(st);
                self.notify(
                    "textDocument/didChange",
                    json!({"textDocument": {"uri": uri, "version": version}, "contentChanges": [{"text": text}]}),
                );
                self.did_save(&uri, text);
                (version, gen)
            }
            None => {
                st.docs.insert(key, Doc { path: path.to_path_buf(), version: 1, hash, changed_gen: gen });
                drop(st);
                let lang = Lang::of(path).map_or("plaintext", |l| l.language_id(path));
                self.notify(
                    "textDocument/didOpen",
                    json!({"textDocument": {"uri": uri, "languageId": lang, "version": 1, "text": text}}),
                );
                (1, gen)
            }
        }
    }

    fn did_save(&self, uri: &str, text: &str) {
        let save = lock(&self.inner.state).caps.get("textDocumentSync").and_then(|s| s.get("save")).cloned();
        let include = save.as_ref().and_then(|s| s.get("includeText")).and_then(Value::as_bool).unwrap_or(false);
        if save.is_some_and(|s| s != Value::Bool(false)) {
            let mut p = json!({"textDocument": {"uri": uri}});
            if include {
                p["text"] = json!(text);
            }
            self.notify("textDocument/didSave", p);
        }
    }

    /// Whether the server has `path` open.
    pub fn is_open(&self, path: &Path) -> bool {
        lock(&self.inner.state).docs.contains_key(&norm_key(path))
    }

    /// The files the server has open (it goes by Marlowe's copy of them, not the disk).
    pub fn open_docs(&self) -> Vec<PathBuf> {
        lock(&self.inner.state).docs.values().map(|d| d.path.clone()).collect()
    }

    /// The version and content hash of the server's copy of `path`, when it has it open.
    pub fn doc(&self, path: &Path) -> Option<(i64, blake3::Hash)> {
        lock(&self.inner.state).docs.get(&norm_key(path)).map(|d| (d.version, d.hash))
    }

    /// The diagnostics last published for `path`, if any were.
    pub fn diagnostics(&self, path: &Path) -> Option<Vec<Diag>> {
        lock(&self.inner.state).diags.get(&norm_key(path)).map(|p| p.items.clone())
    }

    /// The last publish for `path`: its generation and diagnostics.
    pub fn published(&self, path: &Path) -> Option<(u64, Vec<Diag>)> {
        lock(&self.inner.state).diags.get(&norm_key(path)).map(|p| (p.gen, p.items.clone()))
    }

    /// Every file with published diagnostics, and them.
    pub fn all_diagnostics(&self) -> Vec<(PathBuf, Vec<Diag>)> {
        let st = lock(&self.inner.state);
        let mut out: Vec<(PathBuf, Vec<Diag>)> =
            st.diags.values().filter(|p| !p.items.is_empty()).map(|p| (p.path.clone(), p.items.clone())).collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Waits for diagnostics on `path` published after generation `after` for a version at
    /// least `min_version`, then for `settle` without another publish for it, at most `max` in
    /// all. `None` when nothing new came in time.
    pub fn wait_diagnostics(
        &self,
        path: &Path,
        after: u64,
        min_version: i64,
        settle: Duration,
        max: Duration,
        cancelled: &dyn Fn() -> bool,
    ) -> Option<Vec<Diag>> {
        self.wait_published(path, after, min_version, settle, max, cancelled).map(|p| p.1)
    }

    /// [`Self::wait_diagnostics`] with the generation of the publish it settled on.
    pub fn wait_published(
        &self,
        path: &Path,
        after: u64,
        min_version: i64,
        settle: Duration,
        max: Duration,
        cancelled: &dyn Fn() -> bool,
    ) -> Option<(u64, Vec<Diag>)> {
        let key = norm_key(path);
        let deadline = Instant::now() + max;
        let mut st = lock(&self.inner.state);
        loop {
            let fresh = st
                .diags
                .get(&key)
                .filter(|p| p.gen > after && p.version.is_none_or(|v| v >= min_version))
                .map(|p| (p.at, p.gen, p.items.clone()));
            let now = Instant::now();
            if let Some((at, gen, items)) = &fresh {
                if now.duration_since(*at) >= settle || now >= deadline {
                    return Some((*gen, items.clone()));
                }
            } else if now >= deadline || !st.alive {
                return None;
            }
            if cancelled() {
                return fresh.map(|f| (f.1, f.2));
            }
            let wake = match &fresh {
                Some((at, ..)) => (*at + settle).min(deadline),
                None => deadline,
            };
            let wait = wake.saturating_duration_since(now).clamp(Duration::from_millis(5), Duration::from_millis(100));
            st = self.inner.changed.wait_timeout(st, wait).unwrap_or_else(|p| p.into_inner()).0;
        }
    }

    /// `textDocument/diagnostic` for `path` (servers that take pull requests).
    pub fn pull_diagnostics(&self, path: &Path, timeout: Duration, cancelled: &dyn Fn() -> bool) -> Option<Vec<Diag>> {
        self.pull_published(path, timeout, cancelled).map(|p| p.1)
    }

    /// [`Self::pull_diagnostics`] with the generation it was stored at.
    pub fn pull_published(&self, path: &Path, timeout: Duration, cancelled: &dyn Fn() -> bool) -> Option<(u64, Vec<Diag>)> {
        if !self.pulls_diagnostics() {
            return None;
        }
        let r = self
            .request("textDocument/diagnostic", json!({"textDocument": {"uri": path_to_uri(path)}}), timeout, cancelled)
            .ok()?;
        if r.get("kind").and_then(Value::as_str) == Some("unchanged") {
            return self.published(path);
        }
        let items: Vec<Diag> = r.get("items")?.as_array()?.iter().filter_map(Diag::from_value).collect();
        let mut st = lock(&self.inner.state);
        st.gen += 1;
        let gen = st.gen;
        st.diags.insert(
            norm_key(path),
            Published { path: path.to_path_buf(), version: None, items: items.clone(), gen, at: Instant::now() },
        );
        Some((gen, items))
    }

    /// Polite stop: `shutdown`, `exit`, then the process tree is killed if it is still there.
    pub fn shutdown(&self) {
        let _ = self.request_raw("shutdown", Value::Null, Duration::from_secs(2), &|| false);
        self.notify("exit", Value::Null);
        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline && lock(&self.inner.guard).as_mut().is_some_and(|g| g.alive()) {
            std::thread::sleep(Duration::from_millis(20));
        }
        lock(&self.inner.state).alive = false;
        lock(&self.inner.guard).take();
        self.inner.changed.notify_all();
    }
}

/// What Marlowe tells servers it can do.
fn client_capabilities() -> Value {
    json!({
        "general": {"positionEncodings": ["utf-8", "utf-16"]},
        "workspace": {
            "configuration": true,
            "workspaceFolders": true,
            "symbol": {"symbolKind": {"valueSet": (1..=26).collect::<Vec<u32>>()}},
            "workspaceEdit": {"documentChanges": true, "resourceOperations": ["create", "rename", "delete"]},
            "didChangeConfiguration": {"dynamicRegistration": false},
            "diagnostics": {"refreshSupport": false},
        },
        "textDocument": {
            "synchronization": {"didSave": true, "dynamicRegistration": false},
            "publishDiagnostics": {"relatedInformation": false, "versionSupport": true, "codeDescriptionSupport": false},
            "diagnostic": {"dynamicRegistration": true, "relatedDocumentSupport": false},
            "hover": {"contentFormat": ["markdown", "plaintext"]},
            "definition": {"linkSupport": true},
            "typeDefinition": {"linkSupport": true},
            "implementation": {"linkSupport": true},
            "references": {},
            "documentSymbol": {"hierarchicalDocumentSymbolSupport": true, "symbolKind": {"valueSet": (1..=26).collect::<Vec<u32>>()}},
            "rename": {"prepareSupport": true},
        },
        "window": {"workDoneProgress": true},
        "experimental": {"serverStatusNotification": true},
    })
}

/// The dispatcher: one per client, ends when the server's output closes or the client is gone.
fn dispatch(weak: Weak<Inner>, rx: Receiver<Message>) {
    let trace = std::env::var_os("MARLOWE_LSP_TRACE").is_some();
    for msg in rx.iter() {
        let Some(inner) = weak.upgrade() else { return };
        if trace {
            let text = serde_json::to_string(&msg).unwrap_or_default();
            eprintln!("lsp {} <- {}", inner.spec.id, &text[..text.len().min(600)]);
        }
        match msg {
            Message::Response(r) => {
                if let Some(tx) = lock(&inner.pending).remove(&r.id) {
                    let _ = tx.send(r.response_result);
                }
            }
            Message::Notification(n) => on_notification(&inner, n),
            Message::Request(r) => on_request(&inner, r),
        }
    }
    if let Some(inner) = weak.upgrade() {
        lock(&inner.state).alive = false;
        lock(&inner.pending).clear();
        inner.changed.notify_all();
    }
}

fn on_notification(inner: &Inner, n: Notification) {
    match n.method.as_str() {
        "textDocument/publishDiagnostics" => {
            let Some(path) = n.params.get("uri").and_then(Value::as_str).and_then(uri_to_path) else { return };
            let items: Vec<Diag> = n
                .params
                .get("diagnostics")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Diag::from_value).collect())
                .unwrap_or_default();
            let version = n.params.get("version").and_then(Value::as_i64);
            let mut st = lock(&inner.state);
            st.gen += 1;
            let gen = st.gen;
            st.diags.insert(norm_key(&path), Published { path, version, items, gen, at: Instant::now() });
            drop(st);
            inner.changed.notify_all();
        }
        "$/progress" => {
            let token = match n.params.get("token") {
                Some(Value::String(s)) => s.clone(),
                Some(v) => v.to_string(),
                None => return,
            };
            let kind = n.params.get("value").and_then(|v| v.get("kind")).and_then(Value::as_str);
            let mut st = lock(&inner.state);
            match kind {
                Some("begin") => {
                    st.progress.insert(token);
                }
                Some("end") => {
                    st.progress.remove(&token);
                }
                _ => {}
            }
            drop(st);
            inner.changed.notify_all();
        }
        "experimental/serverStatus" => {
            let mut st = lock(&inner.state);
            st.quiescent = n.params.get("quiescent").and_then(Value::as_bool);
            let health = n.params.get("health").and_then(Value::as_str).unwrap_or("ok");
            st.health = (health != "ok").then(|| {
                n.params.get("message").and_then(Value::as_str).unwrap_or(health).trim().to_string()
            });
            drop(st);
            inner.changed.notify_all();
        }
        _ => {}
    }
}

fn on_request(inner: &Inner, r: Request) {
    let reply = |result: Value| Response::new_ok(r.id.clone(), result);
    let resp = match r.method.as_str() {
        "workspace/configuration" => {
            let items = r.params.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
            let answers: Vec<Value> = items
                .iter()
                .map(|item| match item.get("section").and_then(Value::as_str) {
                    Some(s) if s == inner.spec.id => inner.spec.init_options_for(&inner.root, inner.trusted),
                    _ => Value::Null,
                })
                .collect();
            reply(Value::Array(answers))
        }
        "client/registerCapability" => {
            let mut st = lock(&inner.state);
            for reg in r.params.get("registrations").and_then(Value::as_array).into_iter().flatten() {
                if let Some(m) = reg.get("method").and_then(Value::as_str) {
                    let id = reg.get("id").and_then(Value::as_str).unwrap_or(m);
                    st.registrations.insert(id.to_string(), m.to_string());
                }
            }
            reply(Value::Null)
        }
        "client/unregisterCapability" => {
            let mut st = lock(&inner.state);
            for reg in r.params.get("unregisterations").and_then(Value::as_array).into_iter().flatten() {
                if let Some(m) = reg.get("method").and_then(Value::as_str) {
                    let id = reg.get("id").and_then(Value::as_str).unwrap_or(m);
                    st.registrations.remove(id);
                }
            }
            reply(Value::Null)
        }
        "window/workDoneProgress/create"
        | "window/showMessageRequest"
        | "workspace/diagnostic/refresh"
        | "workspace/semanticTokens/refresh"
        | "workspace/inlayHint/refresh"
        | "workspace/inlineValue/refresh"
        | "workspace/codeLens/refresh" => reply(Value::Null),
        "workspace/workspaceFolders" => {
            let uri = path_to_uri(&inner.root);
            reply(json!([{"uri": uri, "name": "workspace"}]))
        }
        "workspace/applyEdit" => reply(json!({"applied": false, "failureReason": "Marlowe applies edits through its own tools"})),
        _ => Response::new_err(r.id.clone(), METHOD_NOT_FOUND, format!("marlowe does not handle {}", r.method)),
    };
    let _ = inner.tx.send(Message::Response(resp));
}
