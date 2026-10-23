//! Serde round-trips for every wire type.

use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::json;

use crate::*;

fn round_trip<T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(v: &T) -> String {
    let s = serde_json::to_string(v).unwrap();
    let back: T = serde_json::from_str(&s).unwrap();
    assert_eq!(&back, v, "round trip changed the value: {s}");
    s
}

#[test]
fn messages_round_trip_with_every_part() {
    let m = Message {
        role: Role::Assistant,
        content: vec![
            Part::Thinking(Thinking {
                text: "consider".into(),
                signature: Some("sig".into()),
                details: Some(json!([{"type": "reasoning.text"}])),
            }),
            Part::Text { text: "Reading it.".into() },
            Part::ToolCall(ToolCall { id: "c1".into(), name: "read".into(), args: json!({"path": "a"}) }),
        ],
    };
    let s = round_trip(&m);
    assert!(s.contains(r#""type":"tool_call""#), "{s}");
    assert!(s.contains(r#""role":"assistant""#), "{s}");
    round_trip(&Message::tool_results([ToolResult {
        call_id: "c1".into(),
        content: "fn main() {}".into(),
        is_error: false,
        parts: Vec::new(),
    }]));
}

#[test]
fn requests_round_trip() {
    let mut r = ChatRequest::new("qwen3:8b", vec![Message::user("hi")]);
    r.effort = Effort::High;
    r.system = vec![SystemBlock { text: "core".into(), cache: true }];
    r.tools = vec![ToolSpec {
        name: "read".into(),
        description: "Read a file".into(),
        input_schema: json!({"type": "object"}),
    }];
    r.temperature = Some(0.5);
    r.max_output_tokens = Some(4096);
    let s = round_trip(&r);
    assert!(s.contains(r#""effort":"high""#), "{s}");
    assert_eq!(r.system_text(), "core");
}

#[test]
fn stream_events_and_completions_round_trip() {
    for ev in [
        StreamEvent::TextDelta("a".into()),
        StreamEvent::ReasoningDelta { text: "b".into(), tokens: 2 },
        StreamEvent::Retract { tokens: 3 },
        StreamEvent::ToolCallStart { index: 0, id: "x".into(), name: "grep".into() },
        StreamEvent::ToolArgsDelta { index: 0, json: "{".into() },
        StreamEvent::ToolCallEnd { index: 0 },
        StreamEvent::Usage(Usage { input: 1, output: 2, reasoning: 0, cached_input: 1, cost_usd: Some(0.5), ..Usage::default() }),
        StreamEvent::Stop(StopReason::ToolUse),
    ] {
        round_trip(&ev);
    }
    round_trip(&Completion {
        text: "t".into(),
        thinking: None,
        tool_calls: vec![ToolCall { id: "1".into(), name: "n".into(), args: json!({}) }],
        usage: Usage::default(),
        stop: StopReason::ToolUse,
    });
}

#[test]
fn model_info_and_refs_round_trip() {
    let info = ModelInfo {
        provider: ProviderKind::OpenRouter,
        id: "anthropic/claude-sonnet-4.5".into(),
        display: "Claude Sonnet 4.5".into(),
        context_window: Some(200_000),
        max_output: Some(64_000),
        supports_tools: true,
        supports_reasoning: true,
        supports_vision: false,
        effort_levels: vec![Effort::Low, Effort::Medium, Effort::High],
        pricing: Some(Pricing { input_per_mtok: 3.0, output_per_mtok: 15.0 }),
        local: false,
    };
    let s = round_trip(&info);
    assert!(s.contains(r#""provider":"openrouter""#), "{s}");
    let r = info.model_ref();
    assert_eq!(r.to_string(), "openrouter/anthropic/claude-sonnet-4.5");
    assert_eq!(round_trip(&r), r#""openrouter/anthropic/claude-sonnet-4.5""#);
    let o: ModelRef = "ollama/qwen3:8b".parse().unwrap();
    assert_eq!(o.provider, ProviderKind::Ollama);
    assert_eq!(o.model_id, "qwen3:8b");
    assert!("qwen3:8b".parse::<ModelRef>().is_err());
    assert!("nowhere/x".parse::<ModelRef>().is_err());
    assert!("ollama/".parse::<ModelRef>().is_err());
    assert!(serde_json::from_str::<ModelRef>(r#""bad""#).is_err());
}

#[test]
fn provider_kinds_and_errors() {
    for k in ProviderKind::ALL {
        round_trip(&k);
        assert_eq!(k.as_str().parse::<ProviderKind>().unwrap(), k);
    }
    assert!(ProviderKind::Ollama.is_available_now());
    assert!(ProviderKind::OpenRouter.is_available_now());
    assert!(!ProviderKind::Anthropic.is_available_now());
    assert!(!ProviderKind::OpenAi.is_available_now());
    assert!(!ProviderKind::LocalGguf.is_available_now());

    let e = ProviderError::from_status(429, "slow down");
    assert_eq!(e.kind, ProviderErrorKind::RateLimit);
    assert!(e.retriable);
    assert!(!ProviderError::from_status(401, "no").retriable);
    round_trip(&e);
}

#[test]
fn effort_parses_and_cycles() {
    assert_eq!("MAX".parse::<Effort>().unwrap(), Effort::Max);
    assert!("extreme".parse::<Effort>().is_err());
    let allowed = [Effort::Low, Effort::Medium, Effort::High];
    assert_eq!(Effort::High.cycle(&allowed), Effort::Low);
    assert_eq!(Effort::Off.cycle(&allowed), Effort::Low);
    assert_eq!(Effort::Max.cycle(&[]), Effort::Max);
}

#[test]
fn usage_adds_across_calls() {
    let mut a = Usage { input: 10, output: 5, cost_usd: None, ..Default::default() };
    a.add(&Usage { input: 1, output: 1, cost_usd: Some(0.25), ..Default::default() });
    assert_eq!(a.total(), 17);
    assert_eq!(a.cost_usd, Some(0.25));
    assert!((Pricing { input_per_mtok: 3.0, output_per_mtok: 15.0 }.cost(1_000_000, 0) - 3.0).abs() < 1e-9);
}

fn png() -> ImageRef {
    ImageRef { mime: "image/png".into(), blob: "ab".repeat(32), w: 1024, h: 768 }
}

#[test]
fn images_round_trip_in_messages_and_tool_results() {
    let m = Message::with_images(Role::User, "what is this?", [png()]);
    let s = round_trip(&m);
    assert!(s.contains(r#"{"type":"image","mime":"image/png","#), "{s}");
    assert_eq!(m.images().count(), 1);

    let r = ToolResult { parts: vec![ResultPart::Image(png())], ..ToolResult::text("c1", "[image 1024x768]", false) };
    let s = round_trip(&Message::tool_results([r.clone()]));
    assert!(s.contains(r#""parts":[{"type":"image""#), "{s}");
    assert_eq!(r.images().next(), Some(&png()));
    assert_eq!(Part::from(ResultPart::Image(png())), Part::Image(png()));
    // A text-only result serializes exactly as before the seam: no `parts` key.
    let plain = serde_json::to_string(&ToolResult::text("c1", "ok", false)).unwrap();
    assert_eq!(plain, r#"{"call_id":"c1","content":"ok","is_error":false}"#);
}

#[test]
fn vision_support_defaults_to_false_and_round_trips() {
    let mut info = ModelInfo::bare(ProviderKind::Anthropic, "claude-x");
    assert!(!info.supports_vision);
    info.supports_vision = true;
    let s = round_trip(&info);
    assert!(s.contains(r#""supports_vision":true"#), "{s}");
}

/// Turns and library rows written before M4.6 (no `parts`, no `supports_vision`) still load.
/// The fixture is the stored format as of M4.5 and is never regenerated.
#[test]
fn pre_m46_stored_json_still_deserialises() {
    let parts: Vec<Vec<Part>> = serde_json::from_str(include_str!("../tests/fixtures/turns_m45.json")).unwrap();
    assert_eq!(parts.len(), 4);
    let results: Vec<&ToolResult> = parts
        .iter()
        .flatten()
        .filter_map(|p| match p {
            Part::ToolResult(r) => Some(r),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 2);
    assert!(results.iter().all(|r| r.parts.is_empty()));
    assert!(results[1].is_error);
    // Re-serialising the loaded value reproduces the same JSON value (nothing gained or lost).
    let again: serde_json::Value = serde_json::to_value(&parts).unwrap();
    let original: serde_json::Value = serde_json::from_str(include_str!("../tests/fixtures/turns_m45.json")).unwrap();
    assert_eq!(again, original);

    let info: ModelInfo = serde_json::from_str(include_str!("../tests/fixtures/model_info_m45.json")).unwrap();
    assert!(!info.supports_vision);
    assert_eq!(info.context_window, Some(131_072));
}

#[test]
fn a_think_budget_is_absent_from_the_wire_when_none_and_round_trips_when_set() {
    let mut req = ChatRequest::new("m", vec![Message::user("hi")]);
    let v = serde_json::to_value(&req).unwrap();
    assert!(v.get("think_budget").is_none());
    // A request stored before the field existed still loads.
    assert_eq!(serde_json::from_value::<ChatRequest>(v).unwrap().think_budget, None);
    req.think_budget = Some(600);
    let v = serde_json::to_value(&req).unwrap();
    assert_eq!(v["think_budget"], 600);
    assert_eq!(serde_json::from_value::<ChatRequest>(v).unwrap(), req);
}
