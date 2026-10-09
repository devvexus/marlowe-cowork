//! What a turn reports while it runs, and how it ended.

use marlowe_model::Usage;
use marlowe_proto::TurnStop;

/// Live turn events. The daemon maps these onto proto events for the session.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    TextDelta(String),
    ReasoningDelta { text: String, tokens: u64 },
    /// Text streamed so far in this model call was reasoning.
    TextRetracted { tokens: u64 },
    ToolStarted { call_id: String, tool: String, summary: String },
    ToolFinished { call_id: String, ok: bool, summary: String },
    /// Usage of one finished model call.
    Usage(Usage),
    /// The history was compacted.
    Compacted { before_tokens: u32, after_tokens: u32 },
    /// A steer from the inbox was folded into the conversation.
    SteerApplied { text: String },
}

/// How a turn ended.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnOutcome {
    pub stop: TurnStop,
    /// Usage summed over every model call in the turn.
    pub usage: Usage,
    /// Model calls made.
    pub steps: u32,
    /// The final assistant text (all text parts of the turn joined).
    pub text: String,
    /// Set when `stop` is `Error`.
    pub error: Option<String>,
}
