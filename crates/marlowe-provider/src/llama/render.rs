//! Does each request extend the last one, as the model's own chat template renders it?
//!
//! A local model reuses its prompt cache only for an exact prefix, and what it reads is not the
//! messages but the template's rendering of them. Some templates print a different text for the
//! same message once something follows it (Qwen's prints an assistant turn's reasoning only after
//! the newest user message), so a history that is a perfect prefix message for message can still
//! render as a different prompt. llama-server renders any request without running it
//! (`POST /apply-template`), so this checks the rendering itself.
//!
//! After a call, the conversation plus the model's reply is rendered with a tool result after it
//! (a template prints a reply's reasoning while a tool result follows it, which is also how the
//! reply stays in the prompt when the model calls tools) and cut where the follower begins: that
//! is the prompt the model's cache now holds. The next request of the same conversation renders
//! to something that must start with it. When it does not, the first differing byte and what was
//! there are reported ([`marlowe_model::prefix::report_render_break`]), and the agent loop logs
//! it as a bug unless it compacted on purpose. The check never fails a request: a server that
//! cannot render, or a template that refuses the follower, only means nothing is checked.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{json, Map, Value};

use crate::http::{self, LocalEndpoint};

const RENDER_TIMEOUT: Duration = Duration::from_secs(10);
/// Characters of the prompt shown either side of a difference.
const SNIPPET: usize = 70;
/// Characters of a reply's end used to find where it stops in the rendering.
const NEEDLE: usize = 32;

/// What the model's cache holds for one conversation after its last reply.
struct Held {
    /// The messages of the request: the next request is compared only when it starts with exactly
    /// these and an assistant message follows (another conversation that began the same way, a
    /// rewind or a compaction is not a rendering question, and the reply itself is exactly what
    /// is being checked: a re-sorted argument list or a trimmed reasoning must show up).
    messages: Vec<Value>,
    /// The rendering of them.
    prompt: String,
}

/// Remembers, per conversation, the prompt the model's cache holds after its last reply.
#[derive(Default)]
pub struct RenderGuard {
    held: Mutex<HashMap<(String, u64), Held>>,
}

/// `MARLOWE_PREFIX_RENDER_CHECK=0` turns the check off.
fn enabled() -> bool {
    std::env::var_os("MARLOWE_PREFIX_RENDER_CHECK").is_none_or(|v| v != "0")
}

impl RenderGuard {
    /// Before a request: its rendering must start with the prompt held for `key`. A difference is
    /// reported for `model`'s conversation `key`.
    pub fn before(&self, endpoint: &LocalEndpoint, model: &str, key: u64, body: &Value) {
        if !enabled() {
            return;
        }
        let Some(now_messages) = body.get("messages").and_then(Value::as_array) else { return };
        let held = {
            let map = self.held.lock().unwrap_or_else(|p| p.into_inner());
            let Some(h) = map.get(&(model.to_string(), key)) else { return };
            let n = h.messages.len();
            let extends = now_messages.len() > n && now_messages[..n] == h.messages[..] && now_messages[n]["role"] == "assistant";
            if !extends {
                return;
            }
            h.prompt.clone()
        };
        let Some(now) = render(endpoint, body, &[], true) else { return };
        if now.starts_with(&held) {
            return;
        }
        let at = common_prefix(&held, &now);
        let mut detail = describe(&held, &now, at);
        if template_dropped_reasoning(now_messages, &held, &now) {
            detail.insert_str(0, marlowe_model::prefix::TEMPLATE_DROP);
        }
        marlowe_model::prefix::report_render_break(model, key, detail);
    }

    /// After a successful call: what the model's cache holds now. `reply` is the model's reply as
    /// the next request will carry it (an OpenAI-shaped assistant message appended to `body`'s
    /// messages), `tail` the end of its text or reasoning, and `calls` whether it called tools.
    #[allow(clippy::too_many_arguments)]
    pub fn after(&self, endpoint: &LocalEndpoint, model: &str, key: u64, body: &Value, reply: &Value, tail: &str, calls: bool) {
        if !enabled() {
            return;
        }
        let ideal = ideal_prompt(endpoint, body, reply, tail, calls);
        let mut held = self.held.lock().unwrap_or_else(|p| p.into_inner());
        if held.len() > 256 {
            held.clear();
        }
        match ideal {
            Some(prompt) => {
                let messages = body.get("messages").and_then(Value::as_array).cloned().unwrap_or_default();
                held.insert((model.to_string(), key), Held { messages, prompt });
            }
            None => {
                held.remove(&(model.to_string(), key));
            }
        }
    }

