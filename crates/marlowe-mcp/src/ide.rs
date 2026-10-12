//! The editor as an MCP server (track H2, rows U14 and U18).
//!
//! The VS Code extension (`ide/vscode`) serves MCP on `127.0.0.1` and writes a connection file
//! `<home>\ide\<port>.lock` with the URL, a random bearer token and the folders the window has
//! open. [`discover`] turns the lock of the window that has the session's workspace open into a
//! session-scope server named `ide`, so its tools reach the model as `mcp__ide__get_context`,
//! `mcp__ide__get_diagnostics`, `mcp__ide__open_diff`, `mcp__ide__execute_code` and so on, with
//! the same output cap, untrusted-content and pinning rules as any server.
//!
//! What the lock file may and may not say:
//! - The URL must be `http://127.0.0.1:<port>/...`. Anything else is ignored, so a lock cannot
//!   point Marlowe (or its token) at another host.
//! - The lock's `consequences` are not used. [`consequence_of`] is the table, and a tool it does
//!   not know loads as `irreversible`, so a lock cannot make a tool quieter than Marlowe decided.
//!   Reads and showing something in the editor are free; `execute_code` asks every time.
//! - A lock whose port does not answer is skipped (the editor died without removing it).
//! - Only a window that has the workspace itself (or a folder containing it) open is adopted. A
//!   window that has some folder below the workspace open is not: started from the home directory
//!   or a drive root, Marlowe would otherwise take in any unrelated window and hand its tabs and
//!   selection to the model.
//!
//! `MARLOWE_NO_IDE=1` turns discovery off; `--strict-mcp-config` already does. [`status`] is the
//! text behind `/ide` and the `editor` line of `marlowe doctor`.

use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use marlowe_tools::ConsequenceLevel;
use serde_json::{json, Value};

use crate::config::{self, Scope, ServerConfig};
use crate::hub::Hub;
use crate::util::Url;

/// The name the editor's server goes by; its tools are `mcp__ide__<tool>`.
pub const SERVER_NAME: &str = "ide";

/// Set to `1` to ignore every connection file.
pub const DISABLE_ENV: &str = "MARLOWE_NO_IDE";

/// The lock-file format this build reads.
pub const LOCK_VERSION: u64 = 1;

/// `execute_code` may run up to 900 s and a waited `open_diff` up to the same; the call timeout
/// leaves room for the round trip.
const TOOL_TIMEOUT: Duration = Duration::from_secs(960);

/// How long to wait for the editor's port to accept a connection.
const PROBE: Duration = Duration::from_millis(300);

/// The consequence each editor tool loads with. Unknown tools are `irreversible`.
pub fn consequence_of(tool: &str) -> ConsequenceLevel {
    match tool {
        "get_context" | "get_diagnostics" | "close_diffs" => ConsequenceLevel::Inert,
        "open_file" | "open_diff" => ConsequenceLevel::Reversible,
        _ => ConsequenceLevel::Irreversible,
    }
}

const TOOLS: [&str; 6] = ["get_context", "get_diagnostics", "open_file", "open_diff", "close_diffs", "execute_code"];

/// One connection file, validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lock {
    pub pid: u64,
    pub ide_name: String,
    pub url: String,
    pub port: u16,
    pub token: String,
    pub folders: Vec<String>,
    /// RFC 3339 start time; sorts as text.
    pub started: String,
}

/// Parses and validates a lock file's text.
pub fn parse_lock(text: &str) -> Result<Lock, String> {
    let v: Value = serde_json::from_str(text).map_err(|e| format!("not valid JSON: {e}"))?;
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    let version = v.get("version").and_then(Value::as_u64).unwrap_or(0);
    if version != LOCK_VERSION {
        return Err(format!("lock version {version}, this build reads {LOCK_VERSION}"));
    }
    if s("transport").as_deref() != Some("http") {
        return Err("transport is not http".into());
    }
    let url = s("url").ok_or("no url")?;
    let parsed = Url::parse(&url)?;
    if parsed.https || parsed.host != "127.0.0.1" {
        return Err(format!("{url} is not a loopback http address"));
    }
    let token = s("authToken").filter(|t| !t.is_empty() && t.bytes().all(|b| b.is_ascii_graphic())).ok_or("no usable authToken")?;
    let folders = match v.get("workspaceFolders") {
        Some(Value::Array(a)) => a.iter().filter_map(|f| f.as_str().map(str::to_string)).collect(),
        _ => Vec::new(),
    };
    Ok(Lock {
        pid: v.get("pid").and_then(Value::as_u64).unwrap_or(0),
        ide_name: s("ideName").unwrap_or_else(|| "editor".into()),
        port: parsed.port,
        url,
        token,
        folders,
        started: s("started").unwrap_or_default(),
    })
}

