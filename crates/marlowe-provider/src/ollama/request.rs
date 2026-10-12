//! The `/api/chat` request body.
//!
//! Shape rules, each from a live failure:
//! - one system message at position 0 (several chat templates refuse any other placement);
//! - `tool_calls` on the assistant message with `arguments` as a JSON object, and `tool_name` +
//!   `tool_call_id` on each tool message, or the model cannot pair results with calls;
//! - `thinking` is kept only for the assistant messages of the current user turn, and within a
//!   tool loop only for the current chunk of [`THINKING_CHUNK`] steps. Dropping it step by step
//!   would change the prompt before the new message every call and defeat Ollama's prefix cache
//!   (A14); dropping it in chunks breaks the prefix once per chunk, and reasoning from earlier
//!   turns never comes back (it compounds, and Qwen's own template discards it too);
//! - `num_ctx` always sent (Ollama silently defaults to 2048), and never smaller than the prompt
//!   plus the output the request asks for ([`plan_request`]);
//! - `tools` omitted rather than `[]`;
//! - leaked tool markup defanged so the template cannot choke on it;
//! - images ride on `user` messages as base64 in `images`. A tool result cannot carry one, so
//!   the images of a batch of results follow the batch as one user message.

use std::collections::HashMap;

use marlowe_model::image::{self, BlobStore};
use marlowe_model::{ChatRequest, ImageRef, ProviderError, Role};
use serde_json::{json, Map, Value};

use super::leak::defang_tool_markup;
use crate::wire;

/// Assistant steps of one tool loop that keep their `thinking` together.
pub const THINKING_CHUNK: usize = 8;

/// Characters per token the estimate assumes. Code and JSON run near 3, prose near 4.2; this
/// errs a little high so a request near the window fails here rather than being truncated.
pub const CHARS_PER_TOKEN: f32 = 3.4;

/// The least output worth starting a call for. Below it the request fails before it is sent.
pub const MIN_OUTPUT_TOKENS: u32 = 1_024;

/// Tag at the start of the error detail when a prompt cannot fit its window.
pub const CONTEXT_OVERFLOW: &str = "context_overflow";

/// True for the error [`plan_request`] returns when the prompt does not fit `num_ctx`.
pub fn is_context_overflow(e: &ProviderError) -> bool {
    e.kind == marlowe_model::ProviderErrorKind::BadRequest && e.detail.starts_with(CONTEXT_OVERFLOW)
}

/// What the body needs beyond the request.
pub struct BodyOptions<'a> {
    pub num_ctx: u32,
    /// The already-mapped `think` value (`None` omits it).
    pub think: Option<Value>,
    /// The model takes images.
    pub vision: bool,
    /// Where image bytes live; without a store images become a text note.
    pub blobs: Option<&'a dyn BlobStore>,
    /// Long edge images are fitted to.
    pub long_edge: u32,
}

impl<'a> BodyOptions<'a> {
    pub fn new(num_ctx: u32, think: Option<Value>) -> Self {
        Self { num_ctx, think, vision: false, blobs: None, long_edge: image::OLLAMA_LONG_EDGE }
    }
}

/// A finished body and what the check against the window found.
#[derive(Debug, Clone)]
pub struct Planned {
    pub body: Value,
    /// Estimated prompt tokens (messages, thinking, tool calls, tool schemas, images).
    pub prompt_tokens: u32,
    pub num_predict: u32,
}

/// Builds the body. `think` is the already-mapped `think` value (`None` omits it). Images are
/// replaced by a text note (no store, no vision); see [`request_body_with`].
pub fn request_body(req: &ChatRequest, num_ctx: u32, think: Option<Value>) -> Value {
    request_body_with(req, &BodyOptions::new(num_ctx, think))
}

/// The body for `opts`, with no window check.
pub fn request_body_with(req: &ChatRequest, opts: &BodyOptions<'_>) -> Value {
    build(req, opts).body
}

