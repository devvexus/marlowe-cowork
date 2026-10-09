//! Conversation messages.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Who authored a message. Tool results travel in a `Tool` message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// Model reasoning, kept so it can be sent back on tool turns.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Thinking {
    pub text: String,
    /// Provider signature over the reasoning (e.g. Anthropic `signature`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    /// Opaque provider payload to round-trip verbatim (e.g. OpenRouter `reasoning_details`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

impl Thinking {
    /// True when there is nothing to show or round-trip.
    pub fn is_empty(&self) -> bool {
        self.text.is_empty() && self.signature.is_none() && self.details.is_none()
    }
}

/// One tool call requested by the model. `args` is the parsed JSON object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Provider id, or harness-assigned when the provider gives none.
    pub id: String,
    pub name: String,
    pub args: Value,
}

/// The result of one tool call, attributed by `call_id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: String,
    pub content: String,
    #[serde(default)]
    pub is_error: bool,
}

/// One piece of a message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Part {
    Text { text: String },
    Thinking(Thinking),
    ToolCall(ToolCall),
    ToolResult(ToolResult),
}

/// A message in a conversation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<Part>,
}

impl Message {
    /// A message holding a single text part.
    pub fn text(role: Role, text: impl Into<String>) -> Self {
        Self { role, content: vec![Part::Text { text: text.into() }] }
    }

    /// A user message.
    pub fn user(text: impl Into<String>) -> Self {
        Self::text(Role::User, text)
    }

    /// An assistant message.
    pub fn assistant(text: impl Into<String>) -> Self {
        Self::text(Role::Assistant, text)
    }

    /// A tool message carrying results.
    pub fn tool_results(results: impl IntoIterator<Item = ToolResult>) -> Self {
        Self { role: Role::Tool, content: results.into_iter().map(Part::ToolResult).collect() }
    }

    /// All text parts concatenated.
    pub fn joined_text(&self) -> String {
        self.content
            .iter()
            .filter_map(|p| match p {
                Part::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    /// The tool calls in this message, in order.
    pub fn tool_calls(&self) -> impl Iterator<Item = &ToolCall> {
        self.content.iter().filter_map(|p| match p {
            Part::ToolCall(c) => Some(c),
            _ => None,
        })
    }

    /// The tool results in this message, in order.
    pub fn tool_results_iter(&self) -> impl Iterator<Item = &ToolResult> {
        self.content.iter().filter_map(|p| match p {
            Part::ToolResult(r) => Some(r),
            _ => None,
        })
    }

    /// The reasoning part, if any.
    pub fn thinking(&self) -> Option<&Thinking> {
        self.content.iter().find_map(|p| match p {
            Part::Thinking(t) => Some(t),
            _ => None,
        })
    }
}
