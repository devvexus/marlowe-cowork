//! Streaming events and the finished completion.

use serde::{Deserialize, Serialize};

use crate::message::{Message, Part, Role, Thinking, ToolCall};

/// Token accounting. Counts are cumulative for one model call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input: u64,
    #[serde(default)]
    pub output: u64,
    /// Reasoning tokens, when the provider reports them (often included in `output`).
    #[serde(default)]
    pub reasoning: u64,
    /// Input tokens served from a prompt cache.
    #[serde(default)]
    pub cached_input: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    /// Prompt tokens the engine actually evaluated (the rest came from its cache), when it says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_eval: Option<u64>,
    /// Engine-reported milliseconds for the model load, the prompt read and the decode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefill_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decode_ms: Option<u64>,
}

impl Usage {
    /// Adds another call's usage (for per-turn totals).
    pub fn add(&mut self, other: &Usage) {
        self.input += other.input;
        self.output += other.output;
        self.reasoning += other.reasoning;
        self.cached_input += other.cached_input;
        self.cost_usd = match (self.cost_usd, other.cost_usd) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or(0.0) + b.unwrap_or(0.0)),
        };
        let sum = |a: Option<u64>, b: Option<u64>| match (a, b) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or(0) + b.unwrap_or(0)),
        };
        self.prompt_eval = sum(self.prompt_eval, other.prompt_eval);
        self.load_ms = sum(self.load_ms, other.load_ms);
        self.prefill_ms = sum(self.prefill_ms, other.prefill_ms);
        self.decode_ms = sum(self.decode_ms, other.decode_ms);
    }

    /// Merges a later report of the same call: each count keeps its maximum.
    pub fn merge_report(&mut self, later: &Usage) {
        self.input = self.input.max(later.input);
        self.output = self.output.max(later.output);
        self.reasoning = self.reasoning.max(later.reasoning);
        self.cached_input = self.cached_input.max(later.cached_input);
        if later.cost_usd.is_some() {
            self.cost_usd = later.cost_usd;
        }
        self.prompt_eval = self.prompt_eval.max(later.prompt_eval);
        self.load_ms = self.load_ms.max(later.load_ms);
        self.prefill_ms = self.prefill_ms.max(later.prefill_ms);
        self.decode_ms = self.decode_ms.max(later.decode_ms);
    }

    /// Input plus output.
    pub fn total(&self) -> u64 {
        self.input + self.output
    }
}

/// Why a model call stopped.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    #[default]
    EndTurn,
    ToolUse,
    MaxTokens,
    ContentFilter,
    Cancelled,
    Error,
}

/// One streamed increment from a provider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum StreamEvent {
    /// The request is on its way to the engine (after any preparation, before the first byte
    /// back). Only providers that can tell emit it.
    Sent,
    /// Visible answer text.
    TextDelta(String),
    /// Reasoning text; `tokens` is the provider's or an estimated count for this delta.
    ReasoningDelta { text: String, tokens: u64 },
    /// All text emitted so far in this call was reasoning after all (e.g. a bare `</think>`).
    Retract { tokens: u64 },
    ToolCallStart { index: u32, id: String, name: String },
    /// A fragment of the call's JSON arguments.
    ToolArgsDelta { index: u32, json: String },
    ToolCallEnd { index: u32 },
    Usage(Usage),
    Stop(StopReason),
}

/// A finished model call.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Completion {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<Thinking>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default)]
    pub usage: Usage,
    #[serde(default)]
    pub stop: StopReason,
}

impl Completion {
    /// The assistant message to append: thinking, then text, then every tool call.
    pub fn to_message(&self) -> Message {
        let mut content = Vec::new();
        if let Some(t) = &self.thinking {
            if !t.is_empty() {
                content.push(Part::Thinking(t.clone()));
            }
        }
        if !self.text.is_empty() {
            content.push(Part::Text { text: self.text.clone() });
        }
        content.extend(self.tool_calls.iter().cloned().map(Part::ToolCall));
        Message { role: Role::Assistant, content }
    }
}
