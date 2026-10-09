//! The MCP tool host — installed servers, reachable by the model. ADR-052.
//!
//! # What this file decides, and what it deliberately does not
//!
//! It decides **one** thing that matters: an MCP tool's result crosses at
//! [`TrustClass::UntrustedContent`], every time, with no branch on the server, the tool, or
//! whether the call succeeded.
//!
//! That is ADR-052's second condition and it is not a carve-out from the decision that servers are
//! trusted — it is the existing rule applied unchanged. `read` is a fully trusted builtin whose
//! file contents are untrusted; so are `bash`'s output and `web`'s pages. **Trust attaches to who
//! wrote the tool, never to what the tool hands back at runtime.** `marlowe-contract`'s definition
//! of `UntrustedContent` has read *"web, inbound mail, MCP server output, third-party skill
//! output"* since it was written; this is the first code to make the third of those true.
//!
//! The consequence is that `Engine::condense_batch` routes every MCP result through a quarantined
//! child, because layer 1 triggers on `blocks_composed_targets` — **the trust class, not the tool
//! name** (ADR-039). Nothing here had to ask for that and nothing here can opt out of it, which is
//! why the trigger was keyed that way.
//!
//! # Where the servers are configured
//!
//! `<profile_root>/mcp.json`, a list of [`marlowe_mcp::ServerSpec`]. Absent means no servers, which
//! is not a fault. A malformed file **is** a fault and refuses at startup rather than starting with
//! the servers the parser happened to reach.
//!
//! # One connection per server, held for the daemon's life
//!
//! Reconnecting per turn would re-run the handshake and re-fetch the tool list on every message,
//! and a server with any start-up cost would pay it each time. The child is killed on drop.

use std::sync::{Arc, Mutex};

use marlowe_contract::TrustClass;
use marlowe_loop::driver::{ToolBody, ToolHost, ToolOutcome};
use marlowe_mcp::{McpClient, McpError, ServerSpec};
use marlowe_permission::{Adjudication, Args};
use marlowe_tools::{Metric, ResultSummary, ToolId, ToolRegistration};

/// Everything one profile's MCP servers contribute.
///
/// Built once at startup: the connections, the registrations they produced, and the pins the
/// registrations were checked against.
pub struct McpFleet {
    clients: Vec<McpClient>,
    registrations: Vec<ToolRegistration>,
    /// Tools whose description differs from the pin, or which were not pinned at all. **Reported,
    /// not silently accepted** — ADR-052 §4.
    reconsent: Vec<String>,
}

impl McpFleet {
    /// No servers. The zero-config path, and the one every existing test takes.
    pub fn empty() -> Self {
        Self { clients: Vec::new(), registrations: Vec::new(), reconsent: Vec::new() }
    }

    /// Connect to every configured server and collect its tools.
    ///
    /// **A server that fails to start is an error, not a silent omission.** A tool the user
    /// installed and cannot see is the failure this avoids; "it did not connect" belongs on
    /// screen, not in a variable nobody reads.
    pub fn connect(specs: &[ServerSpec], pins: &mut marlowe_tools::pin::PinnedDescriptions) -> (Self, Vec<McpError>) {
        let mut clients = Vec::new();
        let mut registrations = Vec::new();
        let mut reconsent = Vec::new();
        let mut errors = Vec::new();

        for spec in specs {
            match McpClient::connect(spec) {
                Err(e) => errors.push(e),
                Ok(mut client) => match client.registrations() {
                    Err(e) => errors.push(e),
                    Ok(regs) => {
                        // ── ADR-052 §4: the description the user approved, or an ask ──────────
                        let current: Vec<(ToolId, marlowe_tools::Description)> =
                            regs.iter().map(|r| (r.id.clone(), r.description.clone())).collect();
                        match pins.verdict(&current) {
                            marlowe_tools::pin::PinVerdict::Unchanged => {}
                            marlowe_tools::pin::PinVerdict::Changed { tools } => {
                                for t in tools {
                                    reconsent.push(format!(
                                        "`{}` describes itself differently than when it was \
                                         installed. Re-read it before using it.",
                                        t.as_str()
                                    ));
                                }
                            }
                            marlowe_tools::pin::PinVerdict::New { tools } => {
                                for t in tools {
                                    reconsent.push(format!(
                                        "`{}` is new since this server was installed.",
                                        t.as_str()
                                    ));
                                }
                            }
                        }
                        // Pinned AFTER the verdict, so the comparison is against what the user
                        // last saw rather than against what just arrived.
                        for (id, description) in &current {
                            pins.pin(id, description);
                        }
                        registrations.extend(regs);
                        clients.push(client);
                    }
                },
            }
        }

        (Self { clients, registrations, reconsent }, errors)
    }

