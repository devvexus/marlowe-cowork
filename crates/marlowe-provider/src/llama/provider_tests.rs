//! The Ollama provider with Marlowe's llama-server behind it, against fakes of both servers.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use marlowe_model::{
    CancelToken, ChatRequest, ContextAllocations, EngineAdvisor, EngineShape, Lane, Message, Provider, StreamEvent, ToolSpec,
};
use serde_json::{json, Value};

use super::engine::LlamaEngine;
use super::testkit::{chatml_with, Fixture, Reply, Tpl};
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
    engine: LlamaEngine,
}

fn rig(initial: Option<EngineShape>) -> Rig {
    let fx = Fixture::new(MODEL);
    let alloc = ContextAllocations::new();
    let advice = Arc::new(Advice(Mutex::new(initial)));
    alloc.set_engine_advisor(advice.clone());
    let engine = fx.engine();
    let provider = OllamaProvider::new(fx.ollama.endpoint()).with_allocations(alloc).with_llama_engine(engine.clone());
    Rig { fx, provider, advice, engine }
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

// ---- reading the next request's prefix ahead of time ----

const ASK: &str = "<|im_start|>user\nhi<|im_end|>\n<|im_start|>assistant\n";

fn thinking_answer() -> Vec<Value> {
    vec![
        json!({"choices": [{"index": 0, "finish_reason": null, "delta": {"reasoning_content": "weighing it"}}]}),
        json!({"choices": [{"index": 0, "finish_reason": "stop", "delta": {"content": "an answer"}}],
               "timings": {"cache_n": 0, "prompt_n": 9, "prompt_ms": 1.0, "predicted_n": 7, "predicted_ms": 3.0}}),
    ]
}

/// A rig whose server answers like `tpl` and says `thinking_answer`.
fn rig_with(tpl: Tpl) -> Rig {
    let r = rig(Some(shape(2, 16_384)));
    r.fx.spawner.answer_chat(|_| Reply::sse(&thinking_answer()));
    r.fx.spawner.apply_template(move |b| Reply::json(json!({"prompt": chatml_with(b, tpl)})));
    r
}

fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
    let until = std::time::Instant::now() + Duration::from_secs(10);
    while !ready() {
        assert!(std::time::Instant::now() < until, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn the_engine_finds_out_what_the_template_does_with_thinking_and_sends_only_what_it_needs() {
    for (tpl, want) in [
        (Tpl::Keeps, super::Thinking::Keeps),
        (Tpl::Switch, super::Thinking::Switch(vec![("preserve_thinking".into(), true)])),
        (Tpl::Drops, super::Thinking::Drops),
        (Tpl::Ignores, super::Thinking::Ignores),
    ] {
        let r = rig_with(tpl);
        r.provider.stream(&request(Lane::Conductor, "hi"), &CancelToken::new(), &mut |_| {}).unwrap();
        assert_eq!(r.engine.thinking(), Some(want), "{tpl:?}");
        let sent = r.fx.spawner.seen.chats.lock().unwrap()[0].clone();
        let kw = &sent["chat_template_kwargs"];
        assert_eq!(kw.get("preserve_thinking").is_some(), tpl == Tpl::Switch, "{tpl:?}: {kw}");
        assert!(kw.get("clear_thinking").is_none(), "{tpl:?}: {kw}");
    }
}

#[test]
fn a_turn_whose_thinking_the_template_will_drop_is_read_ahead_exactly_as_the_next_request_prints_it() {
    let r = rig_with(Tpl::Drops);
    r.provider.stream(&request(Lane::Conductor, "hi"), &CancelToken::new(), &mut |_| {}).unwrap();
    wait_for("the read-ahead", || r.engine.read_aheads().finished == 1);
    let reads = r.fx.spawner.seen.completions.lock().unwrap().clone();
    assert_eq!(reads.len(), 1);
    let expect = format!("{ASK}an answer<|im_end|>\n");
    assert_eq!(reads[0]["prompt"].as_str().unwrap(), expect, "the turn as it prints once a user message follows (no thinking), up to where that message begins");
    assert_eq!(
        (reads[0]["id_slot"].as_u64(), reads[0]["n_predict"].as_u64(), reads[0]["cache_prompt"].as_bool()),
        (Some(0), Some(0), Some(true)),
        "the conversation's own slot, nothing generated"
    );
    assert_eq!(r.engine.read_aheads().failed, 0);
}

#[test]
fn the_next_request_follows_the_read_ahead_on_the_same_slot_and_starts_with_what_was_read() {
    let r = rig_with(Tpl::Drops);
    r.fx.spawner.completion_takes(Duration::from_millis(500), 200);
    let first = request(Lane::Conductor, "follow the read");
    let done = r.provider.stream(&first, &CancelToken::new(), &mut |_| {}).unwrap();
    // The user answers at once, while the read is still going.
    let mut second = first.clone();
    second.messages.push(done.kept_message().unwrap());
    second.messages.push(Message::user("and then?"));
    r.provider.stream(&second, &CancelToken::new(), &mut |_| {}).unwrap();
    let chats = r.fx.spawner.seen.chats.lock().unwrap().clone();
    assert_eq!(chats.len(), 2);
    assert_eq!(chats[1]["id_slot"], 0, "it did not go around the read to another slot");
    let times = r.fx.spawner.seen.completion_times.lock().unwrap().clone();
    assert_eq!(times.len(), 1);
    assert!(times[0].1.is_some(), "the read had ended when the request began");
    // What the second request renders to starts with what was read.
    let mut sent = chats[1].clone();
    sent["add_generation_prompt"] = json!(false);
    let rendered = chatml_with(&sent, Tpl::Drops);
    let read = r.fx.spawner.seen.completions.lock().unwrap()[0]["prompt"].as_str().unwrap().to_string();
    assert!(rendered.starts_with(&read), "{rendered:?} does not start with {read:?}");
}

#[test]
fn nothing_is_read_ahead_when_the_next_request_will_start_with_the_cache_anyway() {
    for tpl in [Tpl::Keeps, Tpl::Switch, Tpl::Ignores] {
        let r = rig_with(tpl);
        r.provider.stream(&request(Lane::Conductor, "hi"), &CancelToken::new(), &mut |_| {}).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert!(r.fx.spawner.seen.completions.lock().unwrap().is_empty(), "{tpl:?}");
        assert_eq!(r.engine.read_aheads().started, 0, "{tpl:?}");
    }
}

#[test]
fn a_reply_that_calls_a_tool_and_a_crew_agents_answer_are_not_read_ahead() {
    // A tool result follows a tool call, and every template prints that turn the same way.
    let r = rig_with(Tpl::Drops);
    r.fx.spawner.answer_chat(|_| {
        Reply::sse(&[
            json!({"choices": [{"index": 0, "finish_reason": null, "delta": {"reasoning_content": "plan"}}]}),
            json!({"choices": [{"index": 0, "finish_reason": null, "delta": {"tool_calls": [{"index": 0, "id": "c1", "type": "function", "function": {"name": "read", "arguments": "{\"path\":\"a\"}"}}]}}]}),
            json!({"choices": [{"index": 0, "finish_reason": "tool_calls", "delta": {}}], "timings": {"cache_n": 0, "prompt_n": 9, "prompt_ms": 1.0, "predicted_n": 7, "predicted_ms": 3.0}}),
        ])
    });
    let mut req = request(Lane::Conductor, "hi");
    req.tools = vec![ToolSpec { name: "read".into(), description: "Read".into(), input_schema: json!({"type": "object", "properties": {"path": {"type": "string"}}}) }];
    let c = r.provider.stream(&req, &CancelToken::new(), &mut |_| {}).unwrap();
    assert_eq!(c.tool_calls.len(), 1);
    // A crew agent's answer ends its conversation: no user message follows it.
    r.fx.spawner.answer_chat(|_| Reply::sse(&thinking_answer()));
    r.provider.stream(&request(Lane::Other, "do a task"), &CancelToken::new(), &mut |_| {}).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    assert!(r.fx.spawner.seen.completions.lock().unwrap().is_empty());
}

#[test]
fn a_server_that_cannot_read_ahead_costs_one_note_and_the_next_request_just_reads() {
    let r = rig_with(Tpl::Drops);
    r.fx.spawner.completion_takes(Duration::ZERO, 400);
    let first = request(Lane::Conductor, "cannot read ahead");
    let done = r.provider.stream(&first, &CancelToken::new(), &mut |_| {}).unwrap();
    wait_for("the failed read", || r.engine.read_aheads().failed == 1);
    assert!(r.engine.read_aheads().last_error.is_some());
    let mut second = first.clone();
    second.messages.push(done.kept_message().unwrap());
    second.messages.push(Message::user("and then?"));
    let c = r.provider.stream(&second, &CancelToken::new(), &mut |_| {}).unwrap();
    assert_eq!(c.text, "an answer", "the conversation goes on");
}

/// An answer whose generated tokens come back with it.
fn answer_with_tokens(generated: &[&str]) -> Vec<Value> {
    let toks: Vec<Value> = generated.iter().map(|t| json!({"token": t, "bytes": t.as_bytes(), "logprob": -0.1, "top_logprobs": []})).collect();
    vec![
        json!({"choices": [{"index": 0, "finish_reason": null, "delta": {"reasoning_content": "weighing it"}, "logprobs": {"content": toks}}]}),
        json!({"choices": [{"index": 0, "finish_reason": "stop", "delta": {"content": "an answer"}}],
               "timings": {"cache_n": 0, "prompt_n": 9, "prompt_ms": 1.0, "predicted_n": generated.len(), "predicted_ms": 3.0}}),
    ]
}

#[test]
fn a_reply_the_template_prints_differently_from_how_it_was_generated_is_read_ahead_even_when_the_template_keeps_thinking() {
    // The template prints a blank line after the thinking; the model wrote a single newline.
    let r = rig_with(Tpl::Keeps);
    r.fx.spawner.answer_chat(|_| Reply::sse(&answer_with_tokens(&["weighing it", "\n</think>\n", "an answer"])));
    r.provider.stream(&request(Lane::Conductor, "hi"), &CancelToken::new(), &mut |_| {}).unwrap();
    wait_for("the read-ahead", || r.engine.read_aheads().finished == 1);
    let chats = r.fx.spawner.seen.chats.lock().unwrap().clone();
    assert_eq!(chats[0]["n_probs"], 1, "the conductor asks for the generated tokens");
    let reads = r.fx.spawner.seen.completions.lock().unwrap().clone();
    assert_eq!(
        reads[0]["prompt"].as_str().unwrap(),
        format!("{ASK}<think>\nweighing it\n</think>\n\nan answer<|im_end|>\n"),
        "what the next request will print, not what was generated"
    );
}

#[test]
fn a_reply_the_template_prints_as_generated_needs_no_read_ahead() {
    let r = rig_with(Tpl::Keeps);
    r.fx.spawner.answer_chat(|_| Reply::sse(&answer_with_tokens(&["weighing it", "\n</think>\n\n", "an answer"])));
    r.provider.stream(&request(Lane::Conductor, "hi"), &CancelToken::new(), &mut |_| {}).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    assert!(r.fx.spawner.seen.completions.lock().unwrap().is_empty());
}

#[test]
fn a_server_that_refuses_the_token_probabilities_is_not_asked_again() {
    let r = rig_with(Tpl::Keeps);
    r.fx.spawner.answer_chat(|b| {
        if b.get("n_probs").is_some() {
            return Reply::status(400, json!({"error": {"code": 400, "type": "invalid_request_error", "message": "n_probs / logprobs is not supported with tools in streaming mode"}}));
        }
        Reply::sse(&thinking_answer())
    });
    let c = r.provider.stream(&request(Lane::Conductor, "hi"), &CancelToken::new(), &mut |_| {}).unwrap();
    assert_eq!(c.text, "an answer", "the same request went out again without them");
    r.provider.stream(&request(Lane::Conductor, "hi again"), &CancelToken::new(), &mut |_| {}).unwrap();
    let chats = r.fx.spawner.seen.chats.lock().unwrap().clone();
    assert_eq!(chats.iter().filter(|b| b.get("n_probs").is_some()).count(), 1, "only the first request asked");
    assert_eq!(chats.len(), 3);
}

#[test]
fn a_models_template_is_asked_once_per_file_even_across_engine_restarts() {
    let r = rig_with(Tpl::Drops);
    let asks = Arc::new(AtomicUsize::new(0));
    let a = asks.clone();
    r.fx.spawner.apply_template(move |b| {
        if b["messages"].as_array().is_some_and(|m| m.iter().any(|x| x["reasoning_content"] == "RSN-ALPHA")) {
            a.fetch_add(1, Ordering::SeqCst);
        }
        Reply::json(json!({"prompt": chatml_with(b, Tpl::Drops)}))
    });
    let mut cfg = r.fx.config();
    cfg.thinking_cache = Some(r.fx.dir.path().join("thinking.json"));
    let mut after_first = 0;
    for load in 0..2 {
        let engine = LlamaEngine::new(cfg.clone(), r.fx.spawner.clone());
        let alloc = ContextAllocations::new();
        alloc.set_engine_advisor(r.advice.clone());
        let p = OllamaProvider::new(r.fx.ollama.endpoint()).with_allocations(alloc).with_llama_engine(engine.clone());
        p.stream(&request(Lane::Conductor, "hi"), &CancelToken::new(), &mut |_| {}).unwrap();
        assert_eq!(engine.thinking(), Some(super::Thinking::Drops));
        engine.unload(None);
        if load == 0 {
            after_first = asks.load(Ordering::SeqCst);
        }
    }
    // The first load renders the probe conversation (plain, with each switch, and the shorter one
    // that tells a dropping template from a silent one); the second reads the answer from the file.
    assert!(after_first > 0, "the first load asked the template");
    assert_eq!(asks.load(Ordering::SeqCst), after_first, "the second load did not ask again");
}
