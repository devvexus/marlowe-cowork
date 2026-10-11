//! Folds `/api/chat` NDJSON frames into [`StreamEvent`]s and a [`Completion`].
//!
//! Reasoning arrives in `message.thinking` (or `reasoning` / `reasoning_content`), and sometimes
//! inline in `content` behind `<think>` tags, which [`ThinkSplitter`] separates. While thinking
//! was requested and no native reasoning has arrived, content is held rather than shown: a late
//! `</think>` would otherwise prove already-rendered "speech" was reasoning.
//!
//! Token counting: a frame is one token, charged once to the channel it fed. On the final frame
//! the engine's `eval_count` settles the difference: anything counted but not streamed belongs to
//! reasoning (Ollama merges whitespace tokens on the thinking channel).
//!
//! Tool calls: structured ones have their arguments repaired against the request's schemas
//! (`repair.rs`). A call written as text is withheld from the screen while it streams
//! ([`LeakGuard`]) and read at the end ([`scan`]); every repair is counted in [`RepairStats`].

use std::sync::Arc;

use marlowe_model::{Completion, CompletionBuilder, ProviderError, StopReason, StreamEvent, ToolSpec, Usage};
use serde_json::Value;

use super::leak::defang_tool_markup;
use super::repair::{repair_call, RepairStats};
use super::scan::{scan, LeakGuard, ToolSet};
use crate::think::{Segment, ThinkSplitter};

pub struct Folder<'a> {
    on: &'a mut dyn FnMut(StreamEvent),
    builder: CompletionBuilder,
    splitter: ThinkSplitter,
    /// The think block is known shut: content streams as speech.
    closed: bool,
    held: String,
    reasoning_tokens: u64,
    speech_tokens: u64,
    held_tokens: u64,
    next_call: u32,
    usage: Usage,
    stop: Option<StopReason>,
    done: bool,
    tools: Option<Vec<ToolSpec>>,
    guard: LeakGuard,
    stats: Option<Arc<RepairStats>>,
}

impl<'a> Folder<'a> {
    /// `thinking` is whether the request asked for reasoning.
    pub fn new(thinking: bool, on: &'a mut dyn FnMut(StreamEvent)) -> Self {
        Self {
            on,
            builder: CompletionBuilder::new(),
            splitter: ThinkSplitter::new(),
            closed: !thinking,
            held: String::new(),
            reasoning_tokens: 0,
            speech_tokens: 0,
            held_tokens: 0,
            next_call: 0,
            usage: Usage::default(),
            stop: None,
            done: false,
            tools: None,
            guard: LeakGuard::new(None),
            stats: None,
        }
    }

    /// The request's tools: call names and arguments are checked against them, and a call
    /// written as text is accepted only for a tool in the list. Without this, only the explicit
    /// markup forms are recovered, under any name.
    pub fn with_tools(mut self, tools: &[ToolSpec]) -> Self {
        self.tools = Some(tools.to_vec());
        self.guard = LeakGuard::new(self.tools.clone());
        self
    }

    /// Counts every repaired call here.
    pub fn with_stats(mut self, stats: Arc<RepairStats>) -> Self {
        self.stats = Some(stats);
        self
    }

    fn emit(&mut self, ev: StreamEvent) {
        self.builder.push(&ev);
        (self.on)(ev);
    }

    fn reasoning(&mut self, text: &str, tokens: u64) {
        self.reasoning_tokens += tokens;
        self.emit(StreamEvent::ReasoningDelta { text: text.to_string(), tokens });
    }

    fn speech(&mut self, text: &str, tokens: u64) {
        self.speech_tokens += tokens;
        let shown = self.guard.push(text);
        if !shown.is_empty() {
            self.emit(StreamEvent::TextDelta(shown));
        }
    }

    /// Speech that has already been judged (no guard).
    fn speech_now(&mut self, text: &str) {
        if !text.is_empty() {
            self.emit(StreamEvent::TextDelta(text.to_string()));
        }
    }

