//! The Ollama provider with Marlowe's llama-server behind it, against fakes of both servers.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use marlowe_model::{
    CancelToken, ChatRequest, ContextAllocations, EngineAdvisor, EngineShape, Lane, Message, Provider, StreamEvent, ToolSpec,
};
use serde_json::{json, Value};

use super::testkit::{Fixture, Reply};
use crate::ollama::OllamaProvider;

const MODEL: &str = "dusk:22b";

struct Advice(Mutex<Option<EngineShape>>);

impl EngineAdvisor for Advice {
    fn shape(&self, _: &str) -> Option<EngineShape> {
        self.0.lock().unwrap().clone()
    }
}

fn shape(slots: u16, ctx: u32) -> EngineShape {
    EngineShape { slots, ctx_per_slot: ctx, kv: "q8_0".into() }
}

struct Rig {
    fx: Fixture,
    provider: OllamaProvider,
    advice: Arc<Advice>,
}

fn rig(initial: Option<EngineShape>) -> Rig {
    let fx = Fixture::new(MODEL);
    let alloc = ContextAllocations::new();
    let advice = Arc::new(Advice(Mutex::new(initial)));
    alloc.set_engine_advisor(advice.clone());
    let provider = OllamaProvider::new(fx.ollama.endpoint()).with_allocations(alloc).with_llama_engine(fx.engine());
    Rig { fx, provider, advice }
}

fn request(lane: Lane, first: &str) -> ChatRequest {
    let mut r = ChatRequest::new(MODEL, vec![Message::user(first)]);
    r.lane = lane;
    r
}

fn answer(text: &str, timings: Value) -> Vec<Value> {
    vec![json!({"choices": [{"index": 0, "finish_reason": "stop", "delta": {"content": text}}], "timings": timings})]
}

#[test]
fn a_model_the_planner_gave_the_engine_is_served_by_llama_server_not_ollama() {
    let r = rig(Some(shape(3, 16_384)));
    r.fx.spawner.answer_chat(|_| {
        Reply::sse(&[
            json!({"choices": [{"index": 0, "finish_reason": null, "delta": {}}], "prompt_progress": {"total": 40, "cache": 10, "processed": 40, "time_ms": 5}}),
            json!({"choices": [{"index": 0, "finish_reason": "stop", "delta": {"content": "hello from llama"}}],
                   "timings": {"cache_n": 10, "prompt_n": 30, "prompt_ms": 25.0, "predicted_n": 4, "predicted_ms": 20.0}}),
        ])
    });
    let mut events = Vec::new();
    let c = r.provider.stream(&request(Lane::Conductor, "hi"), &CancelToken::new(), &mut |e| events.push(e)).unwrap();
    assert_eq!(c.text, "hello from llama");
    assert_eq!(r.fx.ollama.chats(), 0, "Ollama never saw the request");
    assert_eq!((c.usage.input, c.usage.cached_input, c.usage.prompt_eval), (40, 10, None));
    assert_eq!((c.usage.prefill_ms, c.usage.decode_ms), (Some(25), Some(20)));
    assert!(events.iter().any(|e| matches!(e, StreamEvent::ReadProgress { total: 40, cached: 10, processed: 40 })));
    assert!(matches!(events.first(), Some(StreamEvent::Sent)), "the prefill clock starts once the slot is held");
    // The conductor's conversation is pinned to slot 0 and the cache is asked for.
    let sent = r.fx.spawner.seen.chats.lock().unwrap()[0].clone();
    assert_eq!((sent["id_slot"].as_u64(), sent["cache_prompt"].as_bool()), (Some(0), Some(true)));
    // The server was launched for 3 slots of 16k each, the model's own sampler applied.
    let plan = r.fx.spawner.plans.lock().unwrap()[0].clone();
    let at = |f: &str| plan.args.iter().position(|a| a == f).map(|i| plan.args[i + 1].clone());
    assert_eq!((at("-np").as_deref(), at("-c").as_deref()), (Some("3"), Some("49152")));
    assert_eq!(sent["top_k"], 20);
}

