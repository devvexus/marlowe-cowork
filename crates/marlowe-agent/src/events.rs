//! What a turn reports while it runs, and how it ended.

use std::time::Instant;

use marlowe_model::Usage;
use marlowe_proto::{PhaseEvent, PhaseKind, TurnStop};

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
    /// A named step of the turn started, advanced or ended. `session` and `turn` are the daemon's
    /// to fill in.
    Phase(PhaseEvent),
    /// A model call is about to go on the wire. `est_prompt_tokens` is the loop's own estimate of
    /// the prompt (system, history and tool schemas).
    CallStarted { step: u32, est_prompt_tokens: u32 },
    /// The first text or reasoning of a model call reached the loop, at `at`: the origin of the
    /// provider-token-to-screen overhead.
    FirstToken { at: Instant },
}

/// A phase being timed: `Start` goes out when it is made, `End` with the measured time when it
/// ends.
pub(crate) struct PhaseTimer {
    kind: PhaseKind,
    started: Instant,
}

impl PhaseTimer {
    pub fn start(kind: PhaseKind, sink: &mut dyn FnMut(AgentEvent)) -> Self {
        sink(AgentEvent::Phase(PhaseEvent::start(kind)));
        Self { kind, started: Instant::now() }
    }

    pub fn progress(&self, done: u64, total: u64, unit: &str, estimated: bool, sink: &mut dyn FnMut(AgentEvent)) {
        let mut ev = PhaseEvent::progress(self.kind, done, total).unit(unit);
        ev.est = estimated;
        sink(AgentEvent::Phase(ev));
    }

    /// Ends the phase; `with` may add its numbers. Returns the measured ms.
    pub fn end_with(self, sink: &mut dyn FnMut(AgentEvent), with: impl FnOnce(PhaseEvent) -> PhaseEvent) -> u64 {
        let ms = self.started.elapsed().as_millis() as u64;
        sink(AgentEvent::Phase(with(PhaseEvent::end(self.kind, ms))));
        ms
    }

    pub fn end(self, sink: &mut dyn FnMut(AgentEvent)) -> u64 {
        self.end_with(sink, |e| e)
    }
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
    /// A `before_turn` middleware blocked the turn (`error` holds its reason): nothing was
    /// appended and the prompt is not kept.
    pub blocked: bool,
    /// The final model call stopped at the output-token limit, so its text may end mid-sentence.
    pub truncated: bool,
}