    /// True once the final (`done`) frame has been seen.
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// Applies one frame. An `error` frame is returned as a server error.
    pub fn frame(&mut self, frame: &Value) -> Result<(), ProviderError> {
        if let Some(err) = frame.get("error") {
            let detail = err.as_str().map(str::to_string).unwrap_or_else(|| err.to_string());
            return Err(ProviderError::server(format!("Ollama reported mid-stream: {detail}")));
        }
        if let Some(msg) = frame.get("message") {
            let mut charged = false;
            for field in ["thinking", "reasoning", "reasoning_content"] {
                if let Some(r) = msg.get(field).and_then(Value::as_str) {
                    if !r.is_empty() {
                        let charge = u64::from(!charged);
                        charged = true;
                        self.reasoning(r, charge);
                        // The engine separates the channels, so content is outside the block.
                        self.closed = true;
                    }
                }
            }
            if let Some(c) = msg.get("content").and_then(Value::as_str) {
                if !c.is_empty() {
                    self.content(c, u64::from(!charged));
                }
            }
            if let Some(calls) = msg.get("tool_calls").and_then(Value::as_array) {
                for call in calls {
                    self.tool_call(call);
                }
            }
        }
        if frame.get("done").and_then(Value::as_bool) == Some(true) {
            self.done = true;
            let n = |k: &str| frame.get(k).and_then(Value::as_u64).unwrap_or(0);
            self.usage.input = n("prompt_eval_count");
            self.usage.output = n("eval_count");
            self.stop = Some(match frame.get("done_reason").and_then(Value::as_str) {
                Some("length") => StopReason::MaxTokens,
                _ => StopReason::EndTurn,
            });
        }
        Ok(())
    }

    fn content(&mut self, c: &str, mut charge: u64) {
        let split = self.splitter.feed(c);
        if split.retract_speech {
            if !self.held.is_empty() {
                let held = std::mem::take(&mut self.held);
                let owed = std::mem::take(&mut self.held_tokens);
                self.reasoning(&held, owed);
            }
            // Text the guard was still holding was reasoning too.
            let withheld = self.guard.reset();
            self.speech_now(&withheld);
            if !self.builder.text().is_empty() {
                let owed = std::mem::take(&mut self.speech_tokens);
                self.reasoning_tokens += owed;
                self.emit(StreamEvent::Retract { tokens: owed });
            }
            self.closed = true;
        }
        for seg in split.segments {
            match seg {
                Segment::Reasoning(r) => {
                    let owed = std::mem::take(&mut charge);
                    self.reasoning(&r, owed);
                }
                Segment::Speech(t) => {
                    if self.closed {
                        let owed = std::mem::take(&mut charge);
                        self.speech(&t, owed);
                    } else {
                        self.held_tokens += std::mem::take(&mut charge);
                        self.held.push_str(&t);
                    }
                }
            }
        }
        // Swallowed by a partial tag: owed to whichever channel resolves it.
        self.held_tokens += charge;
    }

    fn tool_call(&mut self, call: &Value) {
        let function = call.get("function");
        let name = function
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if name.is_empty() {
            return;
        }
        let id = call.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
        let raw = function.and_then(|f| f.get("arguments")).cloned().unwrap_or(Value::Null);
        // The tool's own spelling, then its schema: the model's `Read` and `"20"` are repaired
        // and counted; an unreadable argument string is passed on raw for the loop to report.
        let (name, outcome) = repair_call(&name, &raw, self.tools.as_deref(), self.stats.as_deref());
        let args = match outcome.into_value() {
            Value::String(s) => s,
            v => v.to_string(),
        };
        let index = self.next_call;
        self.next_call += 1;
        self.emit(StreamEvent::ToolCallStart { index, id, name });
        self.emit(StreamEvent::ToolArgsDelta { index, json: args });
        self.emit(StreamEvent::ToolCallEnd { index });
    }

