//! Track G on OpenRouter, through `ScriptedTransport` and recorded-style SSE fixtures
//! (`tests/fixtures`): images as `image_url` parts, vision read from the catalog, cache
//! breakpoints, and tool-call repair and recovery. No network, no key.

use std::sync::Arc;

use marlowe_model::image::{self, MemoryBlobStore};
use marlowe_model::{
    CancelToken, ChatRequest, Message, Part, Provider, ResultPart, Role, StopReason, StreamEvent, SystemBlock,
    ToolCall, ToolResult, ToolSpec,
};
use marlowe_openrouter::{ApiKey, OpenRouterProvider, Reply, ScriptedTransport, Seen};
use serde_json::{json, Value};

const VISION: &str = "qwen/qwen3-vl-235b-a22b-instruct";
const TEXT_ONLY: &str = "qwen/qwen3-coder";

type Log = Arc<std::sync::Mutex<Vec<Seen>>>;

fn provider(replies: Vec<Reply>) -> (OpenRouterProvider, Log) {
    let t = ScriptedTransport::new(replies);
    let seen = t.seen();
    (OpenRouterProvider::with_transport(Arc::new(t), Some(ApiKey::from_secret("sk-or-v1-feedfacefeedfacefeedfacefeedface"))), seen)
}

fn png(w: u32, h: u32) -> Vec<u8> {
    use ::image::ImageEncoder;
    let img = ::image::RgbaImage::from_fn(w, h, |x, y| ::image::Rgba([(x % 250) as u8, (y % 250) as u8, 70, 255]));
    let mut out = Vec::new();
    ::image::codecs::png::PngEncoder::new(&mut out)
        .write_image(img.as_raw(), w, h, ::image::ExtendedColorType::Rgba8)
        .unwrap();
    out
}

fn read_spec() -> ToolSpec {
    ToolSpec {
        name: "read".into(),
        description: "Read a file".into(),
        input_schema: json!({"type":"object","properties":{
            "path":{"type":"string"},"offset":{"type":"integer"},"limit":{"type":"integer"},"all":{"type":"boolean"}},"required":["path"]}),
    }
}

fn plain_ok() -> Reply {
    Reply::ok(include_str!("fixtures/sse_text_ok.sse"))
}

fn sent(seen: &Log, n: usize) -> Value {
    serde_json::from_str(&seen.lock().unwrap()[n].body).unwrap()
}

/// T2 (provider half): a stored image reaches a vision model as an `image_url` data URI fitted to
/// OpenRouter's long edge; a tool-result image follows its batch as a user message.
#[test]
fn parity_t2_png_in_the_openrouter_request_json() {
    let store = Arc::new(MemoryBlobStore::new());
    let huge = image::ingest(&*store, &png(3000, 1000), image::STORE_LONG_EDGE).unwrap();
    assert_eq!(huge.w, 2048, "ingest keeps the largest any provider takes");
    let shot = image::ingest(&*store, &png(64, 48), 1024).unwrap();

    let (p, seen) = provider(vec![Reply::ok(include_str!("fixtures/catalog_vision.json")), plain_ok()]);
    let p = p.with_blobs(store.clone());
    let models = p.list_models().unwrap();
    assert!(models.iter().find(|m| m.id == VISION).unwrap().supports_vision);
    assert!(!models.iter().find(|m| m.id == TEXT_ONLY).unwrap().supports_vision);

    let req = ChatRequest::new(
        VISION,
        vec![
            Message::with_images(Role::User, "what is this?", [huge]),
            Message {
                role: Role::Assistant,
                content: vec![Part::ToolCall(ToolCall { id: "c1".into(), name: "read".into(), args: json!({"path": "a.png"}) })],
            },
            Message::tool_results([ToolResult {
                call_id: "c1".into(),
                content: "a.png 64x48".into(),
                is_error: false,
                parts: vec![ResultPart::Image(shot)],
            }]),
        ],
    );
    p.stream(&req, &CancelToken::new(), &mut |_| {}).unwrap();

    let body = sent(&seen, 1);
    let msgs = body["messages"].as_array().unwrap();
    let first = &msgs[0]["content"];
    assert_eq!(first[0], json!({"type": "text", "text": "what is this?"}));
    assert_eq!(first[1]["type"], "image_url");
    let url = first[1]["image_url"]["url"].as_str().unwrap();
    let b64 = url.strip_prefix("data:image/png;base64,").expect("a png data URI");
    let decoded = ::image::load_from_memory(&image::base64_decode(b64).unwrap()).unwrap();
    assert_eq!((decoded.width(), decoded.height()), (2048, 683), "fitted to the 2048 long edge");

    let roles: Vec<&str> = msgs.iter().map(|m| m["role"].as_str().unwrap()).collect();
    assert_eq!(roles, ["user", "assistant", "tool", "user"]);
    assert_eq!(msgs[2]["content"], "a.png 64x48", "a tool message stays text");
    assert_eq!(msgs[3]["content"][1]["type"], "image_url");
    assert!(msgs[3]["content"][0]["text"].as_str().unwrap().contains("c1"));
}