/// The body, checked against the window: `num_predict` is cut to what is left after the prompt,
/// and the request is refused ([`is_context_overflow`]) when less than [`MIN_OUTPUT_TOKENS`] (or
/// the output asked for, if smaller) remains.
pub fn plan_request(req: &ChatRequest, opts: &BodyOptions<'_>) -> Result<Planned, ProviderError> {
    let mut built = build(req, opts);
    let num_ctx = opts.num_ctx;
    let wanted = built.num_predict;
    let room = num_ctx.saturating_sub(built.prompt_tokens);
    let floor = MIN_OUTPUT_TOKENS.min(wanted);
    if room < floor {
        return Err(ProviderError::bad_request(format!(
            "{CONTEXT_OVERFLOW}: the prompt is about {} tokens and the window is {num_ctx}, leaving {room} for the \
             reply (at least {floor} needed). Compact the conversation or raise the model's context.",
            built.prompt_tokens
        )));
    }
    if room < wanted {
        built.num_predict = room;
        built.body["options"]["num_predict"] = json!(room);
    }
    Ok(Planned { body: built.body, prompt_tokens: built.prompt_tokens, num_predict: built.num_predict })
}

struct Built {
    body: Value,
    prompt_tokens: u32,
    num_predict: u32,
}

fn tokens_of(chars: usize) -> u32 {
    (chars as f32 / CHARS_PER_TOKEN).ceil() as u32
}

/// The images of one message as `(base64 list, notes to append to the text)`.
fn wire_images<'a>(imgs: impl Iterator<Item = &'a ImageRef>, opts: &BodyOptions<'_>, tokens: &mut u32) -> (Vec<String>, Vec<String>) {
    let (mut data, mut notes) = (Vec::new(), Vec::new());
    for r in imgs {
        match (opts.vision, opts.blobs) {
            (false, _) => notes.push("[image omitted: this model cannot see images]".to_string()),
            (true, None) => notes.push("[image omitted: no image store is configured]".to_string()),
            (true, Some(store)) => match image::wire_image(store, r, opts.long_edge) {
                Ok(w) => {
                    *tokens += image::estimate_tokens(w.w, w.h);
                    data.push(w.base64);
                }
                Err(e) => notes.push(format!("[image unavailable: {e}]")),
            },
        }
    }
    (data, notes)
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

