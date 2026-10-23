//! The request Marlowe sends llama-server and how its answer is read.
//!
//! The body is what Ollama 0.32.5 sends llama-server for a model with no renderer, parser or
//! template of its own (`POST /v1/chat/completions`): the messages and tools as the OpenAI
//! dialect has them, the model's `.params` over Ollama's defaults for every sampler field, and
//! `chat_template_kwargs.enable_thinking`. llama-server applies the GGUF's own chat template and
//! parses the tool calls, so no tool-call parser of Marlowe's is involved.
//!
//! It is built from the body the Ollama path would send (same message rules, typed tool schemas,
//! window check), so a model moving between the two engines sees the same conversation. History
//! goes back exactly as produced, thinking included (`reasoning_content`).
//!
//! The answer is read as the OpenAI stream it is and handed to the same [`Folder`] the Ollama
//! path uses as Ollama-shaped frames, so repair, leak recovery and token accounting are one
//! implementation. The numbers in the last chunk's `timings` are llama-server's own: they become
//! the call's usage (read, cached, decode), and `prompt_progress` chunks become live read
//! progress.

use std::io::BufRead;

use marlowe_model::{CancelToken, ProviderError, StreamEvent};
use serde_json::{json, Map, Value};

use crate::ollama::{Folder, CONTEXT_OVERFLOW};
use crate::ollama_store::Sampling;
use crate::sse::{Frame, SseStream};

/// Ollama's defaults for every sampler field a model's `.params` does not set.
struct Defaults;

impl Defaults {
    const TEMPERATURE: f64 = 0.8;
    const TOP_K: i64 = 40;
    const TOP_P: f64 = 0.9;
    const MIN_P: f64 = 0.0;
    const TYPICAL_P: f64 = 1.0;
    const REPEAT_LAST_N: i64 = 64;
    const REPEAT_PENALTY: f64 = 1.1;
    const PRESENCE_PENALTY: f64 = 0.0;
    const FREQUENCY_PENALTY: f64 = 0.0;
    const SEED: i64 = -1;
    const KEEP: i64 = 4;
}

/// llama-server's body for `planned` (the Ollama-shaped body from `plan_request`) on `slot`.
pub fn chat_body(planned: &Value, sampling: Option<&Sampling>, slot: u32) -> Value {
    let s = sampling.cloned().unwrap_or_default();
    let options = planned.get("options").cloned().unwrap_or(Value::Null);
    let opt_f = |k: &str| options.get(k).and_then(Value::as_f64);
    let messages: Vec<Value> =
        planned.get("messages").and_then(Value::as_array).into_iter().flatten().map(convert_message).collect();
    let mut body = Map::new();
    body.insert("messages".into(), Value::Array(messages));
    body.insert("stream".into(), json!(true));
    body.insert("cache_prompt".into(), json!(true));
    body.insert("return_progress".into(), json!(true));
    body.insert("id_slot".into(), json!(slot));
    body.insert("n_predict".into(), options.get("num_predict").cloned().unwrap_or(json!(-1)));
    body.insert("n_keep".into(), json!(Defaults::KEEP));
    body.insert("temperature".into(), json!(opt_f("temperature").or(s.temperature).unwrap_or(Defaults::TEMPERATURE)));
    body.insert("top_k".into(), json!(s.top_k.unwrap_or(Defaults::TOP_K)));
    body.insert("top_p".into(), json!(s.top_p.unwrap_or(Defaults::TOP_P)));
    body.insert("min_p".into(), json!(s.min_p.unwrap_or(Defaults::MIN_P)));
    body.insert("typical_p".into(), json!(Defaults::TYPICAL_P));
    body.insert("repeat_penalty".into(), json!(s.repeat_penalty.unwrap_or(Defaults::REPEAT_PENALTY)));
    body.insert("repeat_last_n".into(), json!(Defaults::REPEAT_LAST_N));
    body.insert("frequency_penalty".into(), json!(s.frequency_penalty.unwrap_or(Defaults::FREQUENCY_PENALTY)));
    body.insert("presence_penalty".into(), json!(s.presence_penalty.unwrap_or(Defaults::PRESENCE_PENALTY)));
    body.insert("seed".into(), json!(Defaults::SEED));
    if !s.stop.is_empty() {
        body.insert("stop".into(), json!(s.stop));
    }
    if let Some(tools) = planned.get("tools").filter(|t| t.as_array().is_some_and(|a| !a.is_empty())) {
        body.insert("tools".into(), tools.clone());
    }
    // A model that thinks does so unless the request says otherwise.
    let think = planned.get("think");
    let mut kwargs = Map::new();
    kwargs.insert("enable_thinking".into(), json!(match think {
        None | Some(Value::Null) => true,
        Some(Value::Bool(b)) => *b,
        Some(_) => true,
    }));
    if let Some(level) = think.and_then(Value::as_str).filter(|l| !l.is_empty()) {
        kwargs.insert("reasoning_effort".into(), json!(level));
    }
    // What the model has thought stays in the prompt it reads next: the template's own switches
    // for that (`preserve_thinking`, `clear_thinking`) are set, and a template without one ignores
    // them. Without this a template that prints only the newest turn's reasoning rewrites every
    // earlier assistant message when a user message arrives, and the model re-reads the history.
    kwargs.insert("preserve_thinking".into(), json!(true));
    kwargs.insert("clear_thinking".into(), json!(false));
    body.insert("chat_template_kwargs".into(), Value::Object(kwargs));
    if let Some(f) = planned.get("format").filter(|f| !f.is_null()) {
        let format = if f == "json" {
            json!({"type": "json_object"})
        } else {
            json!({"type": "json_schema", "json_schema": {"name": "schema", "schema": f}})
        };
        body.insert("response_format".into(), format);
    }
    Value::Object(body)
}