#[test]
fn a_text_only_model_gets_a_note_and_never_the_bytes() {
    let store = Arc::new(MemoryBlobStore::new());
    let r = image::ingest(&*store, &png(100, 100), 1024).unwrap();
    let (p, seen) = provider(vec![Reply::ok(include_str!("fixtures/catalog_vision.json")), plain_ok()]);
    let p = p.with_blobs(store);
    p.list_models().unwrap();
    let req = ChatRequest::new(TEXT_ONLY, vec![Message::with_images(Role::User, "look", [r])]);
    p.stream(&req, &CancelToken::new(), &mut |_| {}).unwrap();
    let wire = seen.lock().unwrap()[1].body.clone();
    assert!(!wire.contains("image_url") && !wire.contains("base64"));
    let body = sent(&seen, 1);
    assert!(body["messages"][0]["content"].as_str().unwrap().contains("cannot see images"));
}

#[test]
fn an_unlisted_model_is_tried_with_the_image_and_no_store_is_a_note() {
    let store = Arc::new(MemoryBlobStore::new());
    let r = image::ingest(&*store, &png(100, 100), 1024).unwrap();
    let req = ChatRequest::new("vendor/legacy", vec![Message::with_images(Role::User, "look", [r.clone()])]);

    // The catalog never said: the image is sent and OpenRouter decides.
    let (p, seen) = provider(vec![plain_ok()]);
    let p = p.with_blobs(store);
    p.stream(&req, &CancelToken::new(), &mut |_| {}).unwrap();
    assert_eq!(sent(&seen, 0)["messages"][0]["content"][1]["type"], "image_url");

    // Without a blob store there are no bytes to send.
    let (p, seen) = provider(vec![plain_ok()]);
    p.stream(&req, &CancelToken::new(), &mut |_| {}).unwrap();
    assert!(sent(&seen, 0)["messages"][0]["content"].as_str().unwrap().contains("no image store"));
}

