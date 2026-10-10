//! `OpenRouterProvider` driven through `ScriptedTransport`: the real request, header, SSE,
//! retry and error paths, with only the socket replaced.

use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use marlowe_model::{
    CancelToken, ChatRequest, Effort, Message, Part, Provider, ProviderErrorKind, Role,
    StopReason, StreamEvent, Thinking, ToolCall, ToolResult,
};
use marlowe_openrouter::{retry, ApiKey, OpenRouterProvider, Reply, ScriptedTransport, REDACTED};
use serde_json::{json, Value};

const SECRET: &str = "sk-or-v1-0123456789abcdef0123456789abcdef";
const MODEL: &str = "anthropic/claude-sonnet-4.5";

fn sse(chunks: &[Value]) -> String {
    let mut out = String::new();
    for c in chunks {
        out.push_str(&format!("data: {c}\n\n"));
    }
    out.push_str("data: [DONE]\n\n");
    out
}

fn provider(t: ScriptedTransport) -> (OpenRouterProvider, Arc<std::sync::Mutex<Vec<marlowe_openrouter::Seen>>>) {
    let seen = t.seen();
    (OpenRouterProvider::with_transport(Arc::new(t), Some(ApiKey::from_secret(SECRET))), seen)
}

fn request() -> ChatRequest {
    ChatRequest::new(MODEL, vec![Message::user("read notes.md")])
}

fn no_wait(status: u16) -> Reply {
    Reply { status, retry_after: Some("0".into()), body: "{\"error\":{\"message\":\"busy\"}}".into(), stall: None }
}

#[test]
fn text_arrives_in_pieces_with_usage_and_cost() {
    let wire = format!(
        ": OPENROUTER PROCESSING\n\n{}",
        sse(&[
            json!({"choices":[{"delta":{"content":"Notes "}}]}),
            json!({"choices":[{"delta":{"content":"say "}}]}),
            json!({"choices":[{"delta":{"content":"hello."},"finish_reason":"stop"}]}),
            json!({"choices":[],"usage":{"prompt_tokens":12,"completion_tokens":3,"cost":0.00042,
                   "prompt_tokens_details":{"cached_tokens":8}}}),
        ])
    );
    let (p, _) = provider(ScriptedTransport::new(vec![Reply::ok(&wire)]));
    let mut deltas = Vec::new();
    let c = p
        .stream(&request(), &CancelToken::new(), &mut |e| {
            if let StreamEvent::TextDelta(t) = e {
                deltas.push(t);
            }
        })
        .unwrap();
    assert_eq!(deltas, ["Notes ", "say ", "hello."]);
    assert_eq!(c.text, "Notes say hello.");
    assert_eq!(c.usage.input, 12);
    assert_eq!(c.usage.cached_input, 8);
    assert_eq!(c.usage.cost_usd, Some(0.00042));
    assert_eq!(c.stop, StopReason::EndTurn);
}

#[test]
fn a_tool_turn_round_trips_reasoning_details() {
    let wire = sse(&[
        json!({"choices":[{"delta":{"reasoning":"plan","reasoning_details":[{"type":"reasoning.encrypted","data":"xy","index":0}]}}]}),
        json!({"choices":[{"delta":{"reasoning_details":[{"type":"reasoning.encrypted","data":"z","index":0}]}}]}),
        json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"toolu_1","type":"function","function":{"name":"read","arguments":"{\"path\":"}}]}}]}),
        json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"notes.md\"}"}}]}}]}),
        json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
    ]);
    let (p, seen) = provider(ScriptedTransport::new(vec![Reply::ok(&wire), Reply::ok(&sse(&[
        json!({"choices":[{"delta":{"content":"It says hi."},"finish_reason":"stop"}]}),
    ]))]));
    let first = p.stream(&request(), &CancelToken::new(), &mut |_| {}).unwrap();
    assert_eq!(first.stop, StopReason::ToolUse);
    assert_eq!(first.tool_calls[0].args, json!({"path": "notes.md"}));

    // The loop appends the assistant message and the result, then calls again.
    let mut req = request();
    req.messages.push(first.to_message());
    req.messages.push(Message::tool_results([ToolResult {
        call_id: "toolu_1".into(),
        content: "hi".into(),
        is_error: false,
        parts: Vec::new(),
    }]));
    let second = p.stream(&req, &CancelToken::new(), &mut |_| {}).unwrap();
    assert_eq!(second.text, "It says hi.");

    let sent: Value = serde_json::from_str(&seen.lock().unwrap()[1].body).unwrap();
    let assistant = &sent["messages"][1];
    assert_eq!(assistant["reasoning_details"], json!([{"type":"reasoning.encrypted","data":"xyz","index":0}]));
    assert_eq!(assistant["tool_calls"][0]["id"], "toolu_1");
    assert_eq!(sent["messages"][2]["role"], "tool");
}

