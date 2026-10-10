//! A purpose-built in-memory language server for tests (here and in `marlowe-exec`'s code-tool
//! tests), so every LSP path runs on CPU without a real server. It is deliberately simple and
//! deterministic:
//!
//! - it reads the workspace's files at `initialize` (extensions in [`FakeConfig::exts`]) and
//!   keeps open documents' text from `didOpen`/`didChange`;
//! - a definition is a line with `fn`, `struct`, `enum`, `trait`, `def`, `class`, `function` or
//!   `interface` followed by a name; a reference is any whole-word occurrence of a name;
//! - it answers `workspace/symbol`, `documentSymbol`, `definition`, `references`, `hover`,
//!   `prepareRename` and `rename` from that, in UTF-16 (or UTF-8 when [`FakeConfig::utf8`] and the
//!   client offers it);
//! - after `didOpen`/`didChange` it publishes one error per line containing `ERROR:` and one
//!   warning per `WARN:` (message: the rest of the line), after [`FakeConfig::publish_delay`];
//!   with [`FakeConfig::pull`] it also answers `textDocument/diagnostic`.
//!
//! [`FakeLauncher::log`] records every method the client sent, for assertions.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossbeam_channel::Sender;
use lsp_server::{Connection, Message, Notification, Request, Response};
use serde_json::{json, Value};

use super::servers::ServerSpec;
use super::transport::{Channel, Launch, ProcessGuard};
use super::uri::{offset_of, path_to_uri, position_of, uri_to_path, Encoding};
use super::LspError;
use crate::index::{norm_key, Lang};

/// How the fake behaves.
#[derive(Debug, Clone)]
pub struct FakeConfig {
    /// File extensions it reads at initialize.
    pub exts: Vec<&'static str>,
    /// Delay before publishing diagnostics.
    pub publish_delay: Duration,
    /// Offer and answer `textDocument/diagnostic`.
    pub pull: bool,
    /// Agree to UTF-8 positions when offered.
    pub utf8: bool,
    /// Answer `rename` with `documentChanges` instead of `changes`.
    pub document_changes: bool,
    /// Send rust-analyzer's `serverStatus`: not quiescent, then quiescent after this long.
    pub status_after: Option<Duration>,
    /// Fail `initialize` (a server that crashes on start).
    pub fail_init: bool,
}

impl Default for FakeConfig {
    fn default() -> Self {
        Self {
            exts: vec!["rs", "py", "ts", "js"],
            publish_delay: Duration::from_millis(30),
            pull: false,
            utf8: false,
            document_changes: false,
            status_after: None,
            fail_init: false,
        }
    }
}

/// Starts fakes; every launch is counted and every client method logged.
#[derive(Debug, Default, Clone)]
pub struct FakeLauncher {
    pub config: FakeConfig,
    log: Arc<Mutex<Vec<String>>>,
    launches: Arc<Mutex<Vec<String>>>,
}

impl FakeLauncher {
    pub fn new(config: FakeConfig) -> Self {
        Self { config, ..Self::default() }
    }