    /// Forgets a conversation (a server restart empties the cache it described).
    pub fn forget_all(&self) {
        self.held.lock().unwrap_or_else(|p| p.into_inner()).clear();
    }
}

/// Whether the template, not the request, removed reasoning: some reasoning the request carries
/// was in the prompt held for the cache and is not in this one.
fn template_dropped_reasoning(messages: &[Value], held: &str, now: &str) -> bool {
    messages.iter().filter(|m| m["role"] == "assistant").any(|m| {
        let r = m.get("reasoning_content").and_then(Value::as_str).unwrap_or("").trim();
        !r.is_empty() && held.contains(r) && !now.contains(r)
    })
}

fn ideal_prompt(endpoint: &LocalEndpoint, body: &Value, reply: &Value, tail: &str, calls: bool) -> Option<String> {
    let mut texts = Vec::new();
    for content in ["X", "YY"] {
        let follower = json!({"role": "tool", "tool_call_id": follower_id(reply), "name": "audit", "content": content});
        texts.push(render(endpoint, body, &[reply.clone(), follower], false)?);
    }
    let at = common_prefix(&texts[0], &texts[1]);
    let shared = &texts[0][..at];
    if calls {
        // A tool result follows a reply that called tools; its wrapper is part of what is held.
        return Some(shared.to_string());
    }
    // A final answer is followed by a user message, whose wrapper is not the tool result's: keep
    // the rendering up to where the answer ends.
    let needle: String = {
        let t = tail.trim_end();
        let n = t.chars().count();
        t.chars().skip(n.saturating_sub(NEEDLE)).collect()
    };
    if needle.trim().is_empty() {
        return None;
    }
    shared.rfind(&needle).map(|i| shared[..i + needle.len()].to_string())
}

