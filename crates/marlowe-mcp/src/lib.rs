//! **MCP over stdio.** ADR-052.
//!
//! # Stdio, not HTTP, and the reason is layer 4
//!
//! MCP has two standard transports. This crate implements the **stdio** one: a child process,
//! JSON-RPC over its stdin and stdout, one message per line.
//!
//! The HTTP transport is deliberately not implemented, and the reason has NARROWED rather than
//! gone away. This paragraph used to rest on layer 4 being unshipped — *"`EgressPolicy::grant()`
//! still has no production call site"* — and as of 2026-08-29 it has one, so that half is retired.
//! What stands is the half that was always the argument: putting a remote server's bytes on the
//! wire would add a second production egress path, arriving as a side effect of a
//! skills-and-tools session, and `EgressPolicy` is consulted only on parameters typed `Url` —
//! which an MCP transport does not declare, so it would be adjudicated by nothing at all. That is
//! ADR-049 §4's `bash` hole in a new place, and it is not a thing to do quietly.
//!
//! **The dependency list is the enforcement.** This crate does not depend on `marlowe-net`, so it
//! has no TLS and no socket, and `cargo tree -p marlowe-mcp` says so without anyone believing a
//! comment. `no_socket_reaches_this_crate` asserts it from the manifest.
//!
//! A local child process is not a bypass of layer 4 dressed up: the user named the command, the
//! user's own shell could run it, and nothing here decides a destination. What that process does
//! with its own network access is outside every boundary this project has, which is true of
//! `bash` as well and is recorded in ADR-049 §4 rather than implied.
//!
//! # The trust position — ADR-052, and it is a human decision
//!
//! **The server is trusted. Its output is not.**
//!
//! A user installing an MCP server is making an authorization decision: they chose it, added it,
//! and inspecting what they install is their responsibility — the standing every other harness
//! gives an installed server. The agent never adds one on its own initiative, so no path exists by
//! which untrusted content chooses a server.
//!
//! So a tool *description* from a server is ordinary trusted prose and reaches the model as a
//! description. What the tool *returns* is [`TrustClass::UntrustedContent`], exactly as `read`'s
//! file contents and `web`'s pages are, because trust attaches to who wrote the tool and never to
//! what the tool hands back at runtime. `marlowe-contract`'s own definition of
//! `UntrustedContent` has said *"MCP server output"* since it was written.
//!
//! That is not a carve-out bolted onto the decision; it is the existing rule, unchanged.
//!
//! # Consequences declared by the user, defaulting to the maximum
//!
//! An MCP tool's schema says what arguments it takes. It does not say whether calling it moves
//! money. Nothing in the protocol does, and nothing should infer it: [`ServerSpec::consequence`]
//! defaults to `Irreversible`, which `adjudicate` turns into an approval prompt on **every** call,
//! and a user who knows better lowers it in their own config. Absent means maximum — §7.3's rule,
//! applied where the information genuinely is not available.
//!
//! Every parameter loads as [`ArgumentRole::Target`] for the same reason, and that is also the
//! adjudicator's own fail-closed default for an argument no manifest declared.

pub mod deadline;

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use marlowe_tools::{
    load, ArgumentRole, ConsequenceLevel, Description, ManifestProvenance, ParamType, RawManifest,
    RawParamSpec, SummarySpec, ToolId, ToolRegistration, Transport,
};

/// The protocol revision this client speaks.
pub const PROTOCOL_VERSION: &str = "2024-11-05";

/// How long, in milliseconds, to wait for a server to answer one request.
///
/// **A bounded wait, because the alternative is a hung daemon.** A child that never answers would
/// otherwise block the turn forever with no indication why, which is the failure mode the §B5
/// motion rule exists to prevent. The number is generous: a server doing real work on a `tools/call`
/// legitimately takes seconds.
pub const REQUEST_TIMEOUT_MS: u64 = 30_000;

