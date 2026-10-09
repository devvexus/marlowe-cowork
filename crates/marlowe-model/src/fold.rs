//! Folds a stream of [`StreamEvent`]s into a [`Completion`].

use std::collections::BTreeMap;

use serde_json::Value;

use crate::message::{Thinking, ToolCall};
use crate::stream::{Completion, StopReason, StreamEvent, Usage};

#[derive(Debug, Default)]
struct PendingCall {
    id: String,
    name: String,
    args: String,
}

/// Accumulates stream events. Providers feed every event they emit, then attach any opaque
/// reasoning payload with [`set_signature`](Self::set_signature) / [`set_details`](Self::set_details).
#[derive(Debug, Default)]
pub struct CompletionBuilder {
    text: String,
    thinking: Thinking,
    calls: BTreeMap<u32, PendingCall>,
    usage: Usage,
    stop: Option<StopReason>,
}

impl CompletionBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Applies one event.
    pub fn push(&mut self, ev: &StreamEvent) {
        match ev {
            StreamEvent::TextDelta(t) => self.text.push_str(t),
            StreamEvent::ReasoningDelta { text, .. } => self.thinking.text.push_str(text),
            StreamEvent::Retract { .. } => {
                let moved = std::mem::take(&mut self.text);
                self.thinking.text.push_str(&moved);
            }
            StreamEvent::ToolCallStart { index, id, name } => {
                let c = self.calls.entry(*index).or_default();
                if !id.is_empty() {
                    c.id = id.clone();
                }
                if !name.is_empty() {
                    c.name = name.clone();
                }
            }
            StreamEvent::ToolArgsDelta { index, json } => {
                self.calls.entry(*index).or_default().args.push_str(json);
            }
            StreamEvent::ToolCallEnd { .. } => {}
            StreamEvent::Usage(u) => self.usage.merge_report(u),
            StreamEvent::Stop(s) => self.stop = Some(*s),
        }
    }

    /// Sets the reasoning signature.
    pub fn set_signature(&mut self, signature: impl Into<String>) {
        self.thinking.signature = Some(signature.into());
    }

    /// Sets the opaque reasoning payload to round-trip.
    pub fn set_details(&mut self, details: Value) {
        self.thinking.details = Some(details);
    }

    /// Text accumulated so far.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Number of tool calls seen so far.
    pub fn tool_call_count(&self) -> usize {
        self.calls.len()
    }

    /// Finishes the fold.
    ///
    /// Tool calls are ordered by index; empty args become `{}`, unparsable args are kept as a
    /// JSON string so the loop can report them. Empty ids stay empty (the loop assigns them).
    /// With no `Stop` event the reason is `ToolUse` if there are calls, else `EndTurn`; an
    /// `EndTurn` alongside tool calls is normalised to `ToolUse`.
    pub fn finish(self) -> Completion {
        let tool_calls: Vec<ToolCall> = self
            .calls
            .into_values()
            .map(|c| ToolCall { id: c.id, name: c.name, args: parse_args(&c.args) })
            .collect();
        let stop = match self.stop {
            None | Some(StopReason::EndTurn) if !tool_calls.is_empty() => StopReason::ToolUse,
            None => StopReason::EndTurn,
            Some(s) => s,
        };
        let thinking = if self.thinking.is_empty() { None } else { Some(self.thinking) };
        Completion { text: self.text, thinking, tool_calls, usage: self.usage, stop }
    }
}

fn parse_args(raw: &str) -> Value {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Value::Object(Default::default());
    }
    serde_json::from_str(trimmed).unwrap_or_else(|_| Value::String(raw.to_string()))
}