/// One Ollama-shaped message as an OpenAI one: arguments as a JSON string, thinking as
/// `reasoning_content`, the tool's name beside its result.
fn convert_message(m: &Value) -> Value {
    let mut out = Map::new();
    out.insert("role".into(), m.get("role").cloned().unwrap_or(json!("user")));
    if let Some(id) = m.get("tool_call_id").filter(|v| !v.is_null()) {
        out.insert("tool_call_id".into(), id.clone());
    }
    if let Some(name) = m.get("tool_name").filter(|v| !v.is_null()) {
        out.insert("name".into(), name.clone());
    }
    out.insert("content".into(), m.get("content").cloned().filter(|c| !c.is_null()).unwrap_or(json!("")));
    if let Some(t) = m.get("thinking").and_then(Value::as_str).filter(|t| !t.is_empty()) {
        out.insert("reasoning_content".into(), json!(t));
    }
    if let Some(calls) = m.get("tool_calls").and_then(Value::as_array) {
        let calls: Vec<Value> = calls
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let f = c.get("function").cloned().unwrap_or(Value::Null);
                let args = match f.get("arguments") {
                    Some(Value::String(s)) => s.clone(),
                    Some(v) => v.to_string(),
                    None => "{}".into(),
                };
                json!({
                    "id": c.get("id").cloned().unwrap_or(json!("")),
                    "index": i,
                    "type": "function",
                    "function": {"name": f.get("name").cloned().unwrap_or(Value::Null), "arguments": args},
                })
            })
            .collect();
        out.insert("tool_calls".into(), Value::Array(calls));
    }
    Value::Object(out)
}

/// A tool call being assembled from argument fragments.
#[derive(Default)]
struct Call {
    id: String,
    name: String,
    args: String,
}

/// llama-server's own numbers for one call.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Timings {
    pub prompt_n: u64,
    pub cache_n: u64,
    pub prompt_ms: f64,
    pub predicted_n: u64,
    pub predicted_ms: f64,
}

fn timings(t: &Value) -> Timings {
    let n = |k: &str| t.get(k).and_then(Value::as_u64).unwrap_or(0);
    let f = |k: &str| t.get(k).and_then(Value::as_f64).unwrap_or(0.0);
    Timings { prompt_n: n("prompt_n"), cache_n: n("cache_n"), prompt_ms: f("prompt_ms"), predicted_n: n("predicted_n"), predicted_ms: f("predicted_ms") }
}

