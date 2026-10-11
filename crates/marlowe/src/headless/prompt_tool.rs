//! `--permission-prompt-tool mcp__server__tool`: when nothing allowed or denied a call, the run
//! asks this MCP tool, as `claude -p` does. The tool gets `{tool_name, input}` (Claude Code's tool
//! names) and answers a JSON text, `{"behavior":"allow"}` or `{"behavior":"deny","message":".."}`.
//!
//! The client process runs its own MCP hub for it (the same `marlowe-mcp` the daemon uses), from
//! the run's `--mcp-config` entries and the user's and workspace's configuration.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use marlowe_mcp::client::ConnectOpts;
use marlowe_mcp::config::{self, Paths};
use marlowe_mcp::hub::{Hub, HubOpts};
use marlowe_mcp::oauth::{KeyringTokens, TokenStore};
use serde_json::{json, Value};

use super::policy::cc_name;

const CONNECT_WAIT: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    Allow,
    Deny(String),
}

pub struct PromptTool {
    hub: Arc<Hub>,
    exposed: String,
}

impl PromptTool {
    /// Connects the servers and checks that the tool exists.
    pub fn connect(exposed: &str, home: &Path, workspace: Option<&Path>, session_servers: Vec<(String, Value)>, strict: bool) -> Result<Self, String> {
        Self::connect_with(exposed, home, workspace, session_servers, strict, Arc::new(KeyringTokens::new()))
    }

    pub fn connect_with(
        exposed: &str,
        home: &Path,
        workspace: Option<&Path>,
        session_servers: Vec<(String, Value)>,
        strict: bool,
        tokens: Arc<dyn TokenStore>,
    ) -> Result<Self, String> {
        let session = session_servers
            .iter()
            .map(|(n, e)| config::parse_entry(n, config::Scope::Session, e))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("--mcp-config: {e}"))?;
        let mut connect = ConnectOpts::new(tokens);
        connect.cwd = workspace.map(Path::to_path_buf);
        connect.roots = workspace.map(|w| vec![w.to_path_buf()]).unwrap_or_default();
        let mut opts = HubOpts::new(Paths::new(home, workspace), connect);
        opts.session = session;
        opts.strict = strict;
        let hub = Hub::start(opts);
        hub.wait_settled(CONNECT_WAIT);
        if hub.find(exposed).is_none() {
            let status: Vec<String> = hub.statuses().iter().map(|s| format!("{}: {}", s.name, s.status.label())).collect();
            return Err(format!(
                "--permission-prompt-tool {exposed}: that MCP tool is not available ({}); servers: {}",
                "name it mcp__<server>__<tool> and configure the server with --mcp-config or .mcp.json",
                if status.is_empty() { "none configured".to_string() } else { status.join("; ") }
            ));
        }
        Ok(Self { hub, exposed: exposed.to_string() })
    }

    /// Asks the tool about one call. Any failure to get a clear answer is an error, which the
    /// caller treats as a denial.
    pub fn ask(&self, tool: &str, input: &Value, cancelled: &dyn Fn() -> bool) -> Result<Decision, String> {
        let args = json!({"tool_name": cc_name(tool), "input": input, "marlowe_tool": tool});
        let out = self.hub.call(&self.exposed, args, cancelled).map_err(|e| e.to_string())?;
        if out.is_error {
            return Err(format!("{} reported an error: {}", self.exposed, out.text));
        }
        parse_decision(&out.text, input)
    }
}

/// The reason to refuse an `allow` whose `updatedInput` asks for a different call than the one
/// that was judged. Claude Code runs the rewritten input; here the call cannot be changed after
/// the daemon has built it (until the protocol can carry rewritten arguments), so running the
/// original while the client believes it approved something else is worse than a refusal.
pub fn rewritten(answer: &Value, original: &Value) -> Option<String> {
    match answer.get("updatedInput") {
        Some(u) if !u.is_null() && u != original => Some("updatedInput is not supported yet: the call cannot be changed after it was proposed, so it is refused rather than run unchanged".to_string()),
        _ => None,
    }
}

/// The tool's answer: JSON text with `behavior` allow or deny. `original` is the call's input.
pub fn parse_decision(text: &str, original: &Value) -> Result<Decision, String> {
    let v: Value = serde_json::from_str(text.trim()).map_err(|_| format!("the permission prompt tool did not answer JSON: {}", text.chars().take(120).collect::<String>()))?;
    match v.get("behavior").and_then(Value::as_str) {
        Some("allow") => Ok(rewritten(&v, original).map_or(Decision::Allow, Decision::Deny)),
        Some("deny") => Ok(Decision::Deny(v.get("message").and_then(Value::as_str).unwrap_or("denied by the permission prompt tool").to_string())),
        _ => Err("the permission prompt tool's answer has no behavior of allow or deny".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decisions_parse_allow_deny_and_refuse_anything_else() {
        let input = serde_json::json!({"command": "ls"});
        assert_eq!(parse_decision(r#"{"behavior":"allow","updatedInput":{"command":"ls"}}"#, &input).unwrap(), Decision::Allow);
        assert_eq!(parse_decision(r#"{"behavior":"allow"}"#, &input).unwrap(), Decision::Allow);
        assert_eq!(parse_decision(r#" {"behavior":"deny","message":"no rm"} "#, &input).unwrap(), Decision::Deny("no rm".into()));
        assert_eq!(parse_decision(r#"{"behavior":"deny"}"#, &input).unwrap(), Decision::Deny("denied by the permission prompt tool".into()));
        assert!(parse_decision("yes", &input).is_err());
        assert!(parse_decision(r#"{"behavior":"maybe"}"#, &input).is_err());
        assert!(parse_decision("{}", &input).is_err());
    }

    #[test]
    fn an_allow_that_rewrites_the_call_is_refused_not_run_unchanged() {
        let input = serde_json::json!({"command": "rm -rf build"});
        let d = parse_decision(r#"{"behavior":"allow","updatedInput":{"command":"rm -rf build/tmp"}}"#, &input).unwrap();
        assert!(matches!(&d, Decision::Deny(m) if m.contains("updatedInput is not supported yet")), "{d:?}");
        // Dropping a field is a rewrite as well.
        assert!(matches!(parse_decision(r#"{"behavior":"allow","updatedInput":{}}"#, &input).unwrap(), Decision::Deny(_)));
        // A null is no rewrite.
        assert_eq!(parse_decision(r#"{"behavior":"allow","updatedInput":null}"#, &input).unwrap(), Decision::Allow);
    }

    #[test]
    fn a_missing_tool_names_the_servers_it_looked_at() {
        let home = std::env::temp_dir().join(format!("marlowe-hl-pt-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        let e = PromptTool::connect("mcp__nope__approve", &home, None, vec![], true).err().unwrap();
        assert!(e.contains("mcp__nope__approve") && e.contains("none configured"), "{e}");
    }
}
