//! What the engine asks of whoever runs its agents, and what it tells its observers.
//!
//! The engine thread never runs an agent itself: [`AgentHost::start`] hands one call over and
//! returns; the result comes back on the channel it was given. The crew implements the host with
//! durable crew steps (`marlowe-crew/src/playbook/script/`); tests use [`crate::ScriptedHost`].

use crossbeam_channel::Sender;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The options of one `agent()` call, as the script gave them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentOpts {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    /// A JSON Schema (the in-house subset, [`crate::schema`]): the agent returns an object.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<Value>,
    /// A model in the library or Ollama's local models (a full ref, or an id with or without
    /// `:latest`). One that is unknown or does not fit beside what is loaded runs on the
    /// conversation's model with a note; `inherit` or none keeps the role's model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// `low`, `medium`, `high`, `xhigh`, `max`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// `worktree`: the agent gets a child worktree of its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isolation: Option<String>,
    /// The role card (`explore`, `plan`, `general`, a project agent); `None` = `workflow`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
}

/// One agent the script issued.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentCall {
    /// Issue sequence number in this run (first time the script reached the call); what the
    /// progress view shows and `SkipWorkflowAgent` names.
    pub index: u32,
    /// Where in the script the call sits: `3`, `4.1/0`, `5.2.1/0` (top-level op 3; op 4's second
    /// thunk's first call; op 5's third item, second stage, first call). Deterministic for a given
    /// script and args whatever order agents finish in, so it is the resume identity.
    pub id: String,
    /// `wf:<id>:<blake3 of the canonical call>[..16]`: the crew step key and the cache key.
    pub key: String,
    pub prompt: String,
    pub opts: AgentOpts,
}

impl AgentCall {
    /// What the progress view calls it: the label, else the prompt's first line.
    pub fn display_label(&self) -> String {
        if let Some(l) = self.opts.label.as_deref().filter(|l| !l.trim().is_empty()) {
            return l.trim().to_string();
        }
        let first = self.prompt.lines().find(|l| !l.trim().is_empty()).unwrap_or("agent").trim();
        let mut s: String = first.chars().take(60).collect();
        if first.chars().count() > 60 {
            s.push('…');
        }
        s
    }
}

/// How an agent ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Done,
    /// Ended without a usable answer (the script sees `None`).
    Failed,
    /// The user skipped it (the script sees `None`).
    Skipped,
}

/// What one agent produced.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentResult {
    pub status: AgentStatus,
    /// Text for a plain agent, the object for a schema agent, `null` when it failed or was skipped.
    #[serde(default)]
    pub value: Value,
    /// Output tokens it spent (the budget pool counts these).
    #[serde(default)]
    pub tokens: u64,
    /// It read untrusted content; the run is tainted from here on.
    #[serde(default)]
    pub tainted: bool,
    /// Why it failed, when it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl AgentResult {
    pub fn text(text: impl Into<String>, tokens: u64) -> Self {
        Self { status: AgentStatus::Done, value: Value::String(text.into()), tokens, tainted: false, note: None }
    }

    pub fn object(value: Value, tokens: u64) -> Self {
        Self { status: AgentStatus::Done, value, tokens, tainted: false, note: None }
    }

    pub fn failed(why: impl Into<String>, tokens: u64) -> Self {
        Self { status: AgentStatus::Failed, value: Value::Null, tokens, tainted: false, note: Some(why.into()) }
    }

    pub fn skipped() -> Self {
        Self { status: AgentStatus::Skipped, value: Value::Null, tokens: 0, tainted: false, note: Some("skipped".into()) }
    }
}

/// A finished call, sent back to the engine.
#[derive(Debug, Clone, PartialEq)]
pub struct Completion {
    pub index: u32,
    pub result: AgentResult,
}

/// Runs a workflow's agents.
pub trait AgentHost: Send + Sync {
    /// Starts `call` and returns at once; exactly one [`Completion`] for `call.index` goes to
    /// `done` later (a host that cannot start it sends a failed result).
    fn start(&self, call: AgentCall, done: Sender<Completion>);

    /// The user skipped or stopped the call: interrupt it if it is running. The engine has already
    /// recorded it as skipped and ignores any completion that still arrives.
    fn cancel(&self, _index: u32) {}
}

/// Steering a running workflow from outside its thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Control {
    /// Skip the agent with this issue index: queued or in flight, `agent()` returns `None`.
    Skip(u32),
    /// Stop the run: no new agent starts, in-flight agents are cancelled, the run is resumable.
    Stop,
}

/// What happened in a run, in order, for the progress view, the journal and the hooks.
#[derive(Debug, Clone, PartialEq)]
pub enum WfEvent {
    /// The script entered a phase (`phase()`).
    Phase(String),
    /// `log()`.
    Log(String),
    /// An agent was issued (queued behind the concurrency cap or started at once).
    Queued { call: AgentCall },
    /// It started on the host (SubagentStart).
    Started { index: u32 },
    /// It ended (SubagentStop); `cached` = served from a previous run's journal.
    Finished { call: AgentCall, result: AgentResult, cached: bool, completion: u32 },
}

/// Receives a run's events on the engine thread. Must not block for long.
pub trait Observer {
    fn event(&mut self, ev: &WfEvent);
}

/// Discards events.
pub struct NoObserver;

impl Observer for NoObserver {
    fn event(&mut self, _ev: &WfEvent) {}
}

impl<F: FnMut(&WfEvent)> Observer for F {
    fn event(&mut self, ev: &WfEvent) {
        self(ev)
    }
}