    /// Resolves held text, settles tokens, recovers leaked calls and returns the completion.
    pub fn finish(mut self) -> Completion {
        for seg in self.splitter.finish().segments {
            match seg {
                Segment::Reasoning(r) => self.reasoning(&r, 0),
                Segment::Speech(t) => {
                    if self.closed {
                        self.speech(&t, 0);
                    } else {
                        self.held.push_str(&t);
                    }
                }
            }
        }

        // A call that ends in a tool call has not answered; what it said on the way is narration.
        if !self.held.is_empty() {
            let held = std::mem::take(&mut self.held);
            let owed = std::mem::take(&mut self.held_tokens);
            if self.next_call == 0 && (self.closed || !self.splitter.saw_any_tag()) {
                self.speech(&held, owed);
            } else {
                self.reasoning(&held, owed);
            }
        }
        self.reasoning_tokens += std::mem::take(&mut self.held_tokens);

        if self.reasoning_tokens > 0 {
            let streamed = self.reasoning_tokens + self.speech_tokens;
            let owed = self.usage.output.saturating_sub(streamed);
            if owed > 0 {
                self.reasoning("", owed);
            }
        }

        // What the guard withheld: a call written as text, or text that only looked like one.
        let tail = self.guard.take_held();
        if !tail.is_empty() {
            let set = match &self.tools {
                Some(t) => ToolSet::Known(t),
                None => ToolSet::Any,
            };
            let found = if self.next_call == 0 { scan(&tail, set) } else { Default::default() };
            if found.calls.is_empty() {
                self.speech_now(&tail);
            } else {
                self.speech_now(&found.prose);
                for c in found.calls {
                    if let Some(stats) = &self.stats {
                        stats.record(&c.outcome(), true);
                    }
                    let index = self.next_call;
                    self.next_call += 1;
                    self.emit(StreamEvent::ToolCallStart { index, id: String::new(), name: c.name });
                    self.emit(StreamEvent::ToolArgsDelta { index, json: c.args.to_string() });
                    self.emit(StreamEvent::ToolCallEnd { index });
                }
            }
        }

        let mut usage = self.usage;
        usage.reasoning = self.reasoning_tokens;
        self.emit(StreamEvent::Usage(usage));
        let stop = self.stop.unwrap_or(StopReason::EndTurn);
        self.emit(StreamEvent::Stop(stop));

        let mut c = self.builder.finish();
        c.text = defang_tool_markup(&c.text);
        if let Some(t) = c.thinking.as_mut() {
            t.text = defang_tool_markup(&t.text);
        }
        c
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn run(thinking: bool, frames: &[Value]) -> (Completion, Vec<StreamEvent>) {
        let mut events = Vec::new();
        let mut on = |e: StreamEvent| events.push(e);
        let mut f = Folder::new(thinking, &mut on);
        for fr in frames {
            f.frame(fr).unwrap();
        }
        let c = f.finish();
        (c, events)
    }

    fn reasoning_tokens(events: &[StreamEvent]) -> u64 {
        events
            .iter()
            .map(|e| match e {
                StreamEvent::ReasoningDelta { tokens, .. } => *tokens,
                StreamEvent::Retract { tokens } => *tokens,
                _ => 0,
            })
            .sum()
    }

    #[test]
    fn native_thinking_streams_and_the_engine_count_settles_it() {
        let (c, ev) = run(
            true,
            &[
                json!({"message":{"thinking":"Two"}}),
                json!({"message":{"thinking":" plus two is"}}),
                json!({"message":{"content":"4"}}),
                json!({"message":{"content":""},"done":true,"done_reason":"stop","prompt_eval_count":13,"eval_count":8}),
            ],
        );
        assert_eq!(c.text, "4");
        assert_eq!(c.thinking.unwrap().text, "Two plus two is");
        assert_eq!(reasoning_tokens(&ev), 7, "eval_count 8 minus one answer token");
        assert_eq!(c.usage.input, 13);
        assert_eq!(c.usage.output, 8);
        assert_eq!(c.usage.reasoning, 7);
        assert_eq!(c.stop, StopReason::EndTurn);
    }

    #[test]
    fn a_call_that_never_reasoned_settles_nothing() {
        let (c, ev) = run(
            false,
            &[
                json!({"message":{"content":"4"}}),
                json!({"done":true,"eval_count":2}),
            ],
        );
        assert_eq!(c.text, "4");
        assert!(c.thinking.is_none());
        assert_eq!(reasoning_tokens(&ev), 0);
    }

    #[test]
    fn inline_think_tags_are_split_and_never_shown() {
        let (c, ev) = run(
            false,
            &[json!({"message":{"content":"<think>hm</think>Hi"}}), json!({"done":true})],
        );
        assert_eq!(c.text, "Hi");
        assert_eq!(c.thinking.unwrap().text, "hm");
        assert!(!ev.iter().any(|e| matches!(e, StreamEvent::TextDelta(t) if t.contains("think"))));
    }

    #[test]
    fn a_bare_close_tag_retracts_streamed_speech() {
        let (c, ev) = run(
            false,
            &[
                json!({"message":{"content":"pondering"}}),
                json!({"message":{"content":"</think>Answer"}}),
                json!({"done":true}),
            ],
        );
        assert_eq!(c.text, "Answer");
        assert_eq!(c.thinking.unwrap().text, "pondering");
        assert!(ev.iter().any(|e| matches!(e, StreamEvent::Retract { .. })));
    }

    #[test]
    fn content_is_held_until_the_block_is_known_shut() {
        let (c, ev) = run(
            true,
            &[json!({"message":{"content":"Hello"}}), json!({"done":true,"eval_count":1})],
        );
        assert_eq!(c.text, "Hello", "released at the end when no tag ever appeared");
        assert_eq!(
            ev.iter().filter(|e| matches!(e, StreamEvent::TextDelta(_))).count(),
            1,
            "held, then released once"
        );
    }

    #[test]
    fn every_tool_call_in_a_message_is_kept_with_text() {
        let (c, _) = run(
            false,
            &[
                json!({"message":{"content":"Reading both.","tool_calls":[
                    {"function":{"name":"read","arguments":{"path":"a"}}},
                    {"id":"x","function":{"name":"grep","arguments":{"pattern":"b"}}}
                ]}}),
                json!({"done":true,"done_reason":"stop"}),
            ],
        );
        assert_eq!(c.text, "Reading both.");
        assert_eq!(c.tool_calls.len(), 2);
        assert_eq!(c.tool_calls[0].id, "", "no id from Ollama: the loop assigns one");
        assert_eq!(c.tool_calls[0].args, json!({"path": "a"}));
        assert_eq!(c.tool_calls[1].id, "x");
        assert_eq!(c.stop, StopReason::ToolUse);
    }

    #[test]
    fn a_leaked_markup_call_is_recovered_as_a_structured_call() {
        let (c, _) = run(
            false,
            &[
                json!({"message":{"content":"<function=read><parameter=path>notes.md</parameter></function>"}}),
                json!({"done":true}),
            ],
        );
        assert_eq!(c.tool_calls.len(), 1);
        assert_eq!(c.tool_calls[0].name, "read");
        assert_eq!(c.tool_calls[0].args, json!({"path": "notes.md"}));
        assert!(c.text.is_empty());
        assert!(c.thinking.is_none_or(|t| !t.text.contains("<function")));
    }

    #[test]
    fn length_maps_to_max_tokens_and_errors_surface() {
        let (c, _) = run(false, &[json!({"message":{"content":"x"},"done":true,"done_reason":"length"})]);
        assert_eq!(c.stop, StopReason::MaxTokens);
        let mut on = |_e: StreamEvent| {};
        let mut f = Folder::new(false, &mut on);
        assert!(f.frame(&json!({"error":"model crashed"})).is_err());
    }
}
