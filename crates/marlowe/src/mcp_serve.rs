//! `marlowe mcp serve` (design X21, Claude Code's `claude mcp serve`): Marlowe's own tools for
//! another MCP client, over stdio. The workspace is the current directory's repository. Nothing
//! is asked interactively: the session runs in `dont_ask`, so a call runs only when the
//! permission engine lets it through on its own (a read, or an allow rule in the settings
//! layers) and is otherwise refused with the reason. Tripwires and hard denies hold as always,
//! and after a call that brought in untrusted content, side effects are refused too.
//!
//! The client is another agent: what it sends is agent-trust input. It never reaches memory (no
//! tool served here writes to it), and every call is recorded in
//! `<home>\mcp-serve\<session>.jsonl` (tool, verdict, outcome summary; no arguments).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use marlowe_agent::approval::{Gate, Permissions};
use marlowe_agent::{CallCtx, CallFlow, Exposure, SessionScope, ToolCtx, ToolGroup, ToolHost, ToolMiddleware};
use marlowe_mcp::server::{self, ServeTools, ServedResult, ServedTool, ServerInfo};
use marlowe_model::{CancelToken, ToolCall};
use marlowe_proto::{PermissionMode, SessionId};
use serde_json::{json, Value};

/// Tool groups served. The rest need a conductor (subagents, workflows, tasks, notifications,
/// the crew), the memory service or an MCP hub of their own.
const SERVED_GROUPS: [ToolGroup; 7] =
    [ToolGroup::Files, ToolGroup::Search, ToolGroup::Shell, ToolGroup::Web, ToolGroup::Code, ToolGroup::Verify, ToolGroup::Git];

/// `marlowe mcp serve`: serves until the client closes stdin.
pub fn run(home: &Path, workspace: PathBuf) -> Result<i32, String> {
    let credentials: Arc<dyn marlowe_brains::CredentialStore> = Arc::new(marlowe_brains::KeyringStore::new());
    let session = SessionId(format!("mcp-client-{}", std::process::id()));
    let factory = crate::wire::tool_factory(credentials, home);
    let host = factory(&marlowe_daemon::HostRequest { workspace: Some(workspace.clone()), model: None, session: Some(session.clone()) });
    let served = Served::new(home, &workspace, host, session);
    let info = ServerInfo {
        name: "marlowe".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        instructions: Some(format!(
            "Marlowe's tools for {}. Calls run without prompts: anything that would need approval is refused \
             with the reason; the user allows more with allow rules in the settings.",
            workspace.display()
        )),
    };
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    server::serve(&info, &served, stdin.lock(), &mut stdout.lock()).map_err(|e| e.to_string())?;
    Ok(0)
}

/// The served tools: a session-less tool host behind the permission engine in `dont_ask`.
pub struct Served {
    host: Arc<dyn ToolHost>,
    perms: Permissions,
    session: SessionId,
    workspace: PathBuf,
    scope: SessionScope,
    middleware: Vec<Arc<dyn ToolMiddleware>>,
    tainted: AtomicBool,
    calls: AtomicU32,
    log: Mutex<Option<std::fs::File>>,
}

impl Served {
    pub fn new(home: &Path, workspace: &Path, host: Arc<dyn ToolHost>, session: SessionId) -> Self {
        let perms = Permissions::new(home, Some(workspace), None);
        perms.set_mode(PermissionMode::DontAsk);
        let dir = home.join("mcp-serve");
        let log = std::fs::create_dir_all(&dir)
            .ok()
            .and_then(|_| std::fs::OpenOptions::new().create(true).append(true).open(dir.join(format!("{}.jsonl", session.0))).ok());
        let middleware = host.middleware();
        Self {
            host,
            perms,
            session,
            workspace: workspace.to_path_buf(),
            scope: SessionScope::detached(),
            middleware,
            tainted: AtomicBool::new(false),
            calls: AtomicU32::new(0),
            log: Mutex::new(log),
        }
    }

    fn served(&self, name: &str) -> bool {
        name != "tool_search" && self.host.manifest(name).is_some_and(|m| SERVED_GROUPS.contains(&m.group))
    }

    fn record(&self, tool: &str, verdict: &str, summary: &str) {
        if let Some(f) = self.log.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            let line = json!({"at_ms": now_ms(), "session": self.session.0, "trust": "agent", "tool": tool, "verdict": verdict, "summary": summary});
            let _ = writeln!(f, "{line}");
        }
    }

    fn refuse(&self, tool: &str, text: String) -> ServedResult {
        self.record(tool, "denied", &text);
        ServedResult { text, is_error: true }
    }
}