fn build(req: &ChatRequest, opts: &BodyOptions<'_>) -> Built {
    let num_ctx = opts.num_ctx;
    let mut messages = Vec::new();
    let mut image_tokens = 0u32;

    // Leading system: the request's blocks plus any system messages before the first turn.
    let mut system = req.system_text();
    let lead = req.messages.iter().take_while(|m| m.role == Role::System).count();
    for m in &req.messages[..lead] {
        let t = m.joined_text();
        if !t.trim().is_empty() {
            if !system.is_empty() {
                system.push_str("\n\n");
            }
            system.push_str(&t);
        }
    }
    if !system.trim().is_empty() {
        messages.push(json!({ "role": "system", "content": system }));
    }

    let mut names: HashMap<String, String> = HashMap::new();
    // Where the current user turn starts, for the thinking rule.
    let mut turn_start = messages.len();
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
                    let (data, notes) = wire_images(m.images(), opts, &mut image_tokens);
                    let mut msg = json!({ "role": "user", "content": with_notes(text, &notes) });
                    if !data.is_empty() {
                        msg["images"] = json!(data);
                    }
                    turn_start = messages.len();
                    messages.push(msg);
                }
            }
            Role::Assistant => {
                let mut msg = Map::new();
                msg.insert("role".into(), "assistant".into());
                msg.insert("content".into(), Value::String(defang_tool_markup(&text)));
                if let Some(t) = m.thinking() {
                    if !t.text.is_empty() {
                        msg.insert("thinking".into(), Value::String(defang_tool_markup(&t.text)));
                    }
                }
                let calls: Vec<Value> = m
                    .tool_calls()
                    .map(|c| {
                        names.insert(c.id.clone(), c.name.clone());
                        let mut call = json!({
                            "function": { "name": c.name, "arguments": object_args(&c.args) }
                        });
                        if !c.id.is_empty() {
                            call["id"] = Value::String(c.id.clone());
                        }
                        call
                    })
                    .collect();
                if !calls.is_empty() {
                    msg.insert("tool_calls".into(), Value::Array(calls));
                }
                messages.push(Value::Object(msg));
            }
            Role::Tool => {
                if m.tool_results_iter().next().is_none() && !text.is_empty() {
                    messages.push(json!({ "role": "user", "content": text }));
                }
            }
        }
        // Results travel as one `tool` message each, whatever message carried them. Their images
        // follow the batch as one user message.
        let mut batch_images: Vec<String> = Vec::new();
        let mut batch_labels: Vec<String> = Vec::new();
        for r in m.tool_results_iter() {
            let (data, notes) = wire_images(r.images(), opts, &mut image_tokens);
            let label = names.get(&r.call_id).map_or_else(|| r.call_id.clone(), |n| format!("`{n}` ({})", r.call_id));
            if !data.is_empty() {
                batch_labels.push(format!("{} image(s) from {label}", data.len()));
                batch_images.extend(data);
            }
            let mut msg = json!({
                "role": "tool",
                "content": with_notes(r.content.clone(), &notes),
                "tool_call_id": r.call_id,
            });
            if let Some(name) = names.get(&r.call_id) {
                msg["tool_name"] = Value::String(name.clone());
            }
            messages.push(msg);
        }
        if !batch_images.is_empty() {
            messages.push(json!({
                "role": "user",
                "content": format!("[{}]", batch_labels.join("; ")),
                "images": batch_images,
            }));
        }
    }

    wire::unorphan_tool_messages(&mut messages);
    drop_old_thinking(&mut messages, turn_start);

    let num_predict = req.max_output_tokens.unwrap_or(num_ctx / 4).min(num_ctx / 2).max(1);
    let mut options = json!({ "num_ctx": num_ctx, "num_predict": num_predict });
    if let Some(t) = req.temperature {
        options["temperature"] = json!(t);
    }

    // Plain-string properties go out typeless (fewer tokens, read fine); the probe's typed arm
    // sets MARLOWE_OLLAMA_TYPED_TOOLS.
    let tools = if std::env::var_os("MARLOWE_OLLAMA_TYPED_TOOLS").is_some_and(|v| v != "0" && !v.is_empty()) {
        wire::tools_field_typed(&req.tools)
    } else {
        wire::tools_field(&req.tools)
    };
    let prompt_tokens = estimate_prompt_tokens(&messages, tools.as_ref()) + image_tokens;

    let mut body = json!({
        "model": req.model,
        "messages": messages,
        "stream": true,
        "options": options,
    });
    if let Some(think) = &opts.think {
        body["think"] = think.clone();
    }
    if let Some(tools) = tools {
        body["tools"] = tools;
    }
    // Structured output: Ollama constrains decoding to the schema.
    if let Some(schema) = &req.format {
        body["format"] = schema.clone();
    }
    Built { body, prompt_tokens, num_predict }
}

/// Removes `thinking` from every assistant message outside the current chunk of the current
/// user turn. `turn_start` is the index of that turn's user message.
fn drop_old_thinking(messages: &mut [Value], turn_start: usize) {
    let in_turn: Vec<usize> = (0..messages.len())
        .filter(|&i| i >= turn_start && messages[i].get("role").and_then(Value::as_str) == Some("assistant"))
        .collect();
    let keep_chunk = in_turn.len().saturating_sub(1) / THINKING_CHUNK;
    for (i, m) in messages.iter_mut().enumerate() {
        if m.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let keep = in_turn.iter().position(|&j| j == i).is_some_and(|k| k / THINKING_CHUNK == keep_chunk);
        if !keep {
            if let Some(obj) = m.as_object_mut() {
                obj.remove("thinking");
            }
        }
    }
}

