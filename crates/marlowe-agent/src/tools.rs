//! The tool port. Implemented by `marlowe-exec::BuiltinTools`.

use std::collections::HashSet;
use std::path::PathBuf;

use marlowe_model::{CancelToken, ToolSpec};
use marlowe_proto::SessionId;

/// One tool call as the loop hands it to the host (id already assigned).
pub type ToolCallReq = marlowe_model::ToolCall;

/// Per-call context.
#[derive(Debug, Clone)]
pub struct ToolCtx {
    pub session: SessionId,
    pub turn: u32,
    /// The session's workspace root, if any.
    pub workspace: Option<PathBuf>,
    /// Fires when the turn is interrupted; long-running tools must honour it.
    pub cancel: CancelToken,
}

/// What a tool call produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutcome {
    /// Sent to the model as the tool result.
    pub content: String,
    pub is_error: bool,
    /// One dim line for the UI, e.g. `41 lines` or `exit 1`.
    pub summary: String,
}

impl ToolOutcome {
    pub fn ok(content: impl Into<String>, summary: impl Into<String>) -> Self {
        Self { content: content.into(), is_error: false, summary: summary.into() }
    }

    pub fn error(message: impl Into<String>) -> Self {
        let m = message.into();
        let summary = m.lines().next().unwrap_or("").chars().take(80).collect();
        Self { content: m, is_error: true, summary }
    }
}

/// Whether a call may run without asking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Consequence {
    Free,
    /// Needs approval; `summary` is the one line shown in the approval modal.
    Ask { summary: String },
}

/// What a session has been exposed to. It changes which calls need approval.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Exposure {
    /// Untrusted content (a fetched page, search results, a file from outside the workspace) has
    /// entered the conversation.
    pub tainted: bool,
    /// Network hosts the user allowed in this session.
    pub approved_hosts: HashSet<String>,
}

/// Executes tools. Hosts may be called from several threads at once.
pub trait ToolHost: Send + Sync {
    /// The tools exposed to the model.
    fn specs(&self) -> Vec<ToolSpec>;

    /// Runs one call. Unknown tools and bad args return an error outcome, never panic.
    fn call(&self, call: &ToolCallReq, ctx: &ToolCtx) -> ToolOutcome;

    /// Whether `call` needs approval.
    fn consequence(&self, call: &ToolCallReq) -> Consequence;

    /// Whether `call` needs approval given what the session has been exposed to. The loop calls
    /// this one; the default ignores exposure.
    fn consequence_exposed(&self, call: &ToolCallReq, _exposure: &Exposure) -> Consequence {
        self.consequence(call)
    }

    /// Whether running `call` brings untrusted content into the conversation.
    fn taints(&self, _call: &ToolCallReq) -> bool {
        false
    }

    /// The network host `call` reaches, recorded as approved when the user allows the call.
    fn egress_host(&self, _call: &ToolCallReq) -> Option<String> {
        None
    }

    /// The one-line description shown when the call starts, e.g. `read src/main.rs`.
    fn describe(&self, call: &ToolCallReq) -> String {
        let first = call
            .args
            .as_object()
            .and_then(|o| o.values().find_map(|v| v.as_str()))
            .unwrap_or("");
        let first: String = first.lines().next().unwrap_or("").chars().take(60).collect();
        if first.is_empty() {
            call.name.clone()
        } else {
            format!("{} {}", call.name, first)
        }
    }
}