    pub fn registrations(&self) -> &[ToolRegistration] {
        &self.registrations
    }

    /// The ids these servers contribute, for [`marlowe_loop::CapabilityProfile::interactive_with`].
    pub fn tool_ids(&self) -> Vec<ToolId> {
        self.registrations.iter().map(|r| r.id.clone()).collect()
    }

    /// What the user should re-read before trusting. See ADR-052 §4.
    pub fn reconsent(&self) -> &[String] {
        &self.reconsent
    }

    /// How many servers answered. Distinct from the tool count: one server offering four tools
    /// and four servers offering one each are different facts to a user reading a startup line.
    pub fn servers(&self) -> usize {
        self.clients.len()
    }

    pub fn is_empty(&self) -> bool {
        self.registrations.is_empty()
    }
}

impl std::fmt::Debug for McpFleet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpFleet")
            .field("servers", &self.clients.len())
            .field("tools", &self.registrations.len())
            .finish()
    }
}

/// The daemon's tool host: whatever it wraps, plus every installed MCP server's tools.
pub struct McpTools<H: ToolHost> {
    inner: H,
    fleet: Arc<Mutex<McpFleet>>,
}

impl<H: ToolHost> McpTools<H> {
    pub fn new(inner: H, fleet: Arc<Mutex<McpFleet>>) -> Self {
        Self { inner, fleet }
    }

    fn serves(&self, tool: &ToolId) -> bool {
        self.fleet
            .lock()
            .expect("the mcp fleet lock was poisoned")
            .registrations
            .iter()
            .any(|r| r.id == *tool)
    }

    fn call(&self, tool: &ToolId, args: &Args) -> ToolOutcome {
        let mut fleet = self.fleet.lock().expect("the mcp fleet lock was poisoned");

        // `<server>__<tool>` — see `McpClient::registrations` on why the namespace is in the id.
        let Some((server, remote)) = tool.as_str().split_once("__") else {
            return refused(&format!(
                "`{}` is not a namespaced MCP tool id",
                tool.as_str()
            ));
        };
        let server = server.to_string();
        let remote = remote.to_string();

        let Some(client) = fleet.clients.iter_mut().find(|c| c.id() == server) else {
            return refused(&format!("the MCP server `{server}` is not connected"));
        };

        let arguments = serde_json::Value::Object(
            args.iter()
                .map(|(name, value)| (name.clone(), json_of(value)))
                .collect::<serde_json::Map<_, _>>(),
        );

        // **`wall_ms: 0`, not a measured duration**, and this is the established shape rather
        // than an omission: `recall.rs` and `skills.rs` both report 0 for the same reason.
        // Measuring it here would be a clock read outside CONTRACTS 4.5's fences, and
        // `determinism_guard` refused exactly that on the first workspace run after this file was
        // written. A per-tool latency belongs in the journal's own timing, not in a value this
        // path invents.
        match client.call_tool(&remote, arguments) {
            Err(e) => {
                // **A transport failure is the HARNESS's observation**, not the server's content:
                // no byte the server wrote is in this string, so it does not inherit the server's
                // class. The same split `recall` makes when it reports an empty store.
                refused(&e.to_string())
            }
            Ok((text, is_error)) => ToolOutcome {
                summary: ResultSummary::new(vec![
                    Metric::Bytes { n: text.len() as u64 },
                    Metric::State(if is_error { "tool error" } else { "ok" }),
                ]),
                body: ToolBody::Inline(text),
                // ── THE ONE DECISION IN THIS FILE ────────────────────────────────────────────
                //
                // Unconditional. No branch on the server, the tool, or `is_error`: an error
                // message is content the server chose too, and a failed call is exactly when a
                // hostile server would put its payload somewhere a success path does not look.
                //
                // This is what routes the result through layer 1's quarantine — `condense_batch`
                // triggers on `blocks_composed_targets`, the trust class rather than the tool
                // name — so nothing here asks for containment and nothing here can opt out.
                trust: TrustClass::UntrustedContent,
                failed: is_error,
                wall_ms: 0,
                preview: None,
            },
        }
    }
}

