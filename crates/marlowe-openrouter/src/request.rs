//! The `/chat/completions` request body (OpenAI dialect).
//!
//! Differences from Ollama's `/api/chat`, each a live 400/500 somewhere:
//! - `arguments` is a JSON *string*, and every call carries `"type": "function"`;
//! - pairing is `tool_call_id` alone (no `tool_name`);
//! - reasoning goes back as the opaque `reasoning_details` the model produced, verbatim;
//! - one system message at position 0; for `anthropic/` models the cached system blocks carry
//!   `cache_control` breakpoints;
//! - `openrouter:`-prefixed tools are OpenRouter server tools, sent by type;
//! - images are `image_url` content parts (data URIs) on user messages; a tool message cannot
//!   carry one, so the images of a batch of results follow it as one user message;
//! - for `anthropic/` models the last user message carries a `cache_control` breakpoint too;
//! - a temperature is not sent to the reasoning families that reject it ([`fixes_sampling`]).

use marlowe_model::image::{self, BlobStore};
use marlowe_model::{ChatRequest, ImageRef, Role, SystemBlock, ToolSpec};
use marlowe_provider::wire;
use serde_json::{json, Map, Value};

/// What the body needs beyond the request.
pub struct BodyOptions<'a> {
    /// The already-mapped `reasoning` object (`None` omits it).
    pub reasoning: Option<Value>,
    /// Whether the model takes images: `Some(false)` from the catalog turns images into a text
    /// note; `None` (catalog not read) sends them and lets OpenRouter refuse.
    pub vision: Option<bool>,
    /// Where image bytes live; without a store images become a text note.
    pub blobs: Option<&'a dyn BlobStore>,
    /// Long edge images are fitted to.
    pub long_edge: u32,
}

impl<'a> BodyOptions<'a> {
    pub fn new(reasoning: Option<Value>) -> Self {
        Self { reasoning, vision: None, blobs: None, long_edge: image::OPENROUTER_LONG_EDGE }
    }
}

/// Builds the body. `reasoning` is the already-mapped `reasoning` object (`None` omits it).
/// Images become a text note (no store); see [`request_body_with`].
pub fn request_body(req: &ChatRequest, reasoning: Option<Value>) -> Value {
    request_body_with(req, &BodyOptions::new(reasoning))
}

/// The `image_url` content parts for `images`, and the notes for those that cannot be sent.
fn wire_images<'a>(imgs: impl Iterator<Item = &'a ImageRef>, opts: &BodyOptions<'_>) -> (Vec<Value>, Vec<String>) {
    let (mut parts, mut notes) = (Vec::new(), Vec::new());
    for r in imgs {
        match (opts.vision, opts.blobs) {
            (Some(false), _) => notes.push("[image omitted: this model cannot see images]".to_string()),
            (_, None) => notes.push("[image omitted: no image store is configured]".to_string()),
            (_, Some(store)) => match image::wire_image(store, r, opts.long_edge) {
                Ok(w) => parts.push(json!({ "type": "image_url", "image_url": { "url": w.data_uri() } })),
                Err(e) => notes.push(format!("[image unavailable: {e}]")),
            },
        }
    }
    (parts, notes)
}

fn with_notes(mut text: String, notes: &[String]) -> String {
    for n in notes {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(n);
    }
    text
}

/// A user message: a plain string, or text then `image_url` parts when there are images.
fn user_message(text: String, parts: Vec<Value>) -> Value {
    if parts.is_empty() {
        return json!({ "role": "user", "content": text });
    }
    let mut content = Vec::with_capacity(parts.len() + 1);
    if !text.is_empty() {
        content.push(json!({ "type": "text", "text": text }));
    }
    content.extend(parts);
    json!({ "role": "user", "content": content })
}