    /// Methods the clients sent, in order.
    pub fn log(&self) -> Vec<String> {
        self.log.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// The server ids launched, in order.
    pub fn launches(&self) -> Vec<String> {
        self.launches.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// A spec for the fake serving `langs` (no program; [`FakeLauncher`] never looks for one).
    pub fn spec(id: &'static str, langs: &[Lang], markers: &[&'static str]) -> ServerSpec {
        ServerSpec {
            id,
            langs: langs.to_vec(),
            command: vec!["fake-lsp".into()],
            init_options: json!({"fake": true}),
            env: Vec::new(),
            markers: markers.to_vec(),
            server_status: false,
            options_for_root: None,
            command_for_root: None,
        }
    }
}

struct FakeGuard(Arc<Mutex<bool>>);

impl ProcessGuard for FakeGuard {
    fn alive(&mut self) -> bool {
        *self.0.lock().unwrap_or_else(|p| p.into_inner())
    }
}

impl Launch for FakeLauncher {
    fn launch(&self, spec: &ServerSpec, root: &Path) -> Result<Channel, LspError> {
        self.launches.lock().unwrap_or_else(|p| p.into_inner()).push(spec.id.to_string());
        let (client, server) = Connection::memory();
        let alive = Arc::new(Mutex::new(true));
        let state = Fake {
            cfg: self.config.clone(),
            root: root.to_path_buf(),
            files: BTreeMap::new(),
            versions: BTreeMap::new(),
            enc: Encoding::Utf16,
            log: Arc::clone(&self.log),
            out: server.sender.clone(),
        };
        let a = Arc::clone(&alive);
        std::thread::Builder::new()
            .name("fake-lsp".into())
            .spawn(move || {
                state.serve(server);
                *a.lock().unwrap_or_else(|p| p.into_inner()) = false;
            })
            .map_err(|e| LspError::Start(e.to_string()))?;
        Ok(Channel {
            tx: client.sender,
            rx: client.receiver,
            guard: Some(Box::new(FakeGuard(alive))),
            stderr: Arc::default(),
        })
    }
}

struct Fake {
    cfg: FakeConfig,
    root: PathBuf,
    /// key -> (path, text)
    files: BTreeMap<String, (PathBuf, String)>,
    versions: BTreeMap<String, i64>,
    enc: Encoding,
    log: Arc<Mutex<Vec<String>>>,
    out: Sender<Message>,
}

const DEF_KEYWORDS: &[&str] = &["fn", "struct", "enum", "trait", "def", "class", "function", "interface"];

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// (name, byte offset of the name) of the definition on `line`, if it is one.
fn def_on_line(line: &str) -> Option<(&str, usize)> {
    let mut at = 0;
    for word in line.split(|c: char| !is_ident(c)) {
        let start = line[at..].find(word).map_or(at, |i| at + i);
        at = start + word.len();
        if word.is_empty() {
            continue;
        }
        if DEF_KEYWORDS.contains(&word) {
            let rest = &line[at..];
            let skip = rest.len() - rest.trim_start().len();
            let name_start = at + skip;
            let name: &str = &line[name_start..];
            let end = name.find(|c: char| !is_ident(c)).unwrap_or(name.len());
            if end > 0 {
                return Some((&line[name_start..name_start + end], name_start));
            }
        }
    }
    None
}

/// Byte offsets of whole-word `name` in `text`.
fn occurrences(text: &str, name: &str) -> Vec<usize> {
    let mut out = Vec::new();
    for (i, _) in text.match_indices(name) {
        let before = text[..i].chars().next_back();
        let after = text[i + name.len()..].chars().next();
        if !before.is_some_and(is_ident) && !after.is_some_and(is_ident) {
            out.push(i);
        }
    }
    out
}

fn word_at(text: &str, offset: usize) -> Option<(usize, usize)> {
    let is_word = |i: usize| text[i..].chars().next().is_some_and(is_ident);
    if offset > text.len() || !text.is_char_boundary(offset) {
        return None;
    }
    let mut start = offset;
    while start > 0 {
        let prev = text[..start].char_indices().next_back()?.0;
        if !is_word(prev) {
            break;
        }
        start = prev;
    }
    let mut end = offset;
    while end < text.len() && is_word(end) {
        end += text[end..].chars().next()?.len_utf8();
    }
    (end > start).then_some((start, end))
}

impl Fake {
    fn serve(mut self, conn: Connection) {
        for msg in conn.receiver.iter() {
            match msg {
                Message::Request(r) => {
                    self.note(&r.method);
                    if r.method == "shutdown" {
                        let _ = self.out.send(Response::new_ok(r.id, Value::Null).into());
                        continue;
                    }
                    let resp = self.request(&r);
                    let _ = self.out.send(resp.into());
                }
                Message::Notification(n) => {
                    self.note(&n.method);
                    if n.method == "exit" {
                        return;
                    }
                    self.notification(n);
                }
                Message::Response(_) => {}
            }
        }
    }

    fn note(&self, method: &str) {
        self.log.lock().unwrap_or_else(|p| p.into_inner()).push(method.to_string());
    }