/// C10: Anthropic models behind OpenRouter get breakpoints on the cached system blocks and on the
/// last user message; other models get none.
#[test]
fn parity_c10_openrouter_cache_breakpoints() {
    let mut req = ChatRequest::new("anthropic/claude-sonnet-4.5", vec![Message::user("fix the build")]);
    req.system = vec![SystemBlock { text: "core".into(), cache: true }, SystemBlock { text: "env".into(), cache: false }];
    let (p, seen) = provider(vec![plain_ok(), plain_ok()]);
    p.stream(&req, &CancelToken::new(), &mut |_| {}).unwrap();
    let body = sent(&seen, 0);
    assert_eq!(body["messages"][0]["content"][0]["cache_control"]["type"], "ephemeral");
    assert!(body["messages"][0]["content"][1].get("cache_control").is_none());
    assert_eq!(body["messages"][1]["content"][0]["text"], "fix the build");
    assert_eq!(body["messages"][1]["content"][0]["cache_control"]["type"], "ephemeral");

    // Later steps of the loop keep the same breakpoint on the same message, so the prefix is read.
    req.messages.push(Message {
        role: Role::Assistant,
        content: vec![Part::ToolCall(ToolCall { id: "c1".into(), name: "read".into(), args: json!({"path": "a"}) })],
    });
    req.messages.push(Message::tool_results([ToolResult { call_id: "c1".into(), content: "x".into(), is_error: false, parts: vec![] }]));
    p.stream(&req, &CancelToken::new(), &mut |_| {}).unwrap();
    let again = sent(&seen, 1);
    assert_eq!(again["messages"].as_array().unwrap()[..2], body["messages"].as_array().unwrap()[..2], "the prefix is byte-identical from step to step");

    req.model = "openai/gpt-5".into();
    let (p, seen) = provider(vec![plain_ok()]);
    p.stream(&req, &CancelToken::new(), &mut |_| {}).unwrap();
    assert!(!seen.lock().unwrap()[0].body.contains("cache_control"));
}

#[test]
fn reasoning_families_that_reject_a_temperature_are_not_sent_one() {
    for (model, sends) in [
        ("openai/gpt-5", true),
        ("anthropic/claude-sonnet-4.5", true),
        ("qwen/qwen3-coder", true),
        ("openai/o3-pro", false),
        ("openai/o4-mini", false),
        ("openai/o1", false),
        ("x-ai/grok-3-mini", false),
        ("qwen/qwq-32b", false),
        ("qwen/qwen3-235b-a22b-thinking-2507", false),
        ("openai/omni-thing", true),
    ] {
        let mut req = ChatRequest::new(model, vec![Message::user("hi")]);
        req.temperature = Some(0.2);
        let body = marlowe_openrouter::request::request_body(&req, None);
        assert_eq!(body.get("temperature").is_some(), sends, "{model}");
    }
}

fn run(model: &str, fixture: &'static str) -> (marlowe_model::Completion, String, OpenRouterProvider) {
    let (p, _) = provider(vec![Reply::ok(fixture)]);
    let mut req = ChatRequest::new(model, vec![Message::user("read the file")]);
    req.tools = vec![read_spec()];
    let mut shown = String::new();
    let c = p
        .stream(&req, &CancelToken::new(), &mut |e| {
            if let StreamEvent::TextDelta(t) = e {
                shown.push_str(&t);
            }
        })
        .unwrap();
    (c, shown, p)
}

/// A4: structured calls with Python-dict arguments in fragments are repaired against the schema.
#[test]
fn structured_arguments_are_repaired_and_counted() {
    let (c, _, p) = run(TEXT_ONLY, include_str!("fixtures/sse_python_dict_arguments.sse"));
    assert_eq!(c.tool_calls.len(), 2);
    assert_eq!(c.tool_calls[0].name, "read", "the tool's own spelling");
    assert_eq!(c.tool_calls[0].id, "call_1");
    assert_eq!(c.tool_calls[0].args, json!({"path": "src/lib.rs", "limit": 20, "all": false}));
    assert_eq!(c.tool_calls[1].args, json!({"path": "b.rs"}));
    assert_eq!(c.stop, StopReason::ToolUse);
    let s = p.repair_stats().snapshot();
    assert_eq!((s.calls, s.clean, s.repaired), (2, 1, 1), "{s:?}");
    assert!(s.fixes.contains_key("single_quotes") && s.fixes.contains_key("tool_name"), "{:?}", s.fixes);
}

