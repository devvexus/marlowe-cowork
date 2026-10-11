//! Folds OpenRouter SSE chunks into [`StreamEvent`]s and a [`Completion`].
//!
//! - `delta.reasoning` / `reasoning_content` stream as reasoning; `delta.reasoning_details`
//!   fragments are merged by `index` and attached to the completion for the round trip.
//! - `delta.content` goes through [`ThinkSplitter`], so inline `<think>` blocks never show.
//! - `delta.tool_calls` fragments are joined by `index`; `arguments` arrive as string pieces. A
//!   fragment whose `id` differs from the named call already at its index starts a new call (some
//!   upstreams send every parallel call at index 0).
//! - `usage` arrives on the last chunk; reasoning tokens counted but not streamed are settled
//!   against `completion_tokens_details.reasoning_tokens` when reported.
//! - `url_citation` annotations (server web search) whose URL the text does not already contain
//!   are appended as a short `Sources:` list, since a completion has no field for them.
//! - An `error` object or `finish_reason: "error"` inside a 200 stream is an error.
//! - Tool calls: with the request's tools ([`Accumulator::with_tools`]) the finished calls have
//!   their names and arguments repaired against the schemas (`marlowe_provider::ollama::repair`),
//!   and a call the model wrote as text (Qwen, Hermes, Mistral forms behind OpenRouter) is
//!   withheld from the screen while it streams and recovered at the end (`ollama::scan`). Every
//!   repair is counted in [`RepairStats`] for the bench (A4).

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use marlowe_model::{
    Completion, CompletionBuilder, ProviderError, ProviderErrorKind, StopReason, StreamEvent, ToolSpec, Usage,
};
use marlowe_provider::ollama::repair::{repair_call, RepairStats};
use marlowe_provider::ollama::scan::{scan, LeakGuard, ToolSet};
use marlowe_provider::think::{Segment, ThinkSplitter};
use serde_json::{Map, Value};

/// One tool call being assembled. The name can arrive in pieces.
#[derive(Default)]
struct CallSlot {
    id: String,
    name: String,
    /// A `ToolCallStart` was emitted for it.
    started: bool,
}

pub struct Accumulator<'a> {
    on: &'a mut dyn FnMut(StreamEvent),
    builder: CompletionBuilder,
    splitter: ThinkSplitter,
    /// Tool calls by slot (the index emitted downstream).
    calls: BTreeMap<u32, CallSlot>,
    /// Upstream index -> the slot its id-less fragments continue.
    route: HashMap<u32, u32>,
    /// Call id -> slot.
    by_id: HashMap<String, u32>,
    details: Vec<Value>,
    citations: Vec<(String, String)>,
    reasoning_charged: u64,
    speech_charged: u64,
    usage: Option<Usage>,
    stop: Option<StopReason>,
    tools: Option<Vec<ToolSpec>>,
    guard: LeakGuard,
    stats: Option<Arc<RepairStats>>,
    /// The calls in the completion were read from text (already repaired and counted).
    text_calls: bool,
    /// Anything arrived (text, reasoning, calls). A retry after this would duplicate output.
    pub produced: bool,
}

impl<'a> Accumulator<'a> {
    pub fn new(on: &'a mut dyn FnMut(StreamEvent)) -> Self {
        Self {
            on,
            builder: CompletionBuilder::new(),
            splitter: ThinkSplitter::new(),
            calls: BTreeMap::new(),
            route: HashMap::new(),
            by_id: HashMap::new(),
            details: Vec::new(),
            citations: Vec::new(),
            reasoning_charged: 0,
            speech_charged: 0,
            usage: None,
            stop: None,
            tools: None,
            guard: LeakGuard::new(None),
            stats: None,
            text_calls: false,
            produced: false,
        }
    }