    fn load_root(&mut self) {
        fn walk(dir: &Path, exts: &[&str], out: &mut Vec<PathBuf>) {
            let Ok(rd) = std::fs::read_dir(dir) else { return };
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    if !p.file_name().is_some_and(|n| n == "target" || n == ".git") {
                        walk(&p, exts, out);
                    }
                } else if p.extension().and_then(|x| x.to_str()).is_some_and(|x| exts.contains(&x)) {
                    out.push(p);
                }
            }
        }
        let mut found = Vec::new();
        walk(&self.root, &self.cfg.exts, &mut found);
        for p in found {
            if let Ok(text) = std::fs::read_to_string(&p) {
                self.files.insert(norm_key(&p), (p, text));
            }
        }
    }

    fn pos(&self, text: &str, off: usize) -> Value {
        let (line, character) = position_of(text, off, self.enc);
        json!({"line": line, "character": character})
    }

    fn range(&self, text: &str, start: usize, end: usize) -> Value {
        json!({"start": self.pos(text, start), "end": self.pos(text, end)})
    }

    fn defs(&self, name: &str) -> Vec<(PathBuf, usize, usize, String)> {
        let mut out = Vec::new();
        for (path, text) in self.files.values() {
            let mut at = 0;
            for line in text.split_inclusive('\n') {
                if let Some((n, off)) = def_on_line(line) {
                    if n == name {
                        out.push((path.clone(), at + off, at + off + n.len(), line.trim().to_string()));
                    }
                }
                at += line.len();
            }
        }
        out
    }

    /// (text, byte offset) at a request's `textDocument` + `position`.
    fn at(&self, params: &Value) -> Option<(PathBuf, String, usize)> {
        let path = uri_to_path(params.get("textDocument")?.get("uri")?.as_str()?)?;
        let (_, text) = self.files.get(&norm_key(&path))?;
        let p = params.get("position")?;
        let off = offset_of(text, p.get("line")?.as_u64()? as u32, p.get("character")?.as_u64()? as u32, self.enc)?;
        Some((path, text.clone(), off))
    }

    fn name_at(&self, params: &Value) -> Option<String> {
        let (_, text, off) = self.at(params)?;
        let (s, e) = word_at(&text, off)?;
        Some(text[s..e].to_string())
    }

    fn location(&self, path: &Path, start: usize, end: usize) -> Value {
        let text = &self.files[&norm_key(path)].1;
        json!({"uri": path_to_uri(path), "range": self.range(text, start, end)})
    }

    fn symbol_kind(line: &str) -> u32 {
        if line.contains("struct") || line.contains("enum") {
            23
        } else if line.contains("class") || line.contains("trait") || line.contains("interface") {
            5
        } else {
            12
        }
    }

    fn request(&mut self, r: &Request) -> Response {
        let p = &r.params;
        let ok = |v: Value| Response::new_ok(r.id.clone(), v);
        match r.method.as_str() {
            "initialize" => {
                if self.cfg.fail_init {
                    return Response::new_err(r.id.clone(), -32603, "fake: initialize failed".into());
                }
                let offered = p.pointer("/capabilities/general/positionEncodings").and_then(Value::as_array);
                if self.cfg.utf8 && offered.is_some_and(|a| a.iter().any(|e| e == "utf-8")) {
                    self.enc = Encoding::Utf8;
                }
                if let Some(root) = p.get("rootUri").and_then(Value::as_str).and_then(uri_to_path) {
                    self.root = root;
                }
                self.load_root();
                let mut caps = json!({
                    "positionEncoding": if self.enc == Encoding::Utf8 { "utf-8" } else { "utf-16" },
                    "textDocumentSync": {"openClose": true, "change": 1, "save": {"includeText": false}},
                    "workspaceSymbolProvider": true,
                    "documentSymbolProvider": true,
                    "definitionProvider": true,
                    "referencesProvider": true,
                    "hoverProvider": true,
                    "renameProvider": {"prepareProvider": true},
                });
                if self.cfg.pull {
                    caps["diagnosticProvider"] = json!({"interFileDependencies": false, "workspaceDiagnostics": false});
                }
                if let Some(after) = self.cfg.status_after {
                    let out = self.out.clone();
                    let _ = out.send(Notification::new("experimental/serverStatus".into(), json!({"health": "ok", "quiescent": false})).into());
                    std::thread::spawn(move || {
                        std::thread::sleep(after);
                        let _ = out.send(Notification::new("experimental/serverStatus".into(), json!({"health": "ok", "quiescent": true})).into());
                    });
                }
                ok(json!({"capabilities": caps, "serverInfo": {"name": "fake-lsp"}}))
            }
            "workspace/symbol" => {
                let q = p.get("query").and_then(Value::as_str).unwrap_or("").to_lowercase();
                let mut out = Vec::new();
                for (path, text) in self.files.values() {
                    let mut at = 0;
                    for line in text.split_inclusive('\n') {
                        if let Some((n, off)) = def_on_line(line) {
                            if n.to_lowercase().contains(&q) {
                                out.push(json!({
                                    "name": n,
                                    "kind": Self::symbol_kind(line),
                                    "location": {"uri": path_to_uri(path), "range": self.range(text, at + off, at + off + n.len())},
                                }));
                            }
                        }
                        at += line.len();
                    }
                }
                ok(Value::Array(out))
            }
            "textDocument/documentSymbol" => {
                let Some(path) = p.pointer("/textDocument/uri").and_then(Value::as_str).and_then(uri_to_path) else {
                    return ok(json!([]));
                };
                let Some((_, text)) = self.files.get(&norm_key(&path)) else { return ok(json!([])) };
                let mut out = Vec::new();
                let mut at = 0;
                for line in text.split_inclusive('\n') {
                    if let Some((n, off)) = def_on_line(line) {
                        out.push(json!({
                            "name": n,
                            "kind": Self::symbol_kind(line),
                            "location": {"uri": path_to_uri(&path), "range": self.range(text, at + off, at + off + n.len())},
                        }));
                    }
                    at += line.len();
                }
                ok(Value::Array(out))
            }
            "textDocument/definition" => {
                let Some(name) = self.name_at(p) else { return ok(Value::Null) };
                let locs: Vec<Value> = self.defs(&name).iter().map(|(path, s, e, _)| self.location(path, *s, *e)).collect();
                ok(Value::Array(locs))
            }
            "textDocument/references" => {
                let Some(name) = self.name_at(p) else { return ok(Value::Null) };
                let include_decl = p.pointer("/context/includeDeclaration").and_then(Value::as_bool).unwrap_or(true);
                let def_spots: Vec<(String, usize)> = self.defs(&name).iter().map(|(p, s, ..)| (norm_key(p), *s)).collect();
                let mut locs = Vec::new();
                for (key, (path, text)) in &self.files {
                    for off in occurrences(text, &name) {
                        if !include_decl && def_spots.contains(&(key.clone(), off)) {
                            continue;
                        }
                        locs.push(json!({"uri": path_to_uri(path), "range": self.range(text, off, off + name.len())}));
                    }
                }
                ok(Value::Array(locs))
            }
            "textDocument/hover" => {
                let Some(name) = self.name_at(p) else { return ok(Value::Null) };
                match self.defs(&name).first() {
                    Some((.., line)) => ok(json!({"contents": {"kind": "markdown", "value": format!("```\n{line}\n```\nfake docs for {name}")}})),
                    None => ok(Value::Null),
                }
            }
            "textDocument/prepareRename" => {
                let Some((_, text, off)) = self.at(p) else { return ok(Value::Null) };
                match word_at(&text, off) {
                    Some((s, e)) if !self.defs(&text[s..e]).is_empty() => ok(self.range(&text, s, e)),
                    _ => ok(Value::Null),
                }
            }
            "textDocument/rename" => {
                let Some(name) = self.name_at(p) else { return ok(Value::Null) };
                let new_name = p.get("newName").and_then(Value::as_str).unwrap_or("").to_string();
                if self.defs(&name).is_empty() {
                    return Response::new_err(r.id.clone(), -32602, format!("fake: no symbol {name}"));
                }
                let mut changes = serde_json::Map::new();
                let mut doc_changes = Vec::new();
                for (key, (path, text)) in &self.files {
                    let edits: Vec<Value> = occurrences(text, &name)
                        .into_iter()
                        .map(|off| json!({"range": self.range(text, off, off + name.len()), "newText": new_name}))
                        .collect();
                    if edits.is_empty() {
                        continue;
                    }
                    let uri = path_to_uri(path);
                    doc_changes.push(json!({
                        "textDocument": {"uri": uri, "version": self.versions.get(key).copied()},
                        "edits": edits,
                    }));
                    changes.insert(uri, Value::Array(edits));
                }
                if self.cfg.document_changes {
                    ok(json!({"documentChanges": doc_changes}))
                } else {
                    ok(json!({"changes": changes}))
                }
            }
            "textDocument/diagnostic" => {
                let Some(path) = p.pointer("/textDocument/uri").and_then(Value::as_str).and_then(uri_to_path) else {
                    return ok(json!({"kind": "full", "items": []}));
                };
                ok(json!({"kind": "full", "items": self.diagnostics_for(&path)}))
            }
            _ => Response::new_err(r.id.clone(), -32601, format!("fake: no {}", r.method)),
        }
    }

    fn diagnostics_for(&self, path: &Path) -> Vec<Value> {
        let Some((_, text)) = self.files.get(&norm_key(path)) else { return Vec::new() };
        let mut out = Vec::new();
        let mut at = 0;
        for line in text.split_inclusive('\n') {
            for (marker, severity) in [("ERROR:", 1), ("WARN:", 2)] {
                if let Some(i) = line.find(marker) {
                    let msg = line[i + marker.len()..].trim();
                    out.push(json!({
                        "range": self.range(text, at + i, at + line.trim_end().len()),
                        "severity": severity,
                        "source": "fake",
                        "code": "F1",
                        "message": msg,
                    }));
                }
            }
            at += line.len();
        }
        out
    }

    fn publish(&self, path: &Path) {
        let key = norm_key(path);
        let params = json!({
            "uri": path_to_uri(path),
            "version": self.versions.get(&key).copied(),
            "diagnostics": self.diagnostics_for(path),
        });
        let out = self.out.clone();
        let delay = self.cfg.publish_delay;
        std::thread::spawn(move || {
            std::thread::sleep(delay);
            let _ = out.send(Notification::new("textDocument/publishDiagnostics".into(), params).into());
        });
    }

    fn notification(&mut self, n: Notification) {
        let p = &n.params;
        let Some(path) = p.pointer("/textDocument/uri").and_then(Value::as_str).and_then(uri_to_path) else { return };
        let key = norm_key(&path);
        match n.method.as_str() {
            "textDocument/didOpen" => {
                let text = p.pointer("/textDocument/text").and_then(Value::as_str).unwrap_or("").to_string();
                let version = p.pointer("/textDocument/version").and_then(Value::as_i64).unwrap_or(0);
                self.files.insert(key.clone(), (path.clone(), text));
                self.versions.insert(key, version);
                self.publish(&path);
            }
            "textDocument/didChange" => {
                if let Some(text) = p.pointer("/contentChanges/0/text").and_then(Value::as_str) {
                    self.files.insert(key.clone(), (path.clone(), text.to_string()));
                }
                if let Some(v) = p.pointer("/textDocument/version").and_then(Value::as_i64) {
                    self.versions.insert(key, v);
                }
                self.publish(&path);
            }
            "textDocument/didClose" => {
                self.versions.remove(&key);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fake_finds_definitions_and_words() {
        assert_eq!(def_on_line("pub fn start(&self) {"), Some(("start", 7)));
        assert_eq!(def_on_line("    def open(self):"), Some(("open", 8)));
        assert_eq!(def_on_line("let x = 1;"), None);
        assert_eq!(occurrences("start(); restart(); start_x; start", "start"), vec![0, 29]);
        assert_eq!(word_at("a.start()", 4), Some((2, 7)));
        assert_eq!(word_at("a + b", 2), None);
    }
}