#[test]
fn tool_calls_come_back_as_the_loop_expects_them() {
    let r = rig(Some(shape(2, 16_384)));
    r.fx.spawner.answer_chat(|_| {
        Reply::sse(&[
            json!({"choices": [{"index": 0, "finish_reason": null, "delta": {"tool_calls": [{"index": 0, "id": "c1", "type": "function", "function": {"name": "read", "arguments": "{\"path\":"}}]}}]}),
            json!({"choices": [{"index": 0, "finish_reason": null, "delta": {"tool_calls": [{"index": 0, "function": {"arguments": "\"a.rs\"}"}}]}}]}),
            json!({"choices": [{"index": 0, "finish_reason": "tool_calls", "delta": {}}], "timings": {"cache_n": 0, "prompt_n": 9, "prompt_ms": 1.0, "predicted_n": 7, "predicted_ms": 3.0}}),
        ])
    });
    let mut req = request(Lane::Conductor, "read a.rs");
    req.tools = vec![ToolSpec {
        name: "read".into(),
        description: "Read a file".into(),
        input_schema: json!({"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}),
    }];
    let c = r.provider.stream(&req, &CancelToken::new(), &mut |_| {}).unwrap();
    assert_eq!(c.tool_calls.len(), 1);
    assert_eq!((c.tool_calls[0].name.as_str(), &c.tool_calls[0].args), ("read", &json!({"path": "a.rs"})));
    let sent = r.fx.spawner.seen.chats.lock().unwrap()[0].clone();
    assert_eq!(sent["tools"][0]["function"]["name"], "read");
}

#[test]
fn six_conversations_run_at_once_each_on_its_own_slot_and_the_conductor_keeps_slot_zero() {
    let r = rig(Some(shape(6, 16_384)));
    let inflight = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let (i, p) = (inflight.clone(), peak.clone());
    r.fx.spawner.answer_chat(move |body| {
        let now = i.fetch_add(1, Ordering::SeqCst) + 1;
        p.fetch_max(now, Ordering::SeqCst);
        // Long enough that every caller is in the server together.
        std::thread::sleep(Duration::from_millis(600));
        i.fetch_sub(1, Ordering::SeqCst);
        let who = body["messages"][0]["content"].as_str().unwrap_or("?").to_string();
        Reply::sse(&answer(&format!("reply to {who}"), json!({"cache_n": 0, "prompt_n": 5, "prompt_ms": 1.0, "predicted_n": 2, "predicted_ms": 1.0})))
    });
    let provider = Arc::new(r.provider);
    let mut threads = Vec::new();
    for n in 0..6 {
        let provider = provider.clone();
        threads.push(std::thread::spawn(move || {
            let lane = if n == 0 { Lane::Conductor } else { Lane::Other };
            let req = request(lane, &format!("agent {n}"));
            provider.stream(&req, &CancelToken::new(), &mut |_| {}).map(|c| c.text)
        }));
    }
    let mut texts: Vec<String> = threads.into_iter().map(|t| t.join().unwrap().unwrap()).collect();
    texts.sort();
    assert_eq!(texts, (0..6).map(|n| format!("reply to agent {n}")).collect::<Vec<_>>(), "each agent got its own answer");
    assert_eq!(peak.load(Ordering::SeqCst), 6, "all six were in the server at once");
    let sent = r.fx.spawner.seen.chats.lock().unwrap().clone();
    let slot_of = |who: &str| sent.iter().find(|b| b["messages"][0]["content"] == who).unwrap()["id_slot"].as_u64().unwrap();
    assert_eq!(slot_of("agent 0"), 0);
    let mut crew: Vec<u64> = (1..6).map(|n| slot_of(&format!("agent {n}"))).collect();
    crew.sort();
    assert_eq!(crew, vec![1, 2, 3, 4, 5], "five agents, five distinct slots, none the conductor's");
    assert_eq!(r.fx.spawner.starts(), 1);
}

#[test]
fn a_model_the_planner_did_not_assign_stays_on_ollama() {
    let r = rig(None);
    let c = r.provider.stream(&request(Lane::Conductor, "hi"), &CancelToken::new(), &mut |_| {}).unwrap();
    assert_eq!(c.text, "from ollama");
    assert_eq!(r.fx.ollama.chats(), 1);
    assert_eq!(r.fx.spawner.starts(), 0, "no llama-server was started");
}

#[test]
fn turning_the_engine_off_stops_the_server_before_ollama_loads_the_model() {
    let r = rig(Some(shape(2, 16_384)));
    r.provider.stream(&request(Lane::Conductor, "hi"), &CancelToken::new(), &mut |_| {}).unwrap();
    assert_eq!(r.fx.spawner.killed(), 0);
    *r.advice.0.lock().unwrap() = None;
    let c = r.provider.stream(&request(Lane::Conductor, "hi"), &CancelToken::new(), &mut |_| {}).unwrap();
    assert_eq!(c.text, "from ollama");
    assert_eq!(r.fx.spawner.killed(), 1, "the model is never in memory twice");
}

#[test]
fn a_prompt_too_big_for_its_slot_is_refused_before_anything_is_sent() {
    let r = rig(Some(shape(6, 2_048)));
    let big = "word ".repeat(6_000);
    let e = r.provider.stream(&request(Lane::Other, &big), &CancelToken::new(), &mut |_| {}).err().unwrap();
    assert!(crate::ollama::is_context_overflow(&e), "{e:?}");
    assert_eq!(r.fx.spawner.starts(), 0, "no server was started for a request that cannot fit");
}

#[test]
fn llama_servers_own_context_error_becomes_the_same_overflow_the_loop_compacts_on() {
    let r = rig(Some(shape(2, 16_384)));
    r.fx.spawner.answer_chat(|_| {
        Reply::status(400, json!({"error": {"code": 400, "type": "exceed_context_size_error", "message": "too big", "n_prompt_tokens": 20000, "n_ctx": 16384}}))
    });
    let e = r.provider.stream(&request(Lane::Conductor, "hi"), &CancelToken::new(), &mut |_| {}).err().unwrap();
    assert!(crate::ollama::is_context_overflow(&e), "{e:?}");
}

/// Two requests of one conversation through the provider, the second carrying the first's reply as
/// the loop keeps it, against a fake template that does (`keep`) or does not honour the switch.
fn two_requests(keep: bool) -> (Vec<Value>, Vec<String>) {
    let r = rig(Some(shape(2, 16_384)));
    r.fx.spawner.answer_chat(|_| {
        Reply::sse(&[
            json!({"choices": [{"index": 0, "finish_reason": null, "delta": {"reasoning_content": "weighing it"}}]}),
            json!({"choices": [{"index": 0, "finish_reason": "stop", "delta": {"content": "an answer"}}],
                   "timings": {"cache_n": 0, "prompt_n": 9, "prompt_ms": 1.0, "predicted_n": 7, "predicted_ms": 3.0}}),
        ])
    });
    r.fx.spawner.apply_template(move |b| Reply::json(json!({"prompt": super::testkit::chatml(b, keep)})));
    let first = request(Lane::Conductor, "hi");
    let done = r.provider.stream(&first, &CancelToken::new(), &mut |_| {}).unwrap();
    let mut second = first.clone();
    second.messages.push(done.kept_message().expect("a reply"));
    second.messages.push(Message::user("and then?"));
    r.provider.stream(&second, &CancelToken::new(), &mut |_| {}).unwrap();
    let sent = r.fx.spawner.seen.chats.lock().unwrap().clone();
    let key = marlowe_model::prefix::conversation_key(&second);
    (sent, marlowe_model::prefix::take_render_breaks(MODEL, key))
}

#[test]
fn the_engine_asks_the_template_to_keep_earlier_reasoning_and_a_template_that_obeys_extends_the_prompt() {
    let (sent, breaks) = two_requests(true);
    assert_eq!(sent[1]["chat_template_kwargs"]["preserve_thinking"], true);
    assert_eq!(sent[1]["messages"][1]["reasoning_content"], "weighing it", "the reply goes back with its reasoning");
    assert!(breaks.is_empty(), "{breaks:?}");
}

#[test]
fn a_template_that_drops_earlier_reasoning_anyway_is_reported_with_the_byte() {
    let (_, breaks) = two_requests(false);
    assert_eq!(breaks.len(), 1, "{breaks:?}");
    assert!(breaks[0].contains("weighing it"), "{}", breaks[0]);
}