/// Folds a complete event sequence.
pub fn fold_events<'a>(events: impl IntoIterator<Item = &'a StreamEvent>) -> Completion {
    let mut b = CompletionBuilder::new();
    for ev in events {
        b.push(ev);
    }
    b.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn text_and_reasoning_accumulate() {
        let c = fold_events(&[
            StreamEvent::ReasoningDelta { text: "hmm ".into(), tokens: 1 },
            StreamEvent::ReasoningDelta { text: "ok".into(), tokens: 1 },
            StreamEvent::TextDelta("Hello".into()),
            StreamEvent::TextDelta(", world".into()),
            StreamEvent::Stop(StopReason::EndTurn),
        ]);
        assert_eq!(c.text, "Hello, world");
        assert_eq!(c.thinking.unwrap().text, "hmm ok");
        assert_eq!(c.stop, StopReason::EndTurn);
        assert!(c.tool_calls.is_empty());
    }

    #[test]
    fn retract_moves_emitted_text_into_thinking() {
        let c = fold_events(&[
            StreamEvent::TextDelta("weighing it".into()),
            StreamEvent::Retract { tokens: 3 },
            StreamEvent::TextDelta("answer".into()),
        ]);
        assert_eq!(c.text, "answer");
        assert_eq!(c.thinking.unwrap().text, "weighing it");
    }

    #[test]
    fn interleaved_tool_calls_assemble_by_index_and_keep_text() {
        let c = fold_events(&[
            StreamEvent::TextDelta("Reading both.".into()),
            StreamEvent::ToolCallStart { index: 1, id: "b".into(), name: "grep".into() },
            StreamEvent::ToolCallStart { index: 0, id: "a".into(), name: "read".into() },
            StreamEvent::ToolArgsDelta { index: 0, json: "{\"path\":".into() },
            StreamEvent::ToolArgsDelta { index: 1, json: "{\"pattern\":\"x\"}".into() },
            StreamEvent::ToolArgsDelta { index: 0, json: "\"a.rs\"}".into() },
            StreamEvent::ToolCallEnd { index: 0 },
            StreamEvent::ToolCallEnd { index: 1 },
            StreamEvent::Stop(StopReason::EndTurn),
        ]);
        assert_eq!(c.text, "Reading both.");
        assert_eq!(c.tool_calls.len(), 2);
        assert_eq!(c.tool_calls[0].id, "a");
        assert_eq!(c.tool_calls[0].args, json!({"path": "a.rs"}));
        assert_eq!(c.tool_calls[1].name, "grep");
        assert_eq!(c.tool_calls[1].args, json!({"pattern": "x"}));
        assert_eq!(c.stop, StopReason::ToolUse, "EndTurn with calls is normalised");
    }

    #[test]
    fn empty_and_malformed_args_are_kept_honestly() {
        let c = fold_events(&[
            StreamEvent::ToolCallStart { index: 0, id: String::new(), name: "list".into() },
            StreamEvent::ToolCallStart { index: 1, id: String::new(), name: "read".into() },
            StreamEvent::ToolArgsDelta { index: 1, json: "{\"path\": ".into() },
        ]);
        assert_eq!(c.tool_calls[0].args, json!({}));
        assert_eq!(c.tool_calls[1].args, json!("{\"path\": "));
        assert_eq!(c.tool_calls[0].id, "", "ids are assigned by the loop, not the fold");
        assert_eq!(c.stop, StopReason::ToolUse);
    }

    #[test]
    fn usage_reports_merge_by_maximum_and_keep_cost() {
        let c = fold_events(&[
            StreamEvent::Usage(Usage { input: 100, ..Default::default() }),
            StreamEvent::Usage(Usage { input: 100, output: 40, cost_usd: Some(0.01), ..Default::default() }),
            StreamEvent::Stop(StopReason::MaxTokens),
        ]);
        assert_eq!(c.usage.input, 100);
        assert_eq!(c.usage.output, 40);
        assert_eq!(c.usage.cost_usd, Some(0.01));
        assert_eq!(c.stop, StopReason::MaxTokens);
    }

    #[test]
    fn opaque_reasoning_details_round_trip_into_the_message() {
        let mut b = CompletionBuilder::new();
        b.push(&StreamEvent::ReasoningDelta { text: "plan".into(), tokens: 1 });
        b.push(&StreamEvent::ToolCallStart { index: 0, id: "c1".into(), name: "read".into() });
        b.set_details(json!([{"type": "reasoning.encrypted", "data": "xyz"}]));
        let c = b.finish();
        let m = c.to_message();
        let t = m.thinking().unwrap();
        assert_eq!(t.text, "plan");
        assert_eq!(t.details, Some(json!([{"type": "reasoning.encrypted", "data": "xyz"}])));
        assert_eq!(m.tool_calls().count(), 1);
    }

    #[test]
    fn no_events_is_an_empty_end_turn() {
        let c = fold_events(&[]);
        assert_eq!(c, Completion::default());
    }
}