/// A server the user installed.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ServerSpec {
    /// The name this server's tools are namespaced under.
    pub id: String,
    /// The executable. Run directly — **not through a shell** — so nothing in it is word-split,
    /// glob-expanded or interpreted. A server whose command needs a shell says so by naming the
    /// shell as the command.
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// The consequence every tool from this server loads with. Absent is the maximum. See the
    /// module header — nothing in the protocol carries this, so it is the user's to declare.
    #[serde(default)]
    pub consequence: Option<ConsequenceLevel>,
}

#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("mcp server `{id}`: the command `{command}` could not be started: {detail}")]
    Spawn { id: String, command: String, detail: String },

    #[error("mcp server `{id}`: {detail}")]
    Protocol { id: String, detail: String },

    #[error("mcp server `{id}` did not answer `{method}` within {REQUEST_TIMEOUT_MS} ms")]
    Timeout { id: String, method: String },

    #[error("mcp server `{id}` returned an error for `{method}`: {message}")]
    Remote { id: String, method: String, message: String },

    #[error("mcp server `{id}`: tool `{tool}` cannot be registered: {detail}")]
    Registration { id: String, tool: String, detail: String },
}

/// One tool as the server describes it.
#[derive(Debug, Clone)]
pub struct RemoteTool {
    pub name: String,
    pub description: Description,
    /// Parameter names, in the order the schema listed them, with their JSON types.
    pub params: Vec<(String, ParamType)>,
    /// Which of them the schema marks required.
    pub required: Vec<String>,
}

/// A live connection to one server.
pub struct McpClient {
    id: String,
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
    consequence: Option<ConsequenceLevel>,
}

