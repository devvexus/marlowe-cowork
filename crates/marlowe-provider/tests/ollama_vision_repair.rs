//! Track G against a scripted loopback Ollama: vision, the window check, tool-call repair and the
//! per-turn thinking knob. No live inference; the replies are recorded-style NDJSON.

use std::sync::Arc;

use marlowe_model::image::{self, MemoryBlobStore};
use marlowe_model::{
    CancelToken, ChatRequest, Effort, Message, Part, Provider, ProviderErrorKind, ResultPart, Role, StopReason,
    ToolCall, ToolResult, ToolSpec,
};
use marlowe_provider::ollama::{is_context_overflow, ModelMeta};
use marlowe_provider::OllamaProvider;
use serde_json::{json, Value};

mod common;
use common::{serve, Reply, Seen};

fn png(w: u32, h: u32) -> Vec<u8> {
    use ::image::ImageEncoder;
    let img = ::image::RgbaImage::from_fn(w, h, |x, y| ::image::Rgba([(x % 250) as u8, (y % 250) as u8, 90, 255]));
    let mut out = Vec::new();
    ::image::codecs::png::PngEncoder::new(&mut out)
        .write_image(img.as_raw(), w, h, ::image::ExtendedColorType::Rgba8)
        .unwrap();
    out
}

fn show(caps: &[&str], family: &str) -> String {
    json!({
        "capabilities": caps,
        "details": {"family": family},
        "model_info": {"general.architecture": family, format!("{family}.context_length"): 262144}
    })
    .to_string()
}