/// Reads one SSE stream into the folder. Returns the call's timings.
pub fn read_stream(
    reader: Box<dyn BufRead + Send>,
    folder: &mut Folder<'_>,
    cancel: &CancelToken,
) -> Result<Timings, ProviderError> {
    let mut sse = SseStream::new(reader);
    let mut calls: Vec<Call> = Vec::new();
    let mut finish: Option<String> = None;
    let mut seen = Timings::default();
    let mut timed = false;
    let mut done = false;
    while let Some(frame) = sse.next_frame() {
        if cancel.is_cancelled() {
            return Err(ProviderError::cancelled());
        }
        let data = match frame {
            Ok(Frame::Data(d)) => d,
            Ok(Frame::Done) => {
                done = true;
                break;
            }
            Err(e) => {
                if crate::http::is_cancelled_io(&e) {
                    return Err(ProviderError::cancelled());
                }
                return Err(ProviderError::network(format!("the llama.cpp stream broke: {e}")));
            }
        };
        let v: Value = serde_json::from_str(&data)
            .map_err(|e| ProviderError::server(format!("malformed chunk from llama.cpp: {e}; began: {}", data.chars().take(120).collect::<String>())))?;
        if let Some(err) = v.get("error") {
            return Err(error_from(err));
        }
        if let Some(p) = v.get("prompt_progress") {
            let n = |k: &str| p.get(k).and_then(Value::as_u64).unwrap_or(0);
            folder.passthrough(StreamEvent::ReadProgress { total: n("total"), cached: n("cache"), processed: n("processed") });
        }
        if let Some(t) = v.get("timings") {
            seen = timings(t);
            timed = true;
        }
        let Some(choice) = v.get("choices").and_then(|c| c.get(0)) else { continue };
        if let Some(delta) = choice.get("delta") {
            if let Some(r) = delta.get("reasoning_content").and_then(Value::as_str).filter(|r| !r.is_empty()) {
                folder.frame(&json!({"message": {"thinking": r}}))?;
            }
            if let Some(c) = delta.get("content").and_then(Value::as_str).filter(|c| !c.is_empty()) {
                folder.frame(&json!({"message": {"content": c}}))?;
            }
            for tc in delta.get("tool_calls").and_then(Value::as_array).into_iter().flatten() {
                let i = tc.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                while calls.len() <= i {
                    calls.push(Call::default());
                }
                let call = &mut calls[i];
                if let Some(id) = tc.get("id").and_then(Value::as_str) {
                    call.id = id.to_string();
                }
                if let Some(f) = tc.get("function") {
                    if let Some(n) = f.get("name").and_then(Value::as_str) {
                        call.name.push_str(n);
                    }
                    if let Some(a) = f.get("arguments").and_then(Value::as_str) {
                        call.args.push_str(a);
                    }
                }
            }
        }
        if let Some(r) = choice.get("finish_reason").and_then(Value::as_str) {
            finish = Some(r.to_string());
        }
    }
    if cancel.is_cancelled() {
        return Err(ProviderError::cancelled());
    }
    if !done && finish.is_none() {
        return Err(ProviderError::network("llama.cpp closed the stream before it finished"));
    }
    // The calls are whole now: the folder repairs and reports them as it does Ollama's.
    let calls: Vec<Value> = calls
        .into_iter()
        .filter(|c| !c.name.is_empty())
        .map(|c| {
            let args = serde_json::from_str::<Value>(&c.args).unwrap_or_else(|_| Value::String(c.args.clone()));
            json!({"id": c.id, "function": {"name": c.name, "arguments": args}})
        })
        .collect();
    if !calls.is_empty() {
        folder.frame(&json!({"message": {"tool_calls": calls}}))?;
    }
    let ns = |ms: f64| (ms * 1_000_000.0) as u64;
    folder.frame(&json!({
        "message": {"content": ""},
        "done": true,
        "done_reason": if finish.as_deref() == Some("length") { "length" } else { "stop" },
        "prompt_eval_count": seen.prompt_n + seen.cache_n,
        "cached_tokens": if timed { json!(seen.cache_n) } else { Value::Null },
        "prompt_eval_duration": ns(seen.prompt_ms),
        "eval_count": seen.predicted_n,
        "eval_duration": ns(seen.predicted_ms),
    }))?;
    Ok(seen)
}