/// How the user lets a refused call through: an allow rule in a settings layer that applies.
fn allow_hint(tool: &str) -> String {
    format!(
        "To allow it, add an allow rule such as [permissions] allow = [\"{tool}(...)\"] to your settings.toml in the \
         Marlowe home, or to the project's .marlowe/settings.toml once you have trusted the project."
    )
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

impl ServeTools for Served {
    fn list(&self) -> Vec<ServedTool> {
        self.host
            .specs()
            .into_iter()
            .filter(|s| self.served(&s.name))
            .map(|s| ServedTool { name: s.name, description: s.description, input_schema: s.input_schema })
            .collect()
    }

    fn call(&self, name: &str, args: Value) -> ServedResult {
        if !self.served(name) {
            return self.refuse(name, format!("`{name}` is not one of the tools this server offers"));
        }
        let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        let mut call = ToolCall { id: format!("mcp{n}"), name: name.to_string(), args };
        let tainted = self.tainted.load(Ordering::SeqCst);
        let cx = CallCtx::new(ToolCtx::new(self.session.clone(), n, Some(self.workspace.clone()), CancelToken::new(), tainted), self.scope.clone());
        for m in &self.middleware {
            if let CallFlow::Done(o) = m.before_approval(&mut call, &cx) {
                return self.refuse(name, o.content);
            }
        }
        let exposure = Exposure { tainted, ..Exposure::default() };
        match self.perms.decide(self.host.as_ref(), &call, &exposure, None) {
            Gate::Free => {}
            Gate::Deny(reason) if reason.contains("dont_ask") => {
                return self.refuse(name, format!("refused: {reason}. {}", allow_hint(name)))
            }
            Gate::Deny(reason) => return self.refuse(name, format!("refused: {reason}")),
            Gate::Ask { summary, .. } => {
                return self.refuse(
                    name,
                    format!("refused: {summary} needs approval, and `marlowe mcp serve` runs without prompts. {}", allow_hint(name)),
                )
            }
        }
        for m in &self.middleware {
            if let CallFlow::Done(o) = m.before_call(&call, &cx) {
                return self.refuse(name, o.content);
            }
        }
        let mut outcome = self.host.call_scoped(&call, &cx);
        for m in &self.middleware {
            m.after_call(&call, &cx, &mut outcome);
        }
        if self.host.taints(&call) {
            self.tainted.store(true, Ordering::SeqCst);
        }
        self.record(name, if outcome.is_error { "error" } else { "ran" }, &outcome.summary);
        ServedResult { text: outcome.content, is_error: outcome.is_error }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("marlowe-mcp-serve-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn rpc(served: &Served, lines: &[Value]) -> Vec<Value> {
        let input: String = lines.iter().map(|l| format!("{l}\n")).collect();
        let info = ServerInfo { name: "marlowe".into(), version: "test".into(), instructions: None };
        let mut out = Vec::new();
        server::serve(&info, served, input.as_bytes(), &mut out).unwrap();
        String::from_utf8(out).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
    }

    fn served(home: &Path, ws: &Path) -> Served {
        let host: Arc<dyn ToolHost> = Arc::new(marlowe_exec::BuiltinTools::new(Some(ws.to_path_buf())));
        Served::new(home, ws, host, SessionId("mcp-client-test".into()))
    }

    #[test]
    fn parity_x21_a_client_lists_the_tools_reads_a_file_and_is_refused_a_shell_without_a_rule() {
        let root = temp("x21");
        let (home, ws) = (root.join("home"), root.join("repo"));
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join("notes.txt"), "served by marlowe\n").unwrap();
        let s = served(&home, &ws);
        let replies = rpc(
            &s,
            &[
                json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}}}),
                json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
                json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
                json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "read", "arguments": {"path": "notes.txt"}}}),
                json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "bash", "arguments": {"command": "cargo publish"}}}),
                json!({"jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": {"name": "agent", "arguments": {"prompt": "x"}}}),
            ],
        );
        assert_eq!(replies[0]["result"]["serverInfo"]["name"], "marlowe");
        let names: Vec<&str> = replies[1]["result"]["tools"].as_array().unwrap().iter().filter_map(|t| t["name"].as_str()).collect();
        for want in ["read", "write", "edit", "grep", "glob", "bash", "web_fetch"] {
            assert!(names.contains(&want), "{want} missing from {names:?}");
        }
        assert!(!names.contains(&"agent") && !names.contains(&"workflow"), "conductor-only tools are not served: {names:?}");
        let read = &replies[2]["result"];
        assert_eq!(read["isError"], false, "{read}");
        assert!(read["content"][0]["text"].as_str().unwrap().contains("served by marlowe"), "{read}");
        let bash = &replies[3]["result"];
        assert_eq!(bash["isError"], true, "{bash}");
        let why = bash["content"][0]["text"].as_str().unwrap();
        assert!(why.starts_with("refused:") && why.contains("allow rule"), "{why}");
        assert_eq!(replies[4]["result"]["isError"], true);
        // Every call is on the record, at agent trust, without its arguments.
        let log = std::fs::read_to_string(home.join("mcp-serve").join("mcp-client-test.jsonl")).unwrap();
        assert_eq!(log.lines().count(), 3, "{log}");
        assert!(log.contains("\"trust\":\"agent\"") && log.contains("\"verdict\":\"denied\"") && !log.contains("cargo publish"), "{log}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_allow_rule_in_the_workspace_lets_a_shell_call_run() {
        let root = temp("x21-rule");
        let (home, ws) = (root.join("home"), root.join("repo"));
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        // The user's own layer: a project's allow rules apply only once the project is trusted.
        let refused = served(&root.join("bare-home"), &ws).call("bash", json!({"command": "mkdir made-by-client"}));
        assert!(refused.is_error && refused.text.starts_with("refused:"), "{}", refused.text);
        assert!(!ws.join("made-by-client").exists());
        std::fs::write(home.join("settings.toml"), "[permissions]\nallow = [\"bash(mkdir:*)\"]\n").unwrap();
        let s = served(&home, &ws);
        let r = s.call("bash", json!({"command": "mkdir made-by-client"}));
        assert!(!r.is_error, "{}", r.text);
        assert!(ws.join("made-by-client").is_dir());
        let _ = std::fs::remove_dir_all(&root);
    }
}