/// A path as it compares: separators unified, trailing separator gone, case folded on Windows.
fn fold(p: &str) -> String {
    let mut s = p.replace('\\', "/");
    while s.len() > 1 && s.ends_with('/') {
        s.pop();
    }
    if cfg!(windows) {
        s.make_ascii_lowercase();
    }
    s
}

fn inside(root: &str, child: &str) -> bool {
    let (r, c) = (fold(root), fold(child));
    c == r || c.strip_prefix(r.as_str()).is_some_and(|rest| rest.starts_with('/') || r.ends_with('/'))
}

/// Whether `workspace` is one of the window's folders or inside one. A window whose folder is
/// merely somewhere below the workspace does not count: started from a parent such as the home
/// directory or a drive root, Marlowe would otherwise adopt any unrelated window under it and
/// send its tabs to the model.
fn has_open(lock: &Lock, workspace: &Path) -> bool {
    let ws = workspace.display().to_string();
    lock.folders.iter().any(|f| inside(f, &ws))
}

fn reachable(port: u16) -> bool {
    TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], port)), PROBE).is_ok()
}

/// The server entry for a lock.
pub fn entry(lock: &Lock) -> Value {
    let consequences: serde_json::Map<String, Value> = TOOLS.iter().map(|t| ((*t).to_string(), json!(consequence_of(t)))).collect();
    json!({
        "type": "http",
        "url": lock.url,
        "headers": {"Authorization": format!("Bearer {}", lock.token)},
        "consequence": "irreversible",
        "consequences": consequences,
        "timeout": TOOL_TIMEOUT.as_millis() as u64,
    })
}

/// Every running editor under `<home>\ide` (a lock that parses and whose port answers), newest
/// first.
pub fn editors(home: &Path) -> Vec<Lock> {
    let Ok(dir) = std::fs::read_dir(home.join("ide")) else { return Vec::new() };
    let mut found: Vec<Lock> = Vec::new();
    for e in dir.flatten() {
        let path = e.path();
        if path.extension().and_then(|x| x.to_str()) != Some("lock") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let Ok(lock) = parse_lock(&text) else { continue };
        if reachable(lock.port) {
            found.push(lock);
        }
    }
    found.sort_by(|a, b| b.started.cmp(&a.started));
    found
}

/// The editor window that has `workspace` open: the newest one among those containing it.
pub fn matching(home: &Path, workspace: &Path) -> Option<Lock> {
    editors(home).into_iter().find(|l| has_open(l, workspace))
}

/// The editor server for `workspace`, if a window that has it open is running. Empty when there
/// is no workspace, no lock, or discovery is off.
pub fn discover(home: &Path, workspace: Option<&Path>) -> Vec<ServerConfig> {
    if std::env::var(DISABLE_ENV).is_ok_and(|v| v.trim() == "1") {
        return Vec::new();
    }
    let Some(workspace) = workspace else { return Vec::new() };
    matching(home, workspace).and_then(|lock| config::parse_entry(SERVER_NAME, Scope::Session, &entry(&lock)).ok()).into_iter().collect()
}

/// What `/ide` and `marlowe doctor` say about the editor connection for `workspace`: which window
/// is connected and what it has open, or why there is none.
pub fn status(home: &Path, workspace: Option<&Path>) -> String {
    if std::env::var(DISABLE_ENV).is_ok_and(|v| v.trim() == "1") {
        return format!("off ({DISABLE_ENV}=1)");
    }
    let running = editors(home);
    if let Some(l) = workspace.and_then(|w| running.iter().find(|l| has_open(l, w))) {
        return format!("{} connected as `{SERVER_NAME}` ({}); it has {} open", l.ide_name, l.url, l.folders.join(", "));
    }
    match (running.len(), workspace) {
        (0, _) => "no editor running; install the Marlowe VS Code extension (ide/vscode) and open this folder in it".into(),
        (n, Some(w)) => format!("{n} editor window(s) running, none has {} open", w.display()),
        (n, None) => format!("{n} editor window(s) running; no workspace to match"),
    }
}