fn ok_chat() -> Reply {
    Reply::Chunked(vec![r#"{"message":{"content":"ok"},"done":true,"done_reason":"stop"}"#.into()])
}

fn chat_bodies(seen: &Arc<std::sync::Mutex<Vec<Seen>>>) -> Vec<Value> {
    seen.lock().unwrap().iter().filter(|s| s.path == "/api/chat").map(|s| s.body.clone()).collect()
}

fn read_spec() -> ToolSpec {
    ToolSpec {
        name: "read".into(),
        description: "Read a file".into(),
        input_schema: json!({"type":"object","properties":{
            "path":{"type":"string"},"offset":{"type":"integer"},"limit":{"type":"integer"}},"required":["path"]}),
    }
}

/// T2 (provider half): a stored image reaches a vision model as base64 in `images`, fitted to
/// Ollama's long edge, and a tool-result image follows its batch as a user message.
#[test]
fn parity_t2_png_in_the_ollama_request_json() {
    let store = Arc::new(MemoryBlobStore::new());
    let big = image::ingest(&*store, &png(2000, 1000), image::OPENROUTER_LONG_EDGE).unwrap();
    let small = image::ingest(&*store, &png(64, 48), image::OLLAMA_LONG_EDGE).unwrap();
    assert_eq!(big.w, 2000);

    let (endpoint, seen) = serve(Box::new(|path| match path {
        "/api/show" => Reply::Json(200, show(&["completion", "vision", "tools"], "qwen35")),
        _ => ok_chat(),
    }));
    let p = OllamaProvider::new(endpoint).with_blobs(store.clone());
    let req = ChatRequest::new(
        "qwen3.5:9b",
        vec![
            Message::with_images(Role::User, "what is this?", [big]),
            Message {
                role: Role::Assistant,
                content: vec![Part::ToolCall(ToolCall { id: "c1".into(), name: "read".into(), args: json!({"path": "a.png"}) })],
            },
            Message::tool_results([ToolResult {
                call_id: "c1".into(),
                content: "a.png 64x48".into(),
                is_error: false,
                parts: vec![ResultPart::Image(small)],
            }]),
        ],
    );
    p.stream(&req, &CancelToken::new(), &mut |_| {}).unwrap();

    let body = chat_bodies(&seen).remove(0);
    let msgs = body["messages"].as_array().unwrap();
    let first = &msgs[0];
    assert_eq!(first["content"], "what is this?");
    let sent = image::base64_decode(first["images"][0].as_str().unwrap()).unwrap();
    let decoded = ::image::load_from_memory(&sent).unwrap();
    assert_eq!((decoded.width(), decoded.height()), (1024, 512), "fitted to the 1024 long edge");
    assert_eq!(msgs[2]["role"], "tool");
    assert_eq!(msgs[3]["role"], "user");
    assert_eq!(msgs[3]["images"].as_array().unwrap().len(), 1);
    assert!(msgs[3]["content"].as_str().unwrap().contains("`read`"), "the image is labelled with its call");
}

#[test]
fn a_model_without_vision_never_receives_image_bytes() {
    let store = Arc::new(MemoryBlobStore::new());
    let r = image::ingest(&*store, &png(100, 100), 1024).unwrap();
    let (endpoint, seen) = serve(Box::new(|path| match path {
        "/api/show" => Reply::Json(200, show(&["completion", "tools"], "llama")),
        _ => ok_chat(),
    }));
    let p = OllamaProvider::new(endpoint).with_blobs(store);
    let req = ChatRequest::new("llama3:8b", vec![Message::with_images(Role::User, "look", [r])]);
    p.stream(&req, &CancelToken::new(), &mut |_| {}).unwrap();
    let body = chat_bodies(&seen).remove(0);
    assert!(body["messages"][0].get("images").is_none());
    assert!(body["messages"][0]["content"].as_str().unwrap().contains("cannot see images"));
}

#[test]
fn the_catalog_says_which_models_see() {
    let (endpoint, _) = serve(Box::new(|path| match path {
        "/api/tags" => Reply::Json(
            200,
            json!({"models":[
                {"name":"qwen3-vl:8b","details":{"family":"qwen3vl","context_length":262144},
                 "capabilities":["completion","vision","tools"]},
                {"name":"qwen3.5:9b","details":{"family":"qwen35","context_length":262144},
                 "capabilities":["completion","tools","thinking"]}
            ]})
            .to_string(),
        ),
        _ => Reply::Json(404, "{}".into()),
    }));
    let models = OllamaProvider::new(endpoint).list_models().unwrap();
    let by = |id: &str| models.iter().find(|m| m.id == id).unwrap();
    assert!(by("qwen3-vl:8b").supports_vision);
    assert!(!by("qwen3.5:9b").supports_vision);
}

/// A15: a prompt that cannot fit `num_ctx` plus a useful reply is refused before any request.
#[test]
fn a_prompt_over_the_window_fails_before_it_is_sent() {
    let (endpoint, seen) = serve(Box::new(|path| match path {
        "/api/show" => Reply::Json(200, show(&["completion"], "llama")),
        _ => ok_chat(),
    }));
    let p = OllamaProvider::new(endpoint).with_context_tokens(8_192);
    let req = ChatRequest::new("llama3:8b", vec![Message::user("word ".repeat(30_000))]);
    let err = p.stream(&req, &CancelToken::new(), &mut |_| {}).unwrap_err();
    assert!(is_context_overflow(&err), "{err}");
    assert_eq!(err.kind, ProviderErrorKind::BadRequest);
    assert!(chat_bodies(&seen).is_empty(), "nothing was sent");

    // Every request that is sent keeps prompt + output inside the window it declares.
    let ok = ChatRequest::new("llama3:8b", vec![Message::user("word ".repeat(3_000))]);
    p.stream(&ok, &CancelToken::new(), &mut |_| {}).unwrap();
    let body = chat_bodies(&seen).remove(0);
    let (ctx, predict) = (body["options"]["num_ctx"].as_u64().unwrap(), body["options"]["num_predict"].as_u64().unwrap());
    assert_eq!(ctx, 8_192);
    assert!(predict >= 1_024 && predict < ctx, "num_predict {predict} is cut to what the prompt leaves");
}

/// A4: arguments a small model mangles are repaired against the schema, calls it wrote as text are
/// recovered, and each is counted.
#[test]
fn tool_calls_are_repaired_recovered_and_counted() {
    let (endpoint, _) = serve(Box::new(|path| match path {
        "/api/show" => Reply::Json(200, show(&["completion", "tools"], "qwen35")),
        _ => Reply::Chunked(vec![
            // Arguments as a Python-ish string with a trailing comma and a quoted integer, and a
            // capitalised tool name.
            json!({"message":{"tool_calls":[{"function":{"name":"Read","arguments":"{'path': 'a.rs', 'limit': '20',}"}}]}}).to_string(),
            // A well-formed call.
            json!({"message":{"tool_calls":[{"function":{"name":"read","arguments":{"path":"b.rs"}}}]}}).to_string(),
            r#"{"message":{"content":""},"done":true,"done_reason":"stop"}"#.into(),
        ]),
    }));
    let p = OllamaProvider::new(endpoint);
    let mut req = ChatRequest::new("qwen3.5:9b", vec![Message::user("read both")]);
    req.tools = vec![read_spec()];
    let c = p.stream(&req, &CancelToken::new(), &mut |_| {}).unwrap();
    assert_eq!(c.tool_calls.len(), 2);
    assert_eq!(c.tool_calls[0].name, "read");
    assert_eq!(c.tool_calls[0].args, json!({"path": "a.rs", "limit": 20}));
    assert_eq!(c.tool_calls[1].args, json!({"path": "b.rs"}));
    assert_eq!(c.stop, StopReason::ToolUse);
    let s = p.repair_stats().snapshot();
    assert_eq!((s.calls, s.clean, s.repaired), (2, 1, 1), "{s:?}");
    assert!((s.repair_rate() - 0.5).abs() < 1e-9 && (s.valid_rate() - 1.0).abs() < 1e-9);
    assert!(s.fixes.get("tool_name").is_some() && s.fixes.get("single_quotes").is_some(), "{:?}", s.fixes);
}

/// A4 honesty: a call to a tool the request never offered, and a call missing a required
/// argument, are calls that are not valid after repair.
#[test]
fn unknown_tools_and_missing_required_arguments_are_not_counted_valid() {
    let (endpoint, _) = serve(Box::new(|path| match path {
        "/api/show" => Reply::Json(200, show(&["completion", "tools"], "qwen35")),
        _ => Reply::Chunked(vec![
            json!({"message":{"tool_calls":[{"function":{"name":"teleport","arguments":{"to":"moon"}}}]}}).to_string(),
            json!({"message":{"tool_calls":[{"function":{"name":"read","arguments":{}}}]}}).to_string(),
            json!({"message":{"tool_calls":[{"function":{"name":"read","arguments":{"path":"a.rs"}}}]}}).to_string(),
            r#"{"message":{"content":""},"done":true,"done_reason":"stop"}"#.into(),
        ]),
    }));
    let p = OllamaProvider::new(endpoint);
    let mut req = ChatRequest::new("qwen3.5:9b", vec![Message::user("go")]);
    req.tools = vec![read_spec()];
    let c = p.stream(&req, &CancelToken::new(), &mut |_| {}).unwrap();
    assert_eq!(c.tool_calls.len(), 3, "all three still reach the loop, which reports the bad ones");
    let s = p.repair_stats().snapshot();
    assert_eq!((s.calls, s.unknown_tool, s.invalid, s.clean), (3, 1, 1, 1), "{s:?}");
    assert!((s.valid_rate() - 1.0 / 3.0).abs() < 1e-9, "{}", s.valid_rate());
}

#[test]
fn a_call_written_as_text_is_recovered_only_for_a_tool_the_request_offered() {
    let content = "Let me look.\n<function=read><parameter=path>notes.md</parameter><parameter=limit>40</parameter></function>";
    let (endpoint, _) = serve(Box::new(move |path| match path {
        "/api/show" => Reply::Json(200, show(&["completion", "tools"], "qwen35")),
        _ => Reply::Chunked(vec![
            json!({"message":{"content":content}}).to_string(),
            r#"{"message":{"content":""},"done":true,"done_reason":"stop"}"#.into(),
        ]),
    }));
    let p = OllamaProvider::new(endpoint);
    let mut req = ChatRequest::new("qwen3.5:9b", vec![Message::user("read notes")]);
    req.tools = vec![read_spec()];
    let mut shown = String::new();
    let c = p
        .stream(&req, &CancelToken::new(), &mut |e| {
            if let marlowe_model::StreamEvent::TextDelta(t) = e {
                shown.push_str(&t);
            }
        })
        .unwrap();
    assert_eq!(c.tool_calls.len(), 1);
    assert_eq!(c.tool_calls[0].args, json!({"path": "notes.md", "limit": 40}), "typed by the schema");
    assert_eq!(shown.trim(), "Let me look.", "the markup never reached the screen");
    assert_eq!(p.repair_stats().snapshot().from_text, 1);
}

/// C14: the effort of the turn is in the request: a bool for most models, a level for gpt-oss.
#[test]
fn parity_c14_effort_is_in_the_request() {
    let think_for = |caps: &[&str], family: &str, effort: Effort| -> Option<Value> {
        let caps: Vec<String> = caps.iter().map(|s| s.to_string()).collect();
        let caps2 = caps.clone();
        let family2 = family.to_string();
        let (endpoint, seen) = serve(Box::new(move |path| match path {
            "/api/show" => Reply::Json(
                200,
                json!({"capabilities": caps2, "details": {"family": family2},
                       "model_info": {"general.architecture": family2, format!("{family2}.context_length"): 131072}})
                .to_string(),
            ),
            _ => ok_chat(),
        }));
        let p = OllamaProvider::new(endpoint);
        let mut req = ChatRequest::new("m", vec![Message::user("hi")]);
        req.effort = effort;
        p.stream(&req, &CancelToken::new(), &mut |_| {}).unwrap();
        chat_bodies(&seen).remove(0).get("think").cloned()
    };
    let qwen = ["completion", "tools", "thinking"];
    assert_eq!(think_for(&qwen, "qwen35", Effort::High), Some(json!(true)));
    assert_eq!(think_for(&qwen, "qwen35", Effort::Off), Some(json!(false)));
    assert_eq!(think_for(&qwen, "gptoss", Effort::Low), Some(json!("low")));
    assert_eq!(think_for(&qwen, "gptoss", Effort::Max), Some(json!("high")));
    assert_eq!(think_for(&["completion"], "llama", Effort::High), None, "no thinking capability, no field");
}

/// C10 / A14: across a tool loop each request starts with the previous request's whole prompt (so
/// Ollama's prefix cache hits), except where earlier reasoning is dropped in one chunk.
#[test]
fn parity_c10_the_prompt_prefix_is_stable_across_a_tool_loop() {
    let (endpoint, _) = serve(Box::new(|_| ok_chat()));
    let p = OllamaProvider::new(endpoint);
    let meta = ModelMeta { capabilities: Some(vec!["completion".into(), "tools".into(), "thinking".into()]), context: Some(131_072), family: None };
    let mut req = ChatRequest::new("m", vec![Message::user("fix the build")]);
    req.system = vec![marlowe_model::SystemBlock { text: "core".into(), cache: true }];
    req.tools = vec![read_spec()];
    let mut prev: Option<Vec<Value>> = None;
    let mut breaks = 0;
    for step in 0..12 {
        let msgs = p.build_body(&req, &meta)["messages"].as_array().unwrap().clone();
        if let Some(prev) = &prev {
            if msgs[..prev.len()] != prev[..] {
                breaks += 1;
            }
        }
        prev = Some(msgs);
        req.messages.push(Message {
            role: Role::Assistant,
            content: vec![
                Part::Thinking(marlowe_model::Thinking { text: format!("thought {step}"), ..Default::default() }),
                Part::ToolCall(ToolCall { id: format!("c{step}"), name: "read".into(), args: json!({"path": format!("f{step}.rs")}) }),
            ],
        });
        req.messages.push(Message::tool_results([ToolResult {
            call_id: format!("c{step}"),
            content: format!("body {step}"),
            is_error: false,
            parts: Vec::new(),
        }]));
    }
    assert_eq!(breaks, 0, "every prompt starts with the previous one: the prefix never breaks");
    // The tool schemas are byte-identical from call to call.
    let a = p.build_body(&req, &meta)["tools"].to_string();
    req.messages.push(Message::user("next"));
    assert_eq!(a, p.build_body(&req, &meta)["tools"].to_string());
}