/// Estimated tokens of the messages and tool schemas as sent (image bytes excluded: they are
/// counted from their size).
fn estimate_prompt_tokens(messages: &[Value], tools: Option<&Value>) -> u32 {
    let mut chars = 0usize;
    let mut overhead = 0u32;
    for m in messages {
        overhead += 8;
        for key in ["content", "thinking"] {
            chars += m.get(key).and_then(Value::as_str).map_or(0, |s| s.chars().count());
        }
        for c in m.get("tool_calls").and_then(Value::as_array).into_iter().flatten() {
            let f = &c["function"];
            chars += f["name"].as_str().map_or(0, str::len) + f["arguments"].to_string().len();
            overhead += 4;
        }
    }
    chars += tools.map_or(0, |t| t.to_string().len());
    tokens_of(chars) + overhead
}

/// Ollama wants an object. A malformed call kept as a raw string is re-parsed or sent as `{}`.
fn object_args(args: &Value) -> Value {
    match args {
        Value::Object(_) => args.clone(),
        Value::String(raw) => match serde_json::from_str::<Value>(raw) {
            Ok(v @ Value::Object(_)) => v,
            _ => json!({}),
        },
        _ => json!({}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use marlowe_model::image::MemoryBlobStore;
    use marlowe_model::{Message, Part, SystemBlock, Thinking, ToolCall, ToolResult, ToolSpec};

    fn convo() -> ChatRequest {
        let mut req = ChatRequest::new(
            "qwen3.5:9b",
            vec![
                Message::user("read a.md"),
                Message {
                    role: Role::Assistant,
                    content: vec![
                        Part::Thinking(Thinking { text: "old plan".into(), ..Default::default() }),
                        Part::Text { text: "Reading.".into() },
                        Part::ToolCall(ToolCall {
                            id: "call_1".into(),
                            name: "read".into(),
                            args: json!({"path": "a.md"}),
                        }),
                    ],
                },
                Message::tool_results([ToolResult {
                    call_id: "call_1".into(),
                    content: "hello".into(),
                    is_error: false,
                    parts: Vec::new(),
                }]),
                Message {
                    role: Role::Assistant,
                    content: vec![
                        Part::Thinking(Thinking { text: "new plan".into(), ..Default::default() }),
                        Part::Text { text: "It says hello.".into() },
                    ],
                },
                Message::user("thanks"),
            ],
        );
        req.system = vec![
            SystemBlock { text: "core".into(), cache: true },
            SystemBlock { text: "<environment/>".into(), cache: false },
        ];
        req
    }

    #[test]
    fn one_system_message_leads_and_tool_results_pair_by_id_and_name() {
        let body = request_body(&convo(), 32_768, Some(json!(true)));
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "core\n\n<environment/>");
        assert_eq!(msgs.iter().filter(|m| m["role"] == "system").count(), 1);
        assert_eq!(msgs[2]["tool_calls"][0]["function"]["arguments"]["path"], "a.md");
        assert_eq!(msgs[2]["tool_calls"][0]["id"], "call_1");
        assert_eq!(msgs[3]["role"], "tool");
        assert_eq!(msgs[3]["tool_name"], "read");
        assert_eq!(msgs[3]["tool_call_id"], "call_1");
        assert_eq!(body["options"]["num_ctx"], 32_768);
        assert_eq!(body["think"], true);
        assert!(body.get("tools").is_none(), "no tools means no key, never []");
    }

    #[test]
    fn reasoning_from_earlier_user_turns_is_not_sent_back() {
        // `convo` ends on a new user message: both assistants belong to the finished turn.
        let body = request_body(&convo(), 32_768, None);
        let msgs = body["messages"].as_array().unwrap();
        assert!(msgs[2].get("thinking").is_none());
        assert!(msgs[4].get("thinking").is_none());
        assert!(body.get("think").is_none());
    }

    #[test]
    fn within_a_tool_loop_the_current_chunk_keeps_its_reasoning() {
        let mut req = convo();
        req.messages.pop(); // the loop is still inside the first turn
        let body = request_body(&req, 32_768, None);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[2]["thinking"], "old plan", "an earlier step of the same turn keeps it");
        assert_eq!(msgs[4]["thinking"], "new plan");
    }

    fn loop_request(steps: usize) -> ChatRequest {
        let mut messages = vec![Message::user("fix the build")];
        for i in 0..steps {
            messages.push(Message {
                role: Role::Assistant,
                content: vec![
                    Part::Thinking(Thinking { text: format!("thought {i}"), ..Default::default() }),
                    Part::ToolCall(ToolCall { id: format!("c{i}"), name: "bash".into(), args: json!({"command": format!("step {i}")}) }),
                ],
            });
            messages.push(Message::tool_results([ToolResult {
                call_id: format!("c{i}"),
                content: format!("out {i}"),
                is_error: false,
                parts: Vec::new(),
            }]));
        }
        ChatRequest::new("dusk", messages)
    }

    /// The A14 mechanism: each step's prompt starts with the previous step's whole prompt, except
    /// at a chunk boundary, where the earlier chunk's reasoning is dropped in one go.
    #[test]
    fn the_prompt_is_a_stable_prefix_from_step_to_step_within_a_chunk() {
        let msgs_of = |steps: usize| request_body(&loop_request(steps), 32_768, None)["messages"].as_array().unwrap().clone();
        let mut breaks = Vec::new();
        for steps in 1..(THINKING_CHUNK * 3) {
            let (prev, next) = (msgs_of(steps), msgs_of(steps + 1));
            if next[..prev.len()] != prev[..] {
                breaks.push(steps + 1);
            }
        }
        // The prefix breaks only when step 9 and step 17 are sent (the first call of a new chunk).
        assert_eq!(breaks, vec![THINKING_CHUNK + 1, 2 * THINKING_CHUNK + 1]);
        // And within a chunk every earlier step still carries its reasoning.
        let m = msgs_of(5);
        assert!(m.iter().filter(|x| x["role"] == "assistant").all(|x| x.get("thinking").is_some()));
        // After a boundary only the new chunk does.
        let m = msgs_of(THINKING_CHUNK + 2);
        let with: usize = m.iter().filter(|x| x["role"] == "assistant" && x.get("thinking").is_some()).count();
        assert_eq!(with, 2);
    }

    #[test]
    fn output_cap_is_bounded_by_the_window_and_tools_are_sent() {
        let mut req = convo();
        req.max_output_tokens = Some(200_000);
        req.tools = vec![ToolSpec {
            name: "read".into(),
            description: "Read".into(),
            input_schema: json!({"type": "object", "properties": {}}),
        }];
        let body = request_body(&req, 32_768, None);
        assert_eq!(body["options"]["num_predict"], 16_384);
        assert_eq!(body["tools"][0]["function"]["name"], "read");
        // Found live: without a cap a 32k window predicts 8,192 tokens, which a thinking model
        // spent before answering. A sized research step asks for its own budget.
        req.max_output_tokens = None;
        assert_eq!(request_body(&req, 32_768, None)["options"]["num_predict"], 8_192);
        req.max_output_tokens = Some(12_288);
        assert_eq!(request_body(&req, 32_768, None)["options"]["num_predict"], 12_288);
    }

    #[test]
    fn leaked_markup_in_history_is_defanged_on_the_way_out() {
        let req = ChatRequest::new(
            "m",
            vec![Message::user("hi"), Message::assistant("<function>read</function>")],
        );
        let body = request_body(&req, 8_192, None);
        let content = body["messages"][1]["content"].as_str().unwrap();
        assert!(!content.contains("<function"), "{content}");
    }

    #[test]
    fn a_schema_rides_as_format_and_is_absent_otherwise() {
        let mut req = convo();
        assert!(request_body(&req, 32_768, None).get("format").is_none());
        req.format = Some(json!({"type": "object", "properties": {"n": {"type": "integer"}}, "required": ["n"]}));
        assert_eq!(request_body(&req, 32_768, None)["format"]["required"][0], "n");
    }

    #[test]
    fn a_malformed_argument_string_becomes_an_object() {
        assert_eq!(object_args(&json!("{\"a\":1}")), json!({"a": 1}));
        assert_eq!(object_args(&json!("{\"a\":")), json!({}));
    }

    fn png(w: u32, h: u32) -> Vec<u8> {
        use ::image::ImageEncoder;
        let img = ::image::RgbaImage::from_fn(w, h, |x, y| ::image::Rgba([(x % 250) as u8, (y % 250) as u8, 60, 255]));
        let mut out = Vec::new();
        ::image::codecs::png::PngEncoder::new(&mut out)
            .write_image(img.as_raw(), w, h, ::image::ExtendedColorType::Rgba8)
            .unwrap();
        out
    }

    fn vision_opts(store: &MemoryBlobStore) -> BodyOptions<'_> {
        BodyOptions { vision: true, blobs: Some(store), ..BodyOptions::new(32_768, None) }
    }

    #[test]
    fn a_user_image_rides_as_base64_on_the_user_message_at_the_providers_size() {
        let store = MemoryBlobStore::new();
        let r = image::ingest(&store, &png(1600, 800), image::OPENROUTER_LONG_EDGE).unwrap();
        assert_eq!(r.w, 1600);
        let req = ChatRequest::new("qwen3.5:9b", vec![Message::with_images(Role::User, "what is this?", [r])]);
        let body = request_body_with(&req, &vision_opts(&store));
        let m = &body["messages"][0];
        assert_eq!(m["content"], "what is this?");
        let b64 = m["images"][0].as_str().unwrap();
        let bytes = image::base64_decode(b64).unwrap();
        let decoded = ::image::load_from_memory(&bytes).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (1024, 512), "fitted to Ollama's long edge");
        assert!(!b64.starts_with("data:"), "Ollama takes bare base64");
    }

    #[test]
    fn a_tool_result_image_follows_its_batch_as_a_user_message() {
        let store = MemoryBlobStore::new();
        let r = image::ingest(&store, &png(64, 48), 1024).unwrap();
        let req = ChatRequest::new(
            "m",
            vec![
                Message::user("look"),
                Message {
                    role: Role::Assistant,
                    content: vec![
                        Part::ToolCall(ToolCall { id: "c1".into(), name: "read".into(), args: json!({"path": "a.png"}) }),
                        Part::ToolCall(ToolCall { id: "c2".into(), name: "read".into(), args: json!({"path": "b.md"}) }),
                    ],
                },
                Message::tool_results([
                    ToolResult { call_id: "c1".into(), content: "a.png 64x48".into(), is_error: false, parts: vec![marlowe_model::ResultPart::Image(r)] },
                    ToolResult { call_id: "c2".into(), content: "text".into(), is_error: false, parts: Vec::new() },
                ]),
            ],
        );
        let body = request_body_with(&req, &vision_opts(&store));
        let msgs = body["messages"].as_array().unwrap();
        let roles: Vec<&str> = msgs.iter().map(|m| m["role"].as_str().unwrap()).collect();
        assert_eq!(roles, ["user", "assistant", "tool", "tool", "user"], "images after every result of the batch");
        assert_eq!(msgs[2]["content"], "a.png 64x48");
        assert_eq!(msgs[4]["images"].as_array().unwrap().len(), 1);
        assert!(msgs[4]["content"].as_str().unwrap().contains("`read`"));
    }

    #[test]
    fn without_vision_or_a_store_or_the_blob_the_image_becomes_a_note() {
        let store = MemoryBlobStore::new();
        let r = image::ingest(&store, &png(64, 48), 1024).unwrap();
        let req = ChatRequest::new("m", vec![Message::with_images(Role::User, "what is this?", [r.clone()])]);
        let text_model = BodyOptions { vision: false, blobs: Some(&store), ..BodyOptions::new(32_768, None) };
        let m = &request_body_with(&req, &text_model)["messages"][0];
        assert!(m.get("images").is_none());
        assert!(m["content"].as_str().unwrap().contains("cannot see images"));
        let no_store = BodyOptions { vision: true, ..BodyOptions::new(32_768, None) };
        assert!(request_body_with(&req, &no_store)["messages"][0]["content"].as_str().unwrap().contains("no image store"));
        let empty = MemoryBlobStore::new();
        let m = &request_body_with(&req, &vision_opts(&empty))["messages"][0];
        assert!(m["content"].as_str().unwrap().contains("image unavailable"));
        assert!(m.get("images").is_none());
        // An image-only message is still a message.
        let only = ChatRequest::new("m", vec![Message::with_images(Role::User, "", [r])]);
        assert_eq!(request_body_with(&only, &vision_opts(&store))["messages"].as_array().unwrap().len(), 1);
    }

    fn filler(chars: usize) -> String {
        "word ".repeat(chars / 5)
    }

    #[test]
    fn num_ctx_always_covers_the_prompt_plus_the_output_asked_for() {
        let mut req = ChatRequest::new("m", vec![Message::user(filler(40_000))]);
        req.max_output_tokens = Some(2_000);
        let opts = BodyOptions::new(32_768, None);
        let p = plan_request(&req, &opts).unwrap();
        assert!(p.prompt_tokens + p.num_predict <= 32_768, "{p:?}");
        assert_eq!(p.num_predict, 2_000, "plenty of room: the ask is kept");
        assert_eq!(p.body["options"]["num_predict"], 2_000);

        // Tight: the output is cut to what is left, never past the window.
        let mut tight = ChatRequest::new("m", vec![Message::user(filler(100_000))]);
        tight.max_output_tokens = Some(16_000);
        let t = plan_request(&tight, &opts).unwrap();
        assert!(t.num_predict < 16_000 && t.num_predict >= MIN_OUTPUT_TOKENS, "{t:?}");
        assert!(t.prompt_tokens + t.num_predict <= 32_768);
        assert_eq!(t.body["options"]["num_predict"], t.num_predict);
    }

    #[test]
    fn a_prompt_that_cannot_fit_fails_before_it_is_sent() {
        let req = ChatRequest::new("m", vec![Message::user(filler(150_000))]);
        let err = plan_request(&req, &BodyOptions::new(32_768, None)).unwrap_err();
        assert!(is_context_overflow(&err), "{err}");
        assert!(!err.retriable);
        assert!(err.detail.contains("32768"));
        assert!(!is_context_overflow(&ProviderError::bad_request("something else")));
    }

    #[test]
    fn tool_schemas_and_images_count_toward_the_prompt() {
        let store = MemoryBlobStore::new();
        let r = image::ingest(&store, &png(1000, 1000), 1024).unwrap();
        let mut with_tools = ChatRequest::new("m", vec![Message::user("hi")]);
        let base = plan_request(&with_tools, &BodyOptions::new(32_768, None)).unwrap().prompt_tokens;
        with_tools.tools = vec![ToolSpec {
            name: "read".into(),
            description: "d".repeat(3_400),
            input_schema: json!({"type": "object", "properties": {}}),
        }];
        let t = plan_request(&with_tools, &BodyOptions::new(32_768, None)).unwrap().prompt_tokens;
        assert!(t >= base + 1_000, "{base} -> {t}");
        let pic = ChatRequest::new("m", vec![Message::with_images(Role::User, "hi", [r])]);
        let i = plan_request(&pic, &vision_opts(&store)).unwrap().prompt_tokens;
        assert!(i >= base + 1_000, "an image costs about {} tokens: {base} -> {i}", image::estimate_tokens(1000, 1000));
    }
}