fn follower_id(reply: &Value) -> String {
    reply
        .get("tool_calls")
        .and_then(Value::as_array)
        .and_then(|c| c.first())
        .and_then(|c| c.get("id"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .unwrap_or("audit")
        .to_string()
}

/// The request's messages (plus `extra`) rendered by the model's template.
fn render(endpoint: &LocalEndpoint, body: &Value, extra: &[Value], generation_prompt: bool) -> Option<String> {
    let mut probe = Map::new();
    let mut messages = body.get("messages")?.as_array()?.clone();
    messages.extend(extra.iter().cloned());
    probe.insert("messages".into(), Value::Array(messages));
    for k in ["tools", "chat_template_kwargs"] {
        if let Some(v) = body.get(k) {
            probe.insert(k.into(), v.clone());
        }
    }
    if !generation_prompt {
        probe.insert("add_generation_prompt".into(), json!(false));
    }
    let v = http::post_json(endpoint, "/apply-template", &Value::Object(probe), RENDER_TIMEOUT).ok()?;
    v.get("prompt").and_then(Value::as_str).map(str::to_string)
}

/// Length in bytes of the longest common prefix, on a character boundary.
fn common_prefix(a: &str, b: &str) -> usize {
    let mut n = 0;
    for (x, y) in a.chars().zip(b.chars()) {
        if x != y {
            break;
        }
        n += x.len_utf8();
    }
    n
}

fn clip(s: &str, from: usize, forward: bool) -> String {
    let text: String = if forward {
        s[from..].chars().take(SNIPPET).collect()
    } else {
        let head = &s[..from];
        let n = head.chars().count();
        head.chars().skip(n.saturating_sub(SNIPPET)).collect()
    };
    text.replace('\n', "\\n")
}

fn describe(held: &str, now: &str, at: usize) -> String {
    format!(
        "the prompt no longer starts with the last one plus its reply: it differs at byte {at} of {} held (after \"{}\": was \"{}\", now \"{}\")",
        held.len(),
        clip(held, at, false),
        clip(held, at, true),
        clip(now, at, true),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llama::testkit::{chatml, Handler, Http, Reply};
    use std::sync::Arc;

    fn server(keep: bool) -> (Http, LocalEndpoint) {
        let handler: Arc<Handler> = Arc::new(move |_, path, body| match path {
            "/apply-template" => Reply::json(json!({"prompt": chatml(body, keep)})),
            _ => Reply::status(404, json!({})),
        });
        let http = Http::start(handler);
        let endpoint = http.endpoint();
        (http, endpoint)
    }

    fn body(messages: Vec<Value>) -> Value {
        json!({"messages": messages, "chat_template_kwargs": {"enable_thinking": true, "preserve_thinking": true}})
    }

    fn user(t: &str) -> Value {
        json!({"role": "user", "content": t})
    }

    fn reply(thinking: &str, text: &str) -> Value {
        json!({"role": "assistant", "content": text, "reasoning_content": thinking})
    }

    fn run(keep: bool) -> Vec<String> {
        let (_http, ep) = server(keep);
        let g = RenderGuard::default();
        let key = 7_001 + u64::from(keep);
        let model = format!("render-test-{keep}");
        let first = body(vec![user("one")]);
        g.before(&ep, &model, key, &first);
        g.after(&ep, &model, key, &first, &reply("pondering", "answer one"), "answer one", false);
        // The next request carries the reply and a new user message.
        let second = body(vec![user("one"), reply("pondering", "answer one"), user("two")]);
        g.before(&ep, &model, key, &second);
        marlowe_model::prefix::take_render_breaks(&model, key)
    }

    #[test]
    fn a_template_that_keeps_earlier_reasoning_extends_the_prompt() {
        assert!(run(true).is_empty());
    }

    #[test]
    fn a_template_that_drops_earlier_reasoning_is_named_with_the_byte() {
        let breaks = run(false);
        assert_eq!(breaks.len(), 1, "{breaks:?}");
        assert!(breaks[0].contains("differs at byte") && breaks[0].contains("pondering"), "{}", breaks[0]);
    }

    #[test]
    fn a_reply_with_tool_calls_is_held_with_its_arguments_in_order() {
        let (_http, ep) = server(true);
        let g = RenderGuard::default();
        let first = body(vec![user("go")]);
        let call = json!({"role": "assistant", "content": "", "reasoning_content": "plan",
            "tool_calls": [{"id": "c1", "index": 0, "type": "function", "function": {"name": "write", "arguments": "{\"path\":\"a\",\"content\":\"b\"}"}}]});
        g.after(&ep, "render-calls", 9_001, &first, &call, "", true);
        let sorted = json!({"role": "assistant", "content": "", "reasoning_content": "plan",
            "tool_calls": [{"id": "c1", "index": 0, "type": "function", "function": {"name": "write", "arguments": "{\"content\":\"b\",\"path\":\"a\"}"}}]});
        let result = json!({"role": "tool", "tool_call_id": "c1", "name": "write", "content": "ok"});
        g.before(&ep, "render-calls", 9_001, &body(vec![user("go"), sorted, result.clone()]));
        let breaks = marlowe_model::prefix::take_render_breaks("render-calls", 9_001);
        assert_eq!(breaks.len(), 1, "re-sorted argument keys are a break: {breaks:?}");
        let same = json!({"role": "assistant", "content": "", "reasoning_content": "plan",
            "tool_calls": [{"id": "c1", "index": 0, "type": "function", "function": {"name": "write", "arguments": "{\"path\":\"a\",\"content\":\"b\"}"}}]});
        g.before(&ep, "render-calls", 9_001, &body(vec![user("go"), same, result]));
        assert!(marlowe_model::prefix::take_render_breaks("render-calls", 9_001).is_empty());
    }

    #[test]
    fn a_request_that_does_not_start_with_the_held_one_is_not_a_rendering_question() {
        // Another conversation that began the same way, or a rewind: the messages differ, so the
        // message-level watch speaks and this one stays quiet.
        let (_http, ep) = server(false);
        let g = RenderGuard::default();
        let first = body(vec![user("one")]);
        g.after(&ep, "render-other", 8_001, &first, &reply("pondering", "answer one"), "answer one", false);
        g.before(&ep, "render-other", 8_001, &body(vec![user("another start"), reply("something else", "x"), user("two")]));
        g.before(&ep, "render-other", 8_001, &body(vec![user("one")]));
        assert!(marlowe_model::prefix::take_render_breaks("render-other", 8_001).is_empty());
    }

    #[test]
    fn a_server_that_cannot_render_checks_nothing() {
        let ep = LocalEndpoint::new("127.0.0.1", 9).unwrap();
        let g = RenderGuard::default();
        let b = body(vec![user("x")]);
        g.after(&ep, "render-none", 5, &b, &reply("t", "a"), "a", false);
        g.before(&ep, "render-none", 5, &b);
        assert!(marlowe_model::prefix::take_render_breaks("render-none", 5).is_empty());
    }
}