/// Windows creates a console for a console subsystem child unless told not to. An MCP server is
/// a **stdio pipe peer**, not something anyone looks at, so a window per server is one blank
/// console per installed server sitting in the user's taskbar — and one the user can close,
/// killing the server underneath a running session.
///
/// `marlowe/src/tui.rs` already does this for the daemon spawn; this is the same decision one
/// crate over. `stderr` stays `inherit` for the deadlock reason in [`McpClient::connect`] — with
/// no console attached it simply goes nowhere, which is not a pipe and cannot fill.
#[cfg(windows)]
fn detach_console(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    /// `CREATE_NO_WINDOW`, from `winbase.h`. Not pulled from a crate — it is one constant and
    /// `marlowe-mcp` has no other reason to depend on the Windows API surface.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

/// No-op off Windows: every other platform this targets spawns without a console by default.
#[cfg(not(windows))]
fn detach_console(_cmd: &mut Command) {}

impl McpClient {
    /// Spawn the server and complete the MCP handshake.
    ///
    /// `stderr` is inherited rather than piped. A server that logs to stderr and fills a pipe
    /// nobody drains **deadlocks**, and it deadlocks only under load, which is the worst possible
    /// time to discover it.
    ///
    /// On Windows the child is spawned with no console — see [`detach_console`].
    pub fn connect(spec: &ServerSpec) -> Result<Self, McpError> {
        let mut cmd = Command::new(&spec.command);
        cmd.args(&spec.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        detach_console(&mut cmd);
        let mut child = cmd.spawn().map_err(|e| McpError::Spawn {
            id: spec.id.clone(),
            command: spec.command.clone(),
            detail: e.to_string(),
        })?;

        let stdin = child.stdin.take().expect("stdin was piped");
        let stdout = BufReader::new(child.stdout.take().expect("stdout was piped"));

        let mut client = Self {
            id: spec.id.clone(),
            child,
            stdin,
            stdout,
            next_id: 1,
            consequence: spec.consequence,
        };

        client.request(
            "initialize",
            serde_json::json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "marlowe", "version": env!("CARGO_PKG_VERSION") },
            }),
        )?;
        client.notify("notifications/initialized", serde_json::json!({}))?;
        Ok(client)
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    /// The server's tool list, as it reports it **right now**.
    ///
    /// Fetched live on every connect, which is exactly why `marlowe_tools::pin` exists: the text
    /// the user reviewed at install is not necessarily the text in this response.
    pub fn list_tools(&mut self) -> Result<Vec<RemoteTool>, McpError> {
        let reply = self.request("tools/list", serde_json::json!({}))?;
        let tools = reply.get("tools").and_then(|t| t.as_array()).ok_or(McpError::Protocol {
            id: self.id.clone(),
            detail: "`tools/list` returned no `tools` array".into(),
        })?;

        let mut out = Vec::new();
        for t in tools {
            let Some(name) = t.get("name").and_then(|n| n.as_str()) else {
                return Err(McpError::Protocol {
                    id: self.id.clone(),
                    detail: "a tool in `tools/list` has no `name`".into(),
                });
            };
            // **Sanitised and bounded here, at the boundary**, by `Description::new`. See
            // `registry.rs` — trusting the source does not make invisible characters visible, and
            // the user's inspection is what ADR-052 rests on.
            let description =
                Description::new(t.get("description").and_then(|d| d.as_str()).unwrap_or(""));

            let schema = t.get("inputSchema");
            let properties = schema
                .and_then(|s| s.get("properties"))
                .and_then(|p| p.as_object())
                .cloned()
                .unwrap_or_default();
            let params: Vec<(String, ParamType)> = properties
                .iter()
                .map(|(k, v)| (k.clone(), param_type(v.get("type").and_then(|t| t.as_str()))))
                .collect();
            let required: Vec<String> = schema
                .and_then(|s| s.get("required"))
                .and_then(|r| r.as_array())
                .map(|r| r.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                .unwrap_or_default();

            out.push(RemoteTool { name: name.to_string(), description, params, required });
        }
        Ok(out)
    }

    /// Call one tool and return its text content.
    ///
    /// The caller wraps this in `UntrustedContent`. This function does not decide a trust class,
    /// because a function that returned text *and* its own trust class would be a second place
    /// that opinion lives.
    pub fn call_tool(
        &mut self,
        tool: &str,
        arguments: serde_json::Value,
    ) -> Result<(String, bool), McpError> {
        let reply = self
            .request("tools/call", serde_json::json!({ "name": tool, "arguments": arguments }))?;

        let is_error = reply.get("isError").and_then(|e| e.as_bool()).unwrap_or(false);
        let mut text = String::new();
        if let Some(content) = reply.get("content").and_then(|c| c.as_array()) {
            for part in content {
                match part.get("type").and_then(|t| t.as_str()) {
                    Some("text") => {
                        if let Some(s) = part.get("text").and_then(|t| t.as_str()) {
                            if !text.is_empty() {
                                text.push('\n');
                            }
                            text.push_str(s);
                        }
                    }
                    // **Named, not dropped.** A server returning an image and a harness silently
                    // returning nothing would look like a tool that does not work.
                    Some(other) => {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(&format!("[{other} content, which this build cannot read]"));
                    }
                    None => {}
                }
            }
        }
        Ok((text, is_error))
    }

    fn request(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, McpError> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(serde_json::json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params,
        }))?;

        // **Read until the matching id.** A server may interleave notifications and log messages
        // with responses; taking the first line as the answer would attribute a notification's
        // body to this call.
        //
        // The clock lives in `deadline.rs` -- its own file, so this one keeps failing
        // `determinism_guard` if a second clock read ever appears in it. See that module.
        let deadline = crate::deadline::Deadline::after(REQUEST_TIMEOUT_MS);
        loop {
            if deadline.passed() {
                return Err(McpError::Timeout { id: self.id.clone(), method: method.into() });
            }
            let mut line = String::new();
            let n = self.stdout.read_line(&mut line).map_err(|e| McpError::Protocol {
                id: self.id.clone(),
                detail: format!("reading a reply to `{method}` failed: {e}"),
            })?;
            if n == 0 {
                return Err(McpError::Protocol {
                    id: self.id.clone(),
                    detail: format!("the server closed its output while `{method}` was pending"),
                });
            }
            let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
                // Not JSON: a server printing to stdout instead of stderr. Skipped rather than
                // fatal, because the protocol is still readable and the alternative is refusing
                // to talk to a server over a stray `print`.
                continue;
            };
            if value.get("id").and_then(|v| v.as_i64()) != Some(id) {
                continue;
            }
            if let Some(e) = value.get("error") {
                let message = e
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("no message")
                    .to_string();
                return Err(McpError::Remote {
                    id: self.id.clone(),
                    method: method.into(),
                    message,
                });
            }
            return Ok(value.get("result").cloned().unwrap_or(serde_json::Value::Null));
        }
    }

    fn notify(&mut self, method: &str, params: serde_json::Value) -> Result<(), McpError> {
        self.send(serde_json::json!({ "jsonrpc": "2.0", "method": method, "params": params }))
    }

    fn send(&mut self, message: serde_json::Value) -> Result<(), McpError> {
        let line = format!("{message}\n");
        self.stdin.write_all(line.as_bytes()).and_then(|_| self.stdin.flush()).map_err(|e| {
            McpError::Protocol {
                id: self.id.clone(),
                detail: format!("writing to the server failed: {e}"),
            }
        })
    }

    /// Turn the server's tool list into registrations.
    ///
    /// **Ids are namespaced `<server>__<tool>`.** Two servers offering `search` would otherwise
    /// collide in one registry, and whichever registered second would be refused — which reads as
    /// "the second server is broken". The separator is two underscores because a tool name is
    /// matched by string equality all over this codebase and a `/` or `:` would need every one of
    /// those to learn about namespacing.
    pub fn registrations(&mut self) -> Result<Vec<ToolRegistration>, McpError> {
        let server = self.id.clone();
        let consequence = self.consequence;
        let tools = self.list_tools()?;

        let mut out = Vec::new();
        for t in tools {
            let id = ToolId::new(format!("{server}__{}", t.name));
            let transport = Transport::Mcp { server: server.clone() };
            let params: Vec<RawParamSpec> = t
                .params
                .iter()
                .map(|(name, ty)| RawParamSpec {
                    name: name.clone(),
                    // **Every parameter is a Target, fail-closed.** An MCP schema does not say
                    // which arguments choose a destination, and the adjudicator's own default for
                    // an undeclared argument is `Target` for the same reason. Guessing `Payload`
                    // here would skip the provenance check on an argument nobody classified.
                    role: Some(ArgumentRole::Target),
                    ty: *ty,
                    required: t.required.contains(name),
                    // **None, deliberately.** An MCP `inputSchema` may carry a per-property
                    // `description`, and passing it through would be an improvement -- but it is
                    // third-party text reaching the model's tool schema, which is ADR-052's
                    // subject and not a field to wire in passing. Until then a server's parameter
                    // gets the generated sentence, same as before.
                    description: None,
                })
                .collect();

            let manifest = load(
                RawManifest {
                    tool: id.clone(),
                    paths: vec![],
                    hosts: vec![],
                    creds: vec![],
                    consequence,
                    params,
                },
                // **`UserReviewed`, not `ThirdParty` — ADR-052.** The user installed this server;
                // that is the review. `ThirdParty` would additionally refuse any consequence below
                // `Consequential`, so a user who has decided their local file-search server is
                // `Inert` could not say so.
                ManifestProvenance::UserReviewed { at: 0 },
            )
            .map_err(|e| McpError::Registration {
                id: server.clone(),
                tool: t.name.clone(),
                detail: e.to_string(),
            })?;

            out.push(ToolRegistration {
                id,
                manifest,
                description: t.description,
                summary: SummarySpec::new("mcp", 4_096),
                transport,
            });
        }
        Ok(out)
    }
}