fn json_of(value: &marlowe_permission::ArgValue) -> serde_json::Value {
    match value {
        marlowe_permission::ArgValue::Text(s) => serde_json::Value::String(s.clone()),
        marlowe_permission::ArgValue::Integer(i) => serde_json::json!(i),
        marlowe_permission::ArgValue::Amount(a) => serde_json::json!(a),
        marlowe_permission::ArgValue::Boolean(b) => serde_json::json!(b),
    }
}

/// A harness-authored refusal. `AgentObserved`: the harness observed the failure and no byte the
/// server wrote is in the text.
fn refused(detail: &str) -> ToolOutcome {
    ToolOutcome {
        summary: ResultSummary::new(vec![Metric::State("unavailable")]),
        body: ToolBody::Inline(detail.to_string()),
        trust: TrustClass::AgentObserved,
        failed: true,
        wall_ms: 0,
        preview: None,
    }
}

impl<H: ToolHost> ToolHost for McpTools<H> {
    fn executes(&self) -> Vec<ToolId> {
        let mut tools = self.inner.executes();
        tools.extend(
            self.fleet.lock().expect("the mcp fleet lock was poisoned").tool_ids(),
        );
        tools
    }

    fn execute(&mut self, tool: &ToolId, args: &Args, adjudication: &Adjudication) -> ToolOutcome {
        if self.serves(tool) {
            return self.call(tool, args);
        }
        self.inner.execute(tool, args, adjudication)
    }

    /// Forward the batch. Third wrapper, same trap — see `RecallTools::execute_batch`.
    ///
    /// MCP calls are **served serially** rather than concurrently, and that is deliberate: one
    /// client owns one child's stdin and stdout, so two calls in flight on one server would
    /// interleave on the pipe. Everything else is delegated as one batch, so the inner host's
    /// concurrent fetch is untouched.
    fn execute_batch(&mut self, items: &[marlowe_loop::BatchItem<'_>]) -> Vec<ToolOutcome> {
        let mut delegated_slots: Vec<usize> = Vec::new();
        let mut delegated: Vec<marlowe_loop::BatchItem<'_>> = Vec::new();
        for (i, it) in items.iter().enumerate() {
            if !self.serves(it.tool) {
                delegated_slots.push(i);
                delegated.push(marlowe_loop::BatchItem {
                    tool: it.tool,
                    args: it.args,
                    adjudication: it.adjudication,
                });
            }
        }

        let inner_out = if delegated.is_empty() {
            Vec::new()
        } else {
            self.inner.execute_batch(&delegated)
        };

        let mut out: Vec<Option<ToolOutcome>> = (0..items.len()).map(|_| None).collect();
        let mut got = inner_out.into_iter();
        for slot in &delegated_slots {
            match got.next() {
                Some(o) => out[*slot] = Some(o),
                None => break,
            }
        }
        for (i, it) in items.iter().enumerate() {
            if self.serves(it.tool) {
                out[i] = Some(self.call(it.tool, it.args));
            }
        }

        out.into_iter()
            .enumerate()
            .map(|(i, o)| {
                o.unwrap_or_else(|| {
                    refused(&format!(
                        "the inner tool host returned no outcome for call {i} (`{}`)",
                        items[i].tool.as_str()
                    ))
                })
            })
            .collect()
    }
}

/// Read `<profile_root>/mcp.json`.
///
/// Absent is no servers and no error. **Malformed is an error**, because starting with the servers
/// a broken parse happened to reach is how a user ends up with half their tools and no statement
/// that anything went wrong.
pub fn read_config(profile_root: &std::path::Path) -> Result<Vec<ServerSpec>, String> {
    let path = profile_root.join("mcp.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(Vec::new());
    };
    serde_json::from_str::<Vec<ServerSpec>>(&text)
        .map_err(|e| format!("{} is not a list of MCP server specs: {e}", path.display()))
}

/// Read the descriptions the user approved. Absent is an empty pin, which treats every tool as
/// **new** rather than as approved — see `PinnedDescriptions::verdict`.
pub fn read_pins(profile_root: &std::path::Path) -> marlowe_tools::pin::PinnedDescriptions {
    std::fs::read_to_string(profile_root.join("mcp-pins.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Record them.
///
/// **A write failure is not fatal and not silent-by-design either**: the consequence is that the
/// next start re-asks about every tool, which is the safe direction. Refusing to start because a
/// pin file could not be written would take the whole product down over a bookkeeping file.
pub fn write_pins(profile_root: &std::path::Path, pins: &marlowe_tools::pin::PinnedDescriptions) {
    if let Ok(text) = serde_json::to_string_pretty(pins) {
        let _ = std::fs::write(profile_root.join("mcp-pins.json"), text);
    }
}