#[test]
fn a_call_cut_off_by_the_output_limit_is_never_completed() {
    let (p, _) = provider(vec![Reply::ok(include_str!("fixtures/sse_cut_off_write.sse"))]);
    let mut req = ChatRequest::new(TEXT_ONLY, vec![Message::user("write main")]);
    req.tools = vec![ToolSpec {
        name: "write".into(),
        description: "Write".into(),
        input_schema: json!({"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"]}),
    }];
    let c = p.stream(&req, &CancelToken::new(), &mut |_| {}).unwrap();
    assert_eq!(c.stop, StopReason::MaxTokens);
    assert!(c.tool_calls[0].args.is_string(), "kept raw for the loop to report: {:?}", c.tool_calls[0].args);
    let s = p.repair_stats().snapshot();
    assert_eq!((s.unusable, s.truncated), (1, 1));
}

/// A4: Qwen's `<function=...>` form and a Hermes block behind OpenRouter become structured calls,
/// and the markup never reaches the screen.
#[test]
fn calls_written_as_text_are_recovered_and_kept_off_the_screen() {
    let (c, shown, p) = run(TEXT_ONLY, include_str!("fixtures/sse_qwen_function_leak.sse"));
    assert_eq!(c.tool_calls.len(), 1);
    assert_eq!(c.tool_calls[0].name, "read");
    assert_eq!(c.tool_calls[0].args, json!({"path": "notes.md", "limit": 40}));
    assert_eq!(shown.trim(), "Let me look at the file.");
    assert_eq!(c.text.trim(), "Let me look at the file.");
    assert_eq!(c.stop, StopReason::ToolUse);
    assert_eq!(p.repair_stats().snapshot().from_text, 1);

    let (c, shown, _) = run(TEXT_ONLY, include_str!("fixtures/sse_hermes_leak.sse"));
    assert_eq!(c.tool_calls[0].name, "read");
    assert_eq!(c.tool_calls[0].args, json!({"path": "a.rs", "limit": 20}));
    assert_eq!(shown.trim(), "Checking.");
}

#[test]
fn a_reply_that_only_quotes_markup_in_a_fence_is_left_alone() {
    let wire = format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"delta":{"content":"Format:\n```\n<function=read><parameter=path>a</parameter></function>\n```\n"}}]}),
        json!({"choices":[{"delta":{},"finish_reason":"stop"}]})
    );
    let (p, _) = provider(vec![Reply::ok(&wire)]);
    let mut req = ChatRequest::new(TEXT_ONLY, vec![Message::user("how do I call it?")]);
    req.tools = vec![read_spec()];
    let mut shown = String::new();
    let c = p
        .stream(&req, &CancelToken::new(), &mut |e| {
            if let StreamEvent::TextDelta(t) = e {
                shown.push_str(&t);
            }
        })
        .unwrap();
    assert!(c.tool_calls.is_empty());
    assert!(shown.contains("<function=read>"));
    assert_eq!(p.repair_stats().snapshot().calls, 0);
}

/// C14: the effort of the turn is in the request, spelled the way the catalog says the model
/// takes it, and `Off` switches reasoning off where the model allows it.
#[test]
fn parity_c14_openrouter_effort_is_in_the_request() {
    use marlowe_model::Effort;
    let catalog = json!({"data":[{
        "id": "openai/gpt-5", "architecture": {"output_modalities": ["text"]},
        "supported_parameters": ["reasoning", "tools"],
        "reasoning": {"mandatory": false, "supported_efforts": ["high", "medium", "low", "none"]}
    }]});
    let (p, seen) = provider(vec![Reply::ok(&catalog.to_string()), plain_ok(), plain_ok(), plain_ok()]);
    p.list_models().unwrap();
    for (effort, want) in [
        (Effort::High, json!({"effort": "high"})),
        (Effort::Low, json!({"effort": "low"})),
        (Effort::Off, json!({"effort": "none"})),
    ] {
        let mut req = ChatRequest::new("openai/gpt-5", vec![Message::user("hi")]);
        req.effort = effort;
        p.stream(&req, &CancelToken::new(), &mut |_| {}).unwrap();
        let n = seen.lock().unwrap().len() - 1;
        assert_eq!(sent(&seen, n)["reasoning"], want, "{effort:?}");
    }
}
