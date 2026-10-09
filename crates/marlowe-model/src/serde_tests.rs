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
        StreamEvent::Usage(Usage { input: 1, output: 2, reasoning: 0, cached_input: 1, cost_usd: Some(0.5) }),
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