impl Drop for McpClient {
    /// **Kill the child.** A daemon restart that left a server process running would leave one per
    /// restart, and each holds whatever the user's config gave it.
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn param_type(json_type: Option<&str>) -> ParamType {
    match json_type {
        Some("integer") | Some("number") => ParamType::Integer,
        Some("boolean") => ParamType::Boolean,
        // Everything else — including `object` and `array`, which have no `ParamType` — crosses as
        // text. The alternative is refusing to register the tool at all, which would make a whole
        // server unusable over one structured argument.
        _ => ParamType::Text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A REGRESSION GUARD, AND NOT A PROOF — the distinction is the point.**
    ///
    /// The enforcement of "no console window" is the Windows loader's, not this crate's. `Command`
    /// exposes no getter for `creation_flags`, so nothing in-process can read back what was set,
    /// and no test in this repository can observe whether a window appeared. **Asserting that the
    /// constant equals `0x0800_0000` would assert the declaration, which is the family this
    /// project logs at #16.**
    ///
    /// So this asserts the one thing that IS checkable and that would actually regress: the spawn
    /// path still routes through `detach_console`. A refactor of `connect` that drops the call —
    /// the realistic failure — fails here by name.
    ///
    /// **The real verification was a human looking at the taskbar** after `mcp.json` loaded two
    /// tools and no blank `python.exe` console appeared. Live, not piped.
    #[test]
    fn the_spawn_path_still_detaches_the_console() {
        let src = include_str!("lib.rs");
        let connect = src
            .split_once("pub fn connect(")
            .expect("connect() was renamed; this guard names it")
            .1;
        let body = &connect[..connect.find("let stdin =").unwrap_or(connect.len())];

        assert!(
            body.contains("detach_console(&mut cmd)"),
            "the MCP spawn path no longer calls detach_console, so every stdio server on Windows \
             opens a blank console the user can close -- killing the server under a live session. \
             See the fn's own doc comment for why stderr stays inherited.\n{body}"
        );
        assert!(
            body.contains(".spawn()"),
            "the vacuity control: this guard is about the ORDER of two calls, so if the spawn is \
             gone the assertion above is about nothing"
        );
    }

    /// **The dependency claim in the module header, checked rather than asserted in prose.**
    ///
    /// ADR-031 §2.3's discipline: a crate that reaches no socket proves it by what it depends on,
    /// and the manifest is the only place that cannot be wrong about it.
    #[test]
    fn no_socket_reaches_this_crate() {
        let manifest = include_str!("../Cargo.toml");
        let deps = manifest
            .split("[dependencies]")
            .nth(1)
            .expect("the manifest has a dependencies section");
        for forbidden in ["marlowe-net", "rustls", "reqwest", "hyper", "tokio"] {
            // Lines that begin with `#` are the comment explaining this very rule, and naming
            // `marlowe-net` in it must not fail the test that the rule describes.
            let declared = deps
                .lines()
                .filter(|l| !l.trim_start().starts_with('#'))
                .any(|l| l.trim_start().starts_with(forbidden));
            assert!(
                !declared,
                "`{forbidden}` is a dependency of marlowe-mcp. ADR-052 §3: MCP speaks over a \
                 child process's stdio and reaches no socket, because layer 4 (egress \
                 allowlisting) is approved and NOT shipped. If an HTTP transport is being added, \
                 it needs its own argument and layer 4 in front of it"
            );
        }
        // The control: without it this passes against an empty or unfindable manifest.
        assert!(deps.contains("marlowe-tools"), "the manifest scan found no real dependency");
    }

    #[test]
    fn a_json_type_maps_to_a_param_type_and_the_unknown_case_is_text() {
        assert_eq!(param_type(Some("integer")), ParamType::Integer);
        assert_eq!(param_type(Some("number")), ParamType::Integer);
        assert_eq!(param_type(Some("boolean")), ParamType::Boolean);
        assert_eq!(param_type(Some("string")), ParamType::Text);
        assert_eq!(param_type(Some("object")), ParamType::Text);
        assert_eq!(param_type(None), ParamType::Text);
    }

    #[test]
    fn a_server_that_cannot_be_started_says_so_by_name() {
        let spec = ServerSpec {
            id: "ghost".into(),
            command: "definitely-not-an-executable-anywhere".into(),
            args: vec![],
            consequence: None,
        };
        match McpClient::connect(&spec) {
            Err(McpError::Spawn { id, .. }) => assert_eq!(id, "ghost"),
            other => panic!("expected a named spawn failure, got {other:?}"),
        }
    }
}

impl std::fmt::Debug for McpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpClient").field("id", &self.id).finish_non_exhaustive()
    }
}