/// The body for `opts`.
pub fn request_body_with(req: &ChatRequest, opts: &BodyOptions<'_>) -> Value {
    let mut messages = Vec::new();

    let lead = req.messages.iter().take_while(|m| m.role == Role::System).count();
    let mut blocks: Vec<SystemBlock> = req.system.iter().filter(|b| !b.text.trim().is_empty()).cloned().collect();
    for m in &req.messages[..lead] {
        let t = m.joined_text();
        if !t.trim().is_empty() {
            blocks.push(SystemBlock { text: t, cache: false });
        }
    }
    let anthropic = req.model.starts_with("anthropic/");
    if let Some(sys) = system_message(&blocks, anthropic) {
        messages.push(sys);
    }

    // The last user message that starts a turn: where the conversation breakpoint goes.
    let mut last_user: Option<usize> = None;
    for m in &req.messages[lead..] {
        let text = m.joined_text();
        match m.role {
            Role::System => {
                if !text.trim().is_empty() {
                    messages.push(json!({ "role": "system", "content": text }));
                }
            }
            Role::User => {
                if !text.is_empty() || m.images().next().is_some() || m.tool_results_iter().next().is_none() {
                    let (parts, notes) = wire_images(m.images(), opts);
                    last_user = Some(messages.len());
                    messages.push(user_message(with_notes(text, &notes), parts));
                }
            }
            Role::Assistant => {
                let calls: Vec<Value> = m
                    .tool_calls()
                    .map(|c| {
                        json!({
                            "id": c.id,
                            "type": "function",
                            "function": { "name": c.name, "arguments": string_args(&c.args) },
                        })
                    })
                    .collect();
                let mut msg = Map::new();
                msg.insert("role".into(), "assistant".into());
                msg.insert(
                    "content".into(),
                    if text.is_empty() && !calls.is_empty() { Value::Null } else { Value::String(text) },
                );
                if !calls.is_empty() {
                    msg.insert("tool_calls".into(), Value::Array(calls));
                }
                if let Some(details) = m.thinking().and_then(|t| t.details.clone()) {
                    msg.insert("reasoning_details".into(), details);
                }
                messages.push(Value::Object(msg));
            }
            Role::Tool => {
                if m.tool_results_iter().next().is_none() && !text.is_empty() {
                    messages.push(json!({ "role": "user", "content": text }));
                }
            }
        }
        // A tool message carries text only. The images of a batch of results follow the batch as
        // one user message, labelled with the call each came from.
        let mut batch_parts: Vec<Value> = Vec::new();
        let mut batch_labels: Vec<String> = Vec::new();
        for r in m.tool_results_iter() {
            let (parts, notes) = wire_images(r.images(), opts);
            if !parts.is_empty() {
                batch_labels.push(format!("{} image(s) from call {}", parts.len(), r.call_id));
                batch_parts.extend(parts);
            }
            messages.push(json!({
                "role": "tool",
                "tool_call_id": r.call_id,
                "content": with_notes(r.content.clone(), &notes),
            }));
        }
        if !batch_parts.is_empty() {
            messages.push(user_message(format!("[{}]", batch_labels.join("; ")), batch_parts));
        }
    }

    wire::unorphan_tool_messages(&mut messages);
    if anthropic {
        if let Some(i) = last_user {
            mark_cache_breakpoint(&mut messages[i]);
        }
    }

    let mut body = json!({
        "model": req.model,
        "messages": messages,
        "stream": true,
        "usage": { "include": true },
    });
    if let Some(n) = req.max_output_tokens {
        body["max_tokens"] = json!(n);
    }
    if let Some(t) = req.temperature.filter(|_| !fixes_sampling(&req.model)) {
        body["temperature"] = json!(t);
    }
    if let Some(tools) = tools_field(&req.tools) {
        body["tools"] = tools;
    }
    if let Some(r) = &opts.reasoning {
        body["reasoning"] = r.clone();
    }
    body
}

/// Reasoning families that reject a temperature (an upstream 400): OpenAI o-series, grok-3-mini,
/// QwQ and the Qwen3 thinking variants. Knowledge from claw-code's MODEL_COMPATIBILITY.md.
pub fn fixes_sampling(model: &str) -> bool {
    let lowered = model.to_ascii_lowercase();
    let name = lowered.rsplit('/').next().unwrap_or(&lowered);
    let o_series = |p: &str| name.strip_prefix(p).is_some_and(|rest| rest.is_empty() || !rest.starts_with(|c: char| c.is_ascii_alphabetic()));
    o_series("o1")
        || o_series("o3")
        || o_series("o4")
        || name == "grok-3-mini"
        || name.starts_with("qwq")
        || name.starts_with("qwen-qwq")
        || (name.starts_with("qwen3") && name.contains("-thinking"))
}

/// A cache breakpoint on the last content block of a user message, so the next call of a tool
/// loop reads the whole conversation up to it from the cache.
fn mark_cache_breakpoint(msg: &mut Value) {
    let Some(content) = msg.get_mut("content") else { return };
    if let Value::String(text) = content {
        if text.is_empty() {
            return;
        }
        *content = json!([{ "type": "text", "text": std::mem::take(text) }]);
    }
    if let Some(parts) = content.as_array_mut() {
        if let Some(last) = parts.iter_mut().rev().find(|p| p.get("type").and_then(Value::as_str) == Some("text")) {
            last["cache_control"] = json!({ "type": "ephemeral" });
        }
    }
}

/// Server tools (`openrouter:` prefix, e.g. `openrouter:web_search`) are sent as
/// `{"type": name}` plus `input_schema.parameters` when given; OpenRouter runs them itself.
/// Every other tool is a function tool.
fn tools_field(tools: &[ToolSpec]) -> Option<Value> {
    let (server, functions): (Vec<&ToolSpec>, Vec<&ToolSpec>) =
        tools.iter().partition(|t| t.name.starts_with(SERVER_TOOL_PREFIX));
    let functions: Vec<ToolSpec> = functions.into_iter().cloned().collect();
    let mut out = match wire::tools_field(&functions) {
        Some(Value::Array(a)) => a,
        _ => Vec::new(),
    };
    for t in server {
        let mut tool = json!({ "type": t.name });
        if let Some(params) = t.input_schema.get("parameters").filter(|p| p.is_object()) {
            tool["parameters"] = params.clone();
        }
        out.push(tool);
    }
    (!out.is_empty()).then_some(Value::Array(out))
}