#[test]
fn the_key_is_in_exactly_one_header_and_nowhere_in_the_body() {
    let wire = sse(&[json!({"choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}]})]);
    let (p, seen) = provider(ScriptedTransport::new(vec![Reply::ok(&wire)]));
    let mut req = request();
    req.messages.push(Message {
        role: Role::Assistant,
        content: vec![
            Part::Thinking(Thinking { text: "t".into(), ..Default::default() }),
            Part::ToolCall(ToolCall { id: "a".into(), name: "read".into(), args: json!({}) }),
        ],
    });
    p.stream(&req, &CancelToken::new(), &mut |_| {}).unwrap();
    let seen = seen.lock().unwrap();
    let call = &seen[0];
    assert_eq!(call.path, "/chat/completions");
    let holders: Vec<&str> = call.headers.iter().filter(|(_, v)| v.contains(SECRET)).map(|(k, _)| k.as_str()).collect();
    assert_eq!(holders, ["Authorization"]);
    assert!(!call.body.contains(SECRET));
    assert!(!format!("{p:?}", p = ApiKey::from_secret(SECRET)).contains(SECRET));
}

#[test]
fn a_key_an_upstream_echoes_back_is_redacted_and_auth_is_not_retried() {
    let body = format!("{{\"error\":{{\"message\":\"invalid key {SECRET}\"}}}}");
    let (p, seen) = provider(ScriptedTransport::new(vec![Reply::status(401, &body)]));
    let e = p.stream(&request(), &CancelToken::new(), &mut |_| {}).unwrap_err();
    assert_eq!(e.kind, ProviderErrorKind::Auth);
    assert!(!e.detail.contains(SECRET), "{}", e.detail);
    assert!(e.detail.contains(REDACTED), "{}", e.detail);
    assert_eq!(seen.lock().unwrap().len(), 1, "a credential failure is not retried");
}

#[test]
fn the_rate_limited_path_redacts_too_and_retries_are_bounded() {
    let echo = Reply {
        status: 429,
        retry_after: Some("0".into()),
        body: format!("{{\"error\":{{\"message\":\"slow down {SECRET}\"}}}}"),
        stall: None,
    };
    let (p, seen) = provider(ScriptedTransport::new(vec![]).then_forever(echo));
    let e = p.stream(&request(), &CancelToken::new(), &mut |_| {}).unwrap_err();
    assert_eq!(e.kind, ProviderErrorKind::RateLimit);
    assert!(!e.detail.contains(SECRET));
    assert_eq!(seen.lock().unwrap().len() as u32, retry::MAX_ATTEMPTS);
}

#[test]
fn a_transient_status_is_retried_then_succeeds() {
    let ok = sse(&[json!({"choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}]})]);
    let (p, seen) = provider(ScriptedTransport::new(vec![no_wait(503), no_wait(429), Reply::ok(&ok)]));
    let c = p.stream(&request(), &CancelToken::new(), &mut |_| {}).unwrap();
    assert_eq!(c.text, "ok");
    assert_eq!(seen.lock().unwrap().len(), 3);
}

#[test]
fn a_missing_model_names_the_catalog() {
    let (p, _) = provider(ScriptedTransport::new(vec![Reply::status(404, "{\"error\":{\"message\":\"No endpoints\"}}")]));
    let e = p.stream(&request(), &CancelToken::new(), &mut |_| {}).unwrap_err();
    assert_eq!(e.kind, ProviderErrorKind::BadRequest);
    assert!(e.detail.contains(MODEL) && e.detail.contains("openrouter.ai/models"), "{}", e.detail);
}

#[test]
fn cancel_returns_promptly_while_the_upstream_is_silent() {
    let partial = format!("data: {}\n\n", json!({"choices":[{"delta":{"reasoning":"thinking"}}]}));
    let (p, _) = provider(ScriptedTransport::new(vec![Reply::stalled(&partial, Duration::from_secs(20))]));
    let cancel = CancelToken::new();
    let c2 = cancel.clone();
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(300));
        c2.cancel();
    });
    let started = Instant::now();
    let mut saw = false;
    let e = p
        .stream(&request(), &cancel, &mut |e| saw |= matches!(e, StreamEvent::ReasoningDelta { .. }))
        .unwrap_err();
    assert!(e.is_cancelled(), "{e}");
    assert!(saw, "frames before the stall were delivered");
    assert!(started.elapsed() < Duration::from_secs(3), "took {:?}", started.elapsed());
}

#[test]
fn no_key_is_an_auth_error_before_any_request() {
    let t = ScriptedTransport::new(vec![]);
    let seen = t.seen();
    let p = OpenRouterProvider::with_transport(Arc::new(t), None);
    let e = p.stream(&request(), &CancelToken::new(), &mut |_| {}).unwrap_err();
    assert_eq!(e.kind, ProviderErrorKind::Auth);
    assert!(seen.lock().unwrap().is_empty());
}

#[test]
fn errors_inside_a_200_stream_and_early_eof_are_errors() {
    let wire = sse(&[
        json!({"choices":[{"delta":{"content":"par"}}]}),
        json!({"error":{"code":502,"message":"upstream overloaded"},"choices":[{"delta":{},"finish_reason":"error"}]}),
    ]);
    let (p, _) = provider(ScriptedTransport::new(vec![Reply::ok(&wire)]));
    let e = p.stream(&request(), &CancelToken::new(), &mut |_| {}).unwrap_err();
    assert_eq!(e.kind, ProviderErrorKind::Server);
    assert!(e.detail.contains("upstream overloaded"));

    let cut = format!("data: {}\n\n", json!({"choices":[{"delta":{"content":"half"}}]}));
    let (p, _) = provider(ScriptedTransport::new(vec![Reply::ok(&cut)]));
    let e = p.stream(&request(), &CancelToken::new(), &mut |_| {}).unwrap_err();
    assert_eq!(e.kind, ProviderErrorKind::Network);
}

#[test]
fn the_catalog_lists_models_and_informs_effort_mapping() {
    let catalog = json!({"data":[{
        "id": "openai/gpt-5",
        "name": "OpenAI: GPT-5",
        "context_length": 400000,
        "architecture": {"output_modalities": ["text"]},
        "pricing": {"prompt": "0.00000125", "completion": "0.00001"},
        "supported_parameters": ["reasoning", "tools", "max_tokens"],
        "reasoning": {"mandatory": false, "supported_efforts": ["high", "medium", "low", "minimal", "none"]}
    }]});
    let ok = sse(&[json!({"choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}]})]);
    let (p, seen) = provider(ScriptedTransport::new(vec![Reply::ok(&catalog.to_string()), Reply::ok(&ok)]));
    let models = p.list_models().unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].display, "OpenAI: GPT-5");
    assert!((models[0].pricing.unwrap().output_per_mtok - 10.0).abs() < 1e-9);
    assert_eq!(models[0].effort_levels, vec![Effort::Off, Effort::Low, Effort::Medium, Effort::High]);

    let mut req = ChatRequest::new("openai/gpt-5", vec![Message::user("hi")]);
    req.effort = Effort::Off;
    p.stream(&req, &CancelToken::new(), &mut |_| {}).unwrap();
    let seen = seen.lock().unwrap();
    assert_eq!(seen[0].method, "GET");
    assert_eq!(seen[0].path, "/models");
    assert!(seen[0].headers.is_empty(), "the catalog is fetched without the key");
    let body: Value = serde_json::from_str(&seen[1].body).unwrap();
    assert_eq!(body["reasoning"], json!({"effort": "none"}));
}