// ---------------------------------------------------------------- what the user is looking at

/// Selected text longer than this is cut when it is put in front of the model.
const SELECTION_CHARS: usize = 4_000;
/// Other open tabs named in the note.
const TABS_NAMED: usize = 8;
/// The longest the start of a turn waits for the editor to answer `get_context`.
pub const CONTEXT_WAIT: Duration = Duration::from_millis(1_500);

/// What every context note starts with; the daemon looks for it in the history to know the
/// current note is already there.
pub const NOTE_PREFIX: &str = "Editor context from the user's editor";

/// The exposed name of the context tool.
pub const CONTEXT_TOOL: &str = "mcp__ide__get_context";

/// A fence longer than any run of backticks in `text`.
fn fence_for(text: &str) -> String {
    let mut longest = 0;
    let mut run = 0;
    for c in text.chars() {
        run = if c == '`' { run + 1 } else { 0 };
        longest = longest.max(run);
    }
    "`".repeat((longest + 1).max(3))
}

/// The note for the model from `get_context`'s answer: the file the user is in and what they
/// selected, and the other tabs they have open. `None` when there is nothing to say.
pub fn render_context(answer: &str) -> Option<String> {
    let v: Value = serde_json::from_str(answer).ok()?;
    let mut lines: Vec<String> = Vec::new();
    let mut focused: Option<String> = None;
    if let Some(sel) = v.get("selection").filter(|s| s.is_object()) {
        let path = sel.get("path").and_then(Value::as_str)?;
        let start = sel.get("startLine").and_then(Value::as_u64).unwrap_or(1);
        let end = sel.get("endLine").and_then(Value::as_u64).unwrap_or(start);
        let text = sel.get("text").and_then(Value::as_str).unwrap_or("");
        focused = Some(path.to_string());
        if sel.get("empty").and_then(Value::as_bool).unwrap_or(text.is_empty()) || text.trim().is_empty() {
            lines.push(format!("They are in `{path}`, cursor on line {start}."));
        } else {
            let cut: String = text.chars().take(SELECTION_CHARS).collect();
            let cut_short = cut.chars().count() < text.chars().count() || sel.get("truncated").and_then(Value::as_bool) == Some(true);
            let more = if cut_short { "\n[selection cut]" } else { "" };
            let fence = fence_for(&cut);
            let lang = sel.get("language").and_then(Value::as_str).unwrap_or("");
            let span = if end > start { format!("{start}-{end}") } else { start.to_string() };
            lines.push(format!("They have `{path}` open with line {span} selected:\n{fence}{lang}\n{cut}\n{fence}{more}"));
        }
    }
    let others: Vec<String> = v
        .get("openEditors")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|e| e.get("path").and_then(Value::as_str))
                .filter(|p| focused.as_deref().is_none_or(|f| fold(f) != fold(p)))
                .take(TABS_NAMED)
                .map(|p| format!("`{p}`"))
                .collect()
        })
        .unwrap_or_default();
    if !others.is_empty() {
        lines.push(format!("Other open tabs: {}.", others.join(", ")));
    }
    if lines.is_empty() {
        return None;
    }
    Some(format!("{NOTE_PREFIX} (not typed by them; use it if the request refers to \"this\" or \"here\"):\n{}", lines.join("\n")))
}

/// Asks the connected editor what the user is looking at, waiting at most `wait`. `None` when no
/// editor is connected, it does not answer in time (the call is cancelled), or there is nothing
/// to report. Never blocks longer than `wait`.
pub fn context_note(hub: &Arc<Hub>, wait: Duration) -> Option<String> {
    hub.find(CONTEXT_TOOL)?;
    let (tx, rx) = crossbeam_channel::bounded(1);
    let stop = Arc::new(AtomicBool::new(false));
    let (h, flag) = (hub.clone(), stop.clone());
    std::thread::Builder::new()
        .name("mcp-ide-context".into())
        .spawn(move || {
            let r = h.call(CONTEXT_TOOL, json!({}), &|| flag.load(Ordering::Relaxed));
            let _ = tx.send(r.ok().filter(|o| !o.is_error).map(|o| o.text));
        })
        .ok()?;
    let answer = rx.recv_timeout(wait);
    stop.store(true, Ordering::Relaxed);
    render_context(&answer.ok()??)
}

#[cfg(test)]
mod tests;