/// llama-server's `{"error": {"code", "message", "type"}}`.
pub fn error_from(err: &Value) -> ProviderError {
    let message = err.get("message").and_then(Value::as_str).or_else(|| err.as_str()).unwrap_or("unknown error").to_string();
    let kind = err.get("type").and_then(Value::as_str).unwrap_or_default();
    if kind == "exceed_context_size_error" {
        let n = err.get("n_prompt_tokens").and_then(Value::as_u64);
        let ctx = err.get("n_ctx").and_then(Value::as_u64);
        return ProviderError::bad_request(format!(
            "{CONTEXT_OVERFLOW}: llama.cpp counted {} prompt tokens against a slot of {}. Compact the conversation or raise the model's context.",
            n.map_or("more".to_string(), |n| n.to_string()),
            ctx.map_or("fewer".to_string(), |c| c.to_string()),
        ));
    }
    if crate::ollama::is_tool_parse_error(&message) || message.to_lowercase().contains("failed to parse") {
        return ProviderError::malformed_call(crate::ollama::clip_detail(&message));
    }
    ProviderError::server(format!("llama.cpp reported: {message}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use marlowe_model::{CompletionBuilder, StopReason};
    use std::io::Cursor;

    fn sse(chunks: &[Value]) -> Box<dyn BufRead + Send> {
        let mut s = String::new();
        for c in chunks {
            s.push_str(&format!("data: {c}\n\n"));
        }
        s.push_str("data: [DONE]\n\n");
        Box::new(Cursor::new(s.into_bytes()))
    }

    fn delta(d: Value) -> Value {
        json!({"choices": [{"index": 0, "finish_reason": null, "delta": d}]})
    }

    fn last(finish: &str, timings: Value) -> Value {
        json!({"choices": [{"index": 0, "finish_reason": finish, "delta": {}}], "timings": timings})
    }

    fn run(thinking: bool, chunks: &[Value]) -> (marlowe_model::Completion, Vec<StreamEvent>, Timings) {
        let mut events = Vec::new();
        let mut on = |e: StreamEvent| events.push(e);
        let (c, t) = {
            let mut folder = Folder::new(thinking, &mut on);
            let t = read_stream(sse(chunks), &mut folder, &CancelToken::new()).unwrap();
            (folder.finish(), t)
        };
        (c, events, t)
    }

    #[test]
    fn the_body_carries_the_models_sampler_over_ollamas_defaults() {
        let planned = json!({
            "messages": [{"role": "user", "content": "hi"}],
            "options": {"num_ctx": 32768, "num_predict": 800},
            "think": true,
            "tools": [{"type": "function", "function": {"name": "read", "description": "", "parameters": {}}}],
        });
        let s = Sampling { temperature: Some(1.0), top_k: Some(20), top_p: Some(0.95), presence_penalty: Some(0.0), repeat_penalty: Some(1.0), min_p: Some(0.0), ..Default::default() };
        let b = chat_body(&planned, Some(&s), 3);
        assert_eq!(b["id_slot"], 3);
        assert_eq!(b["cache_prompt"], true);
        assert_eq!(b["n_predict"], 800);
        assert_eq!((b["temperature"].as_f64(), b["top_k"].as_i64(), b["top_p"].as_f64()), (Some(1.0), Some(20), Some(0.95)));
        assert_eq!(b["repeat_penalty"].as_f64(), Some(1.0));
        assert_eq!(b["chat_template_kwargs"]["enable_thinking"], true);
        // Earlier reasoning stays in the prompt wherever the template has a switch for it.
        assert_eq!(b["chat_template_kwargs"]["preserve_thinking"], true);
        assert_eq!(b["chat_template_kwargs"]["clear_thinking"], false);
        assert_eq!(b["tools"][0]["function"]["name"], "read");
        // Without a `.params` layer the defaults are Ollama's.
        let d = chat_body(&planned, None, 0);
        assert_eq!((d["temperature"].as_f64(), d["top_k"].as_i64(), d["repeat_penalty"].as_f64()), (Some(0.8), Some(40), Some(1.1)));
    }

    #[test]
    fn a_request_temperature_wins_and_think_off_reaches_the_template() {
        let planned = json!({"messages": [], "options": {"num_predict": 8, "temperature": 0.0}, "think": false});
        let b = chat_body(&planned, Some(&Sampling { temperature: Some(1.0), ..Default::default() }), 0);
        assert_eq!(b["temperature"].as_f64(), Some(0.0));
        assert_eq!(b["chat_template_kwargs"]["enable_thinking"], false);
        assert!(b.get("tools").is_none());
        let lv = chat_body(&json!({"messages": [], "options": {}, "think": "high"}), None, 0);
        assert_eq!(lv["chat_template_kwargs"]["reasoning_effort"], "high");
    }

    #[test]
    fn history_goes_back_as_produced_with_arguments_as_text_and_thinking_kept() {
        let planned = json!({"messages": [
            {"role": "user", "content": "go"},
            {"role": "assistant", "content": "", "thinking": "I should read it",
             "tool_calls": [{"id": "call_1", "function": {"name": "read", "arguments": {"path": "a.rs", "offset": 2, "limit": 3}}}]},
            {"role": "tool", "content": "fn main() {}", "tool_call_id": "call_1", "tool_name": "read"},
        ], "options": {}});
        let b = chat_body(&planned, None, 0);
        let m = b["messages"].as_array().unwrap();
        assert_eq!(m[1]["reasoning_content"], "I should read it");
        let call = &m[1]["tool_calls"][0];
        assert_eq!((call["id"].as_str(), call["type"].as_str(), call["index"].as_u64()), (Some("call_1"), Some("function"), Some(0)));
        assert_eq!(call["function"]["arguments"], "{\"path\":\"a.rs\",\"offset\":2,\"limit\":3}", "keys in the order the model wrote them");
        assert_eq!((m[2]["tool_call_id"].as_str(), m[2]["name"].as_str()), (Some("call_1"), Some("read")));
    }

    #[test]
    fn text_reasoning_and_the_servers_own_numbers_become_the_calls_usage() {
        let (c, events, t) = run(
            true,
            &[
                json!({"choices": [{"index": 0, "finish_reason": null, "delta": {"role": "assistant", "content": null}}], "prompt_progress": {"total": 900, "cache": 600, "processed": 600, "time_ms": 0}}),
                json!({"choices": [{"index": 0, "finish_reason": null, "delta": {}}], "prompt_progress": {"total": 900, "cache": 600, "processed": 900, "time_ms": 120}}),
                delta(json!({"reasoning_content": "hmm"})),
                delta(json!({"content": "Hello"})),
                last("stop", json!({"cache_n": 600, "prompt_n": 300, "prompt_ms": 120.0, "predicted_n": 12, "predicted_ms": 80.0})),
            ],
        );
        assert_eq!(c.text, "Hello");
        assert_eq!(c.thinking.as_ref().map(|t| t.text.as_str()), Some("hmm"));
        assert_eq!(c.stop, StopReason::EndTurn);
        // The whole prompt is 900 tokens; 600 came from the slot's cache; the read took 120 ms.
        assert_eq!((c.usage.input, c.usage.cached_input, c.usage.prompt_eval), (900, 600, None));
        assert_eq!((c.usage.prefill_ms, c.usage.decode_ms, c.usage.output), (Some(120), Some(80), 12));
        assert_eq!((t.prompt_n, t.cache_n), (300, 600));
        let progress: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ReadProgress { total, cached, processed } => Some((*total, *cached, *processed)),
                _ => None,
            })
            .collect();
        assert_eq!(progress, vec![(900, 600, 600), (900, 600, 900)]);
    }

    #[test]
    fn tool_call_fragments_are_assembled_and_handed_over_whole() {
        let (c, _, _) = run(
            false,
            &[
                delta(json!({"tool_calls": [{"index": 0, "id": "abc", "type": "function", "function": {"name": "read", "arguments": "{"}}]})),
                delta(json!({"tool_calls": [{"index": 0, "function": {"arguments": "\"path\":"}}]})),
                delta(json!({"tool_calls": [{"index": 0, "function": {"arguments": " \"a.rs\"}"}}]})),
                delta(json!({"tool_calls": [{"index": 1, "id": "def", "type": "function", "function": {"name": "grep", "arguments": "{\"q\":\"x\"}"}}]})),
                last("tool_calls", json!({"cache_n": 0, "prompt_n": 50, "prompt_ms": 10.0, "predicted_n": 20, "predicted_ms": 100.0})),
            ],
        );
        assert_eq!(c.tool_calls.len(), 2);
        assert_eq!((c.tool_calls[0].id.as_str(), c.tool_calls[0].name.as_str()), ("abc", "read"));
        assert_eq!(c.tool_calls[0].args, json!({"path": "a.rs"}));
        assert_eq!(c.tool_calls[1].args, json!({"q": "x"}));
        let _ = CompletionBuilder::new();
    }

    #[test]
    fn a_length_stop_is_max_tokens() {
        let (c, _, _) = run(false, &[delta(json!({"content": "abc"})), last("length", json!({"prompt_n": 5, "cache_n": 0, "prompt_ms": 1.0, "predicted_n": 3, "predicted_ms": 1.0}))]);
        assert_eq!(c.stop, StopReason::MaxTokens);
    }

    #[test]
    fn a_context_overflow_error_is_the_tagged_bad_request_the_loop_compacts_on() {
        let e = error_from(&json!({"code": 400, "type": "exceed_context_size_error", "message": "too big", "n_prompt_tokens": 70000, "n_ctx": 65536}));
        assert!(crate::ollama::is_context_overflow(&e), "{e:?}");
        assert!(e.detail.contains("70000") && e.detail.contains("65536"));
        let other = error_from(&json!({"message": "boom"}));
        assert!(!crate::ollama::is_context_overflow(&other));
    }

    #[test]
    fn a_cancelled_read_stops_at_the_next_chunk() {
        let cancel = CancelToken::new();
        cancel.cancel();
        let mut on = |_: StreamEvent| {};
        let mut folder = Folder::new(false, &mut on);
        let r = read_stream(sse(&[delta(json!({"content": "x"}))]), &mut folder, &cancel);
        assert!(r.is_err_and(|e| e.is_cancelled()));
    }
}