/// Tool names with this prefix are OpenRouter server tools.
pub const SERVER_TOOL_PREFIX: &str = "openrouter:";

fn system_message(blocks: &[SystemBlock], cache_control: bool) -> Option<Value> {
    if blocks.is_empty() {
        return None;
    }
    if cache_control && blocks.iter().any(|b| b.cache) {
        let parts: Vec<Value> = blocks
            .iter()
            .map(|b| {
                let mut p = json!({ "type": "text", "text": b.text });
                if b.cache {
                    p["cache_control"] = json!({ "type": "ephemeral" });
                }
                p
            })
            .collect();
        return Some(json!({ "role": "system", "content": parts }));
    }
    let text = blocks.iter().map(|b| b.text.as_str()).collect::<Vec<_>>().join("\n\n");
    Some(json!({ "role": "system", "content": text }))
}

/// `arguments` as a JSON string. A malformed call kept raw is sent as `{}` if it does not parse.
fn string_args(args: &Value) -> String {
    match args {
        Value::String(raw) => match serde_json::from_str::<Value>(raw) {
            Ok(v @ Value::Object(_)) => v.to_string(),
            _ => "{}".to_string(),
        },
        Value::Object(_) => args.to_string(),
        _ => "{}".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use marlowe_model::{Message, Part, Thinking, ToolCall, ToolResult};

    fn convo(model: &str) -> ChatRequest {
        let mut req = ChatRequest::new(
            model,
            vec![
                Message::user("read a.md"),
                Message {
                    role: Role::Assistant,
                    content: vec![
                        Part::Thinking(Thinking {
                            text: "plan".into(),
                            signature: None,
                            details: Some(json!([{"type": "reasoning.encrypted", "data": "xyz", "index": 0}])),
                        }),
                        Part::ToolCall(ToolCall { id: "c1".into(), name: "read".into(), args: json!({"path": "a.md"}) }),
                    ],
                },
                Message::tool_results([ToolResult { call_id: "c1".into(), content: "hello".into(), is_error: false, parts: Vec::new() }]),
            ],
        );
        req.system = vec![
            SystemBlock { text: "core".into(), cache: true },
            SystemBlock { text: "env".into(), cache: false },
        ];
        req.tools = vec![ToolSpec { name: "read".into(), description: "Read".into(), input_schema: json!({"type":"object"}) }];
        req
    }

    #[test]
    fn tool_turns_use_string_arguments_and_round_trip_reasoning_details() {
        let body = request_body(&convo("openai/gpt-5"), Some(json!({"effort": "high"})));
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[0], json!({"role": "system", "content": "core\n\nenv"}));
        let call = &msgs[2]["tool_calls"][0];
        assert_eq!(call["type"], "function");
        assert_eq!(call["function"]["arguments"], "{\"path\":\"a.md\"}");
        assert_eq!(msgs[2]["content"], Value::Null);
        assert_eq!(msgs[2]["reasoning_details"][0]["data"], "xyz");
        assert_eq!(msgs[3], json!({"role": "tool", "tool_call_id": "c1", "content": "hello"}));
        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["tools"][0]["function"]["name"], "read");
        assert!(body.get("max_tokens").is_none());
    }

    #[test]
    fn server_tools_are_sent_by_type() {
        let mut req = ChatRequest::new("google/gemini-2.5-flash", vec![Message::user("news")]);
        req.tools = vec![ToolSpec {
            name: "openrouter:web_search".into(),
            description: "Server-side web search".into(),
            input_schema: json!({"type": "object", "properties": {}}),
        }];
        let body = request_body(&req, None);
        assert_eq!(body["tools"], json!([{"type": "openrouter:web_search"}]));
    }

    #[test]
    fn anthropic_models_get_cache_breakpoints_on_cached_blocks() {
        let body = request_body(&convo("anthropic/claude-sonnet-4.5"), None);
        let sys = &body["messages"][0]["content"];
        assert_eq!(sys[0]["cache_control"]["type"], "ephemeral");
        assert!(sys[1].get("cache_control").is_none());
        assert!(body.get("reasoning").is_none());
    }

    #[test]
    fn an_orphaned_result_is_demoted_and_no_tools_means_no_key() {
        let mut req = ChatRequest::new(
            "x/y",
            vec![Message::tool_results([ToolResult { call_id: "ghost".into(), content: "page".into(), is_error: false, parts: Vec::new() }])],
        );
        req.max_output_tokens = Some(512);
        let body = request_body(&req, None);
        assert_eq!(body["messages"][0]["role"], "user");
        assert!(body.get("tools").is_none());
        assert_eq!(body["max_tokens"], 512);
    }
}