    /// The request's tools: call names and arguments are checked against them, and a call written
    /// as text is accepted only for a tool in the list.
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
        self.produced = true;
        self.reasoning_charged += tokens;
        self.emit(StreamEvent::ReasoningDelta { text: text.to_string(), tokens });
    }

    fn speech(&mut self, text: &str, tokens: u64) {
        self.produced = true;
        self.speech_charged += tokens;
        let shown = self.guard.push(text);
        self.speech_now(&shown);
    }

    /// Speech that has already been judged (no guard).
    fn speech_now(&mut self, text: &str) {
        if !text.is_empty() {
            self.emit(StreamEvent::TextDelta(text.to_string()));
        }
    }

    /// Applies one decoded SSE payload.
    pub fn chunk(&mut self, value: &Value) -> Result<(), ProviderError> {
        if let Some(err) = value.get("error") {
            return Err(stream_error(err));
        }
        if let Some(u) = value.get("usage").filter(|u| u.is_object()) {
            self.usage = Some(parse_usage(u));
        }
        let Some(choice) = value.get("choices").and_then(Value::as_array).and_then(|a| a.first()) else {
            return Ok(());
        };
        if let Some(err) = choice.get("error") {
            return Err(stream_error(err));
        }
        let delta = choice.get("delta").or_else(|| choice.get("message"));
        if let Some(delta) = delta {
            self.delta(delta);
        }
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.stop = Some(match reason {
                "stop" | "end_turn" => StopReason::EndTurn,
                "tool_calls" | "tool_use" | "function_call" => StopReason::ToolUse,
                "length" | "max_tokens" => StopReason::MaxTokens,
                "content_filter" => StopReason::ContentFilter,
                "error" => {
                    return Err(ProviderError::server("OpenRouter ended the stream with finish_reason \"error\""))
                }
                _ => StopReason::EndTurn,
            });
        }
        Ok(())
    }

    fn delta(&mut self, delta: &Value) {
        let mut charged = false;
        for field in ["reasoning", "reasoning_content"] {
            if let Some(r) = delta.get(field).and_then(Value::as_str) {
                if !r.is_empty() {
                    let charge = u64::from(!charged);
                    charged = true;
                    self.reasoning(r, charge);
                }
            }
        }
        if let Some(details) = delta.get("reasoning_details").and_then(Value::as_array) {
            for d in details {
                merge_detail(&mut self.details, d);
            }
            // Summaries and text that only arrive here still belong on screen.
            if !charged {
                let text: String = details
                    .iter()
                    .filter_map(|d| d.get("text").or_else(|| d.get("summary")).and_then(Value::as_str))
                    .collect();
                if !text.is_empty() {
                    charged = true;
                    self.reasoning(&text, 1);
                }
            }
        }
        if let Some(c) = delta.get("content").and_then(Value::as_str) {
            if !c.is_empty() {
                self.content(c, u64::from(!charged));
            }
        }
        if let Some(notes) = delta.get("annotations").and_then(Value::as_array) {
            for n in notes {
                let Some(c) = n.get("url_citation") else { continue };
                let Some(url) = c.get("url").and_then(Value::as_str).filter(|u| !u.is_empty()) else {
                    continue;
                };
                if !self.citations.iter().any(|(u, _)| u == url) {
                    let title = c.get("title").and_then(Value::as_str).unwrap_or("").trim().to_string();
                    self.citations.push((url.to_string(), title));
                }
            }
        }
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for (position, call) in calls.iter().enumerate() {
                self.tool_fragment(position, call);
            }
        }
    }

    fn content(&mut self, c: &str, mut charge: u64) {
        let split = self.splitter.feed(c);
        if split.retract_speech {
            // Text the guard was still holding was reasoning too.
            let withheld = self.guard.reset();
            if !withheld.is_empty() {
                self.reasoning(&withheld, 0);
            }
            if !self.builder.text().is_empty() {
                let owed = std::mem::take(&mut self.speech_charged);
                self.reasoning_charged += owed;
                self.emit(StreamEvent::Retract { tokens: owed });
            }
        }
        for seg in split.segments {
            let owed = std::mem::take(&mut charge);
            match seg {
                Segment::Reasoning(r) => self.reasoning(&r, owed),
                Segment::Speech(t) => self.speech(&t, owed),
            }
        }
    }

    /// The slot a fragment belongs to: its id's slot, else the slot its upstream index routes to,
    /// unless that slot is a different, already named call; then a fresh slot.
    fn slot_for(&mut self, upstream: u32, id: &str) -> u32 {
        if id.is_empty() {
            return self.route.get(&upstream).copied().unwrap_or(upstream);
        }
        if let Some(&slot) = self.by_id.get(id) {
            self.route.insert(upstream, slot);
            return slot;
        }
        let want = self.route.get(&upstream).copied().unwrap_or(upstream);
        let taken = self.calls.get(&want).is_some_and(|c| !c.id.is_empty() && c.id != id && !c.name.is_empty());
        let slot = if taken {
            let used = self.calls.keys().chain(self.route.keys()).chain(self.route.values());
            used.copied().max().map_or(0, |m| m.saturating_add(1))
        } else {
            want
        };
        self.calls.entry(slot).or_default().id = id.to_string();
        self.by_id.insert(id.to_string(), slot);
        self.route.insert(upstream, slot);
        slot
    }

    fn tool_fragment(&mut self, position: usize, call: &Value) {
        self.produced = true;
        let upstream = call
            .get("index")
            .and_then(Value::as_u64)
            .map(|i| i.min(u32::MAX as u64) as u32)
            .unwrap_or(position as u32);
        let id = call.get("id").and_then(Value::as_str).unwrap_or("").to_string();
        let function = call.get("function");
        let name_piece = function.and_then(|f| f.get("name")).and_then(Value::as_str).unwrap_or("");
        let index = self.slot_for(upstream, &id);
        let slot = self.calls.entry(index).or_default();
        let first = !std::mem::replace(&mut slot.started, true);
        slot.name.push_str(name_piece);
        let name = slot.name.clone();
        if first || !id.is_empty() || !name_piece.is_empty() {
            self.emit(StreamEvent::ToolCallStart { index, id, name });
        }
        if let Some(args) = function.and_then(|f| f.get("arguments")) {
            let json = match args {
                Value::String(s) => s.clone(),
                Value::Null => String::new(),
                other => other.to_string(),
            };
            if !json.is_empty() {
                self.emit(StreamEvent::ToolArgsDelta { index, json });
            }
        }
    }

    /// Flushes buffers, settles usage and returns the completion.
    pub fn finish(mut self) -> Completion {
        for seg in self.splitter.finish().segments {
            match seg {
                Segment::Reasoning(r) => self.reasoning(&r, 0),
                Segment::Speech(t) => self.speech(&t, 0),
            }
        }
        // What the guard withheld: a call written as text, or text that only looked like one.
        let tail = self.guard.take_held();
        if !tail.is_empty() {
            let set = match &self.tools {
                Some(t) => ToolSet::Known(t),
                None => ToolSet::Any,
            };
            let found = if self.calls.is_empty() { scan(&tail, set) } else { Default::default() };
            if found.calls.is_empty() {
                self.speech_now(&tail);
            } else {
                self.speech_now(&found.prose);
                for (index, c) in found.calls.into_iter().enumerate() {
                    let index = index as u32;
                    if let Some(stats) = &self.stats {
                        stats.record(&c.outcome(), true);
                    }
                    self.produced = true;
                    self.text_calls = true;
                    self.calls.entry(index).or_default().started = true;
                    self.emit(StreamEvent::ToolCallStart { index, id: String::new(), name: c.name });
                    self.emit(StreamEvent::ToolArgsDelta { index, json: c.args.to_string() });
                }
            }
        }
        let missing: Vec<(String, String)> = std::mem::take(&mut self.citations)
            .into_iter()
            .filter(|(url, _)| !self.builder.text().contains(url.as_str()))
            .collect();
        if !missing.is_empty() {
            let mut list = String::from("

Sources:");
            for (url, title) in missing {
                let label = if title.is_empty() { url.clone() } else { title.replace(['[', ']'], "") };
                list.push_str(&format!("
- [{label}]({url})"));
            }
            self.speech_now(&list);
        }
        let indices: Vec<u32> = self.calls.keys().copied().collect();
        for index in indices {
            self.emit(StreamEvent::ToolCallEnd { index });
        }
        if let Some(mut usage) = self.usage {
            if usage.reasoning > self.reasoning_charged {
                let owed = usage.reasoning - self.reasoning_charged;
                self.reasoning("", owed);
            }
            if usage.reasoning == 0 {
                usage.reasoning = self.reasoning_charged;
            }
            self.emit(StreamEvent::Usage(usage));
        }
        let stop = self.stop.unwrap_or(StopReason::EndTurn);
        self.emit(StreamEvent::Stop(stop));
        let details = std::mem::take(&mut self.details);
        let (tools, stats, text_calls) = (self.tools.take(), self.stats.take(), self.text_calls);
        let mut b = self.builder;
        if !details.is_empty() {
            b.set_details(Value::Array(details));
        }
        let mut done = b.finish();
        if !text_calls {
            repair_calls(&mut done, tools.as_deref(), stats.as_deref());
        }
        done
    }
}

/// Puts each structured call's name and arguments right against the request's tools and counts
/// what had to change. Arguments that cannot be read stay as the raw string for the loop to report.
fn repair_calls(done: &mut Completion, tools: Option<&[ToolSpec]>, stats: Option<&RepairStats>) {
    if tools.is_none() && stats.is_none() {
        return;
    }
    for call in &mut done.tool_calls {
        let (name, outcome) = repair_call(&call.name, &call.args, tools, stats);
        call.name = name;
        call.args = outcome.into_value();
    }
}

fn parse_usage(u: &Value) -> Usage {
    let n = |v: Option<&Value>| v.and_then(Value::as_u64).unwrap_or(0);
    Usage {
        input: n(u.get("prompt_tokens")),
        output: n(u.get("completion_tokens")),
        reasoning: n(u.get("completion_tokens_details").and_then(|d| d.get("reasoning_tokens"))),
        cached_input: n(u.get("prompt_tokens_details").and_then(|d| d.get("cached_tokens"))),
        cost_usd: u.get("cost").and_then(Value::as_f64),
    }
}

/// Merges one streamed `reasoning_details` fragment: same `index` concatenates the text fields
/// and overwrites the rest; no `index` appends.
fn merge_detail(acc: &mut Vec<Value>, d: &Value) {
    let Some(obj) = d.as_object() else { return };
    let index = obj.get("index").and_then(Value::as_u64);
    let existing = index.and_then(|i| {
        acc.iter_mut().find(|e| e.get("index").and_then(Value::as_u64) == Some(i))
    });
    match existing.and_then(Value::as_object_mut) {
        Some(e) => merge_into(e, obj),
        None => acc.push(d.clone()),
    }
}

fn merge_into(e: &mut Map<String, Value>, obj: &Map<String, Value>) {
    for (k, v) in obj {
        match (k.as_str(), e.get_mut(k), v) {
            ("text" | "summary" | "data", Some(Value::String(prev)), Value::String(s)) => prev.push_str(s),
            (_, _, Value::Null) => {}
            _ => {
                e.insert(k.clone(), v.clone());
            }
        }
    }
}

fn stream_error(err: &Value) -> ProviderError {
    let message = err
        .get("message")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| err.to_string());
    let code = err.get("code").and_then(|c| c.as_u64().or_else(|| c.as_str()?.parse().ok()));
    let detail = format!("OpenRouter reported mid-stream: {message}");
    match code {
        Some(c) if (400..600).contains(&c) => ProviderError::from_status(c as u16, detail),
        _ => ProviderError::new(ProviderErrorKind::Server, detail),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fold(chunks: &[Value]) -> (Result<Completion, ProviderError>, Vec<StreamEvent>) {
        let mut events = Vec::new();
        let mut on = |e: StreamEvent| events.push(e);
        let mut acc = Accumulator::new(&mut on);
        for c in chunks {
            if let Err(e) = acc.chunk(c) {
                drop(acc);
                return (Err(e), events);
            }
        }
        let c = acc.finish();
        (Ok(c), events)
    }

    #[test]
    fn reasoning_details_fragments_merge_by_index() {
        let (c, _) = fold(&[
            json!({"choices":[{"delta":{"reasoning":"Let me","reasoning_details":[{"type":"reasoning.text","text":"Let me","index":0,"format":"anthropic-claude-v1"}]}}]}),
            json!({"choices":[{"delta":{"reasoning":" think","reasoning_details":[{"type":"reasoning.text","text":" think","index":0}]}}]}),
            json!({"choices":[{"delta":{"reasoning_details":[{"type":"reasoning.text","signature":"sig","index":0}]}}]}),
            json!({"choices":[{"delta":{"content":"Done."},"finish_reason":"stop"}]}),
        ]);
        let c = c.unwrap();
        let t = c.thinking.unwrap();
        assert_eq!(t.text, "Let me think");
        assert_eq!(
            t.details.unwrap(),
            json!([{"type":"reasoning.text","text":"Let me think","index":0,"format":"anthropic-claude-v1","signature":"sig"}])
        );
        assert_eq!(c.text, "Done.");
    }

    #[test]
    fn tool_call_fragments_assemble_by_index() {
        let (c, events) = fold(&[
            json!({"choices":[{"delta":{"content":"Checking.","tool_calls":[{"index":0,"id":"call_a","type":"function","function":{"name":"read","arguments":""}}]}}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"pa"}}]}}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":1,"id":"call_b","function":{"name":"grep","arguments":"{}"}}]}}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"a.md\"}"}}]}}]}),
            json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":10,"completion_tokens":20,"cost":0.0012}}),
        ]);
        let c = c.unwrap();
        assert_eq!(c.text, "Checking.");
        assert_eq!(c.tool_calls.len(), 2);
        assert_eq!(c.tool_calls[0].id, "call_a");
        assert_eq!(c.tool_calls[0].args, json!({"path": "a.md"}));
        assert_eq!(c.tool_calls[1].name, "grep");
        assert_eq!(c.stop, StopReason::ToolUse);
        assert_eq!(c.usage.input, 10);
        assert_eq!(c.usage.cost_usd, Some(0.0012));
        assert_eq!(events.iter().filter(|e| matches!(e, StreamEvent::ToolCallEnd { .. })).count(), 2);
    }

    #[test]
    fn parallel_calls_sharing_index_zero_stay_separate() {
        let (c, events) = fold(&[
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_a","type":"function","function":{"name":"read","arguments":"{\"path\":"}}]}}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a.md\"}"}}]}}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_b","type":"function","function":{"name":"grep","arguments":"{\"pattern\":"}}]}}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"x\"}"}}]}}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":1,"id":"call_c","function":{"name":"glob","arguments":"{}"}}]}}]}),
            json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
        ]);
        let c = c.unwrap();
        let got: Vec<(&str, &str, Value)> =
            c.tool_calls.iter().map(|t| (t.id.as_str(), t.name.as_str(), t.args.clone())).collect();
        assert_eq!(
            got,
            vec![
                ("call_a", "read", json!({"path": "a.md"})),
                ("call_b", "grep", json!({"pattern": "x"})),
                ("call_c", "glob", json!({})),
            ]
        );
        assert_eq!(events.iter().filter(|e| matches!(e, StreamEvent::ToolCallEnd { .. })).count(), 3);
    }

    #[test]
    fn reported_reasoning_tokens_settle_the_count() {
        let (c, events) = fold(&[
            json!({"choices":[{"delta":{"reasoning":"a"}}]}),
            json!({"choices":[{"delta":{"content":"b"},"finish_reason":"stop"}]}),
            json!({"choices":[],"usage":{"prompt_tokens":5,"completion_tokens":30,"completion_tokens_details":{"reasoning_tokens":25}}}),
        ]);
        let total: u64 = events
            .iter()
            .map(|e| if let StreamEvent::ReasoningDelta { tokens, .. } = e { *tokens } else { 0 })
            .sum();
        assert_eq!(total, 25);
        assert_eq!(c.unwrap().usage.reasoning, 25);
    }

    #[test]
    fn mid_stream_errors_are_errors() {
        let (r, _) = fold(&[
            json!({"choices":[{"delta":{"content":"par"}}]}),
            json!({"error":{"code":502,"message":"upstream died"}}),
        ]);
        let e = r.unwrap_err();
        assert_eq!(e.kind, ProviderErrorKind::Server);
        assert!(e.detail.contains("upstream died"));

        let (r, _) = fold(&[json!({"choices":[{"delta":{"content":""},"finish_reason":"error"}]})]);
        assert!(r.is_err());
    }

    #[test]
    fn uncited_url_citations_are_appended_as_sources() {
        let (c, _) = fold(&[
            json!({"choices":[{"delta":{"content":"See https://a.example/x."}}]}),
            json!({"choices":[{"delta":{"annotations":[
                {"type":"url_citation","url_citation":{"url":"https://a.example/x","title":"A"}},
                {"type":"url_citation","url_citation":{"url":"https://b.example/y","title":"B [beta]"}}
            ]},"finish_reason":"stop"}]}),
        ]);
        let text = c.unwrap().text;
        assert!(text.ends_with("Sources:
- [B beta](https://b.example/y)"), "{text}");
        assert_eq!(text.matches("a.example").count(), 1);
    }

    #[test]
    fn inline_think_tags_are_reasoning() {
        let (c, _) = fold(&[
            json!({"choices":[{"delta":{"content":"<think>hm</thi"}}]}),
            json!({"choices":[{"delta":{"content":"nk>Answer"},"finish_reason":"stop"}]}),
        ]);
        let c = c.unwrap();
        assert_eq!(c.text, "Answer");
        assert_eq!(c.thinking.unwrap().text, "hm");
    }
}
