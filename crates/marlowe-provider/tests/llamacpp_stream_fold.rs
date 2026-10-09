//! **The SSE fold, driven through the real driver on bytes copied off a real socket.**
//!
//! # Where these bytes come from
//!
//! ADR-060's probe recorded `llama-server 0.32.5 (b1-b4d6c7d8f)`'s streaming wire verbatim, over
//! 168 tool-call trials against `qwen3.5:9b`. The fragments below are that transcript, not a shape
//! invented to match the parser:
//!
//! ```text
//! {"role":"assistant","content":null}
//! {"reasoning_content":"The"}                       ← reasoning first, 168/168
//! {"index":0,"id":"hxCojJBd…","type":"function","function":{"name":"read","arguments":"{"}}
//! {"index":0,"function":{"arguments":"\"path\":\""}}
//! {"index":0,"function":{"arguments":"src"}}
//! {"index":0,"function":{"arguments":"/main"}} … {"index":0,"function":{"arguments":"}"}}
//! finish_reason="tool_calls" ; data: [DONE]
//! ```
//!
//! # Why a scripted transport rather than `#[ignore]` against a live server
//!
//! An `#[ignore]`d test that needs a running `llama-server` is a test that silently passes when
//! nothing is running — this project's recorded shape for a measurement that never happened.
//! Replacing the socket and nothing else drives **the real `request_body`, the real
//! `SseStream`, the real fold and the real `parse_step`**, so what is asserted is the fate of the
//! bytes rather than a re-implementation of the parser inside the test.
//!
//! # The failure each test is named for
//!
//! `arguments` arrives as a **JSON string, in fragments**, where Ollama sends one object.
//! `ollama::parse_step` reads `.as_object()`, which returns `None` for a string — so a fold that
//! passed the string through, or that assigned each fragment instead of appending, produces a call
//! with the **right tool name and no arguments**. The permission layer then refuses it for "no
//! declared target", and a model that never erred is reported as one that did.

use marlowe_contract::TrustClass;
use marlowe_loop::{
    Assembler, Block, CallLimits, ContextView, ModelDriver, ModelStep, SessionId, SessionState,
    SourceKind,
};
use marlowe_provider::llamacpp::{LlamaCppDriver, SamplingPlan, ScriptedTransport};
use marlowe_permission::ArgValue;
use marlowe_tools::{builtin_registry, ExposedSet, ToolId};

fn view() -> ContextView {
    let mut state = SessionState::new(SessionId::new(), "Marlowe.");
    state.push(Block::new(SourceKind::History, "read src/main.rs", TrustClass::UserAsserted));
    Assembler::new(8_192, 1_024).assemble(&state)
}

fn tools() -> ExposedSet {
    ExposedSet::new(vec![ToolId::new("read")]).expect("one tool fits")
}

fn limits() -> CallLimits {
    CallLimits { max_output_tokens: 512, route: marlowe_loop::ModelRoute::Orchestrator }
}

fn driver(wire: &str) -> LlamaCppDriver {
    LlamaCppDriver::with_transport(
        Box::new(ScriptedTransport::new(vec![wire.as_bytes().to_vec()])),
        "qwen3.5:9b",
        builtin_registry().expect("builtins"),
        SamplingPlan::ServerDefaults,
    )
}

/// The tool-call transcript, fragment for fragment.
const TOOL_CALL_WIRE: &str = concat!(
    "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":null}}]}\n\n",
    "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"The \"}}]}\n\n",
    "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"user wants the file.\"}}]}\n\n",
    "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"hxCojJBd0P51vDuvpbiK8f3BgGJ6z2kO\",\"type\":\"function\",\"function\":{\"name\":\"read\",\"arguments\":\"{\"}}]}}]}\n\n",
    "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"path\\\":\\\"\"}}]}}]}\n\n",
    "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"src\"}}]}}]}\n\n",
    "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"/main\"}}]}}]}\n\n",
    "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\".rs\"}}]}}]}\n\n",
    "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"\"}}]}}]}\n\n",
    "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"}\"}}]}}]}\n\n",
    "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
    "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":6737,\"completion_tokens\":41}}\n\n",
    "data: [DONE]\n\n",
);

#[test]
fn a_tool_call_split_across_ten_fragments_reassembles_into_one_call_with_its_arguments() {
    let call = driver(TOOL_CALL_WIRE)
        .call(&view(), &tools(), limits())
        .expect("the scripted stream decodes");

    let ModelStep::ToolCall { calls } = call.step else {
        panic!("the transcript ends `finish_reason: tool_calls`; got {:?}", call.step);
    };
    assert_eq!(calls.len(), 1, "ten fragments of ONE call, keyed by index");
    assert_eq!(calls[0].tool.as_str(), "read");
    // **The assertion with teeth.** A fold that assigned instead of appending yields `"}"`, which
    // parses to nothing; one that passed the string through yields no arguments at all. Both leave
    // the tool name correct, which is why asserting the name alone would prove nothing.
    assert_eq!(
        calls[0].args.get("path").and_then(ArgValue::as_text),
        Some("src/main.rs"),
        "the fragments did not reassemble: {:?}",
        calls[0].args
    );
}

#[test]
fn reasoning_goes_to_the_reasoning_channel_and_never_to_the_reply() {
    // §B1 and ADR-030: the reasoning channel is separate, and a `</think>` reaching the screen is
    // the loudest possible statement that the split was wrong. `reasoning_content` is the spelling
    // this server uses — 168/168 first deltas — and reading only `thinking` (Ollama's) would send
    // every byte of it nowhere.
    let mut speech = String::new();
    let mut reasoning = String::new();
    let mut reasoning_tokens = 0u64;
    let call = driver(TOOL_CALL_WIRE)
        .call_streaming_split(
            &view(),
            &tools(),
            limits(),
            &mut |d| speech.push_str(d),
            &mut |r, t| {
                reasoning.push_str(r);
                reasoning_tokens += t;
            },
            &mut |_| {},
        )
        .expect("the scripted stream decodes");

    assert_eq!(reasoning, "The user wants the file.");
    assert!(speech.is_empty(), "a turn that called a tool did not answer: {speech:?}");
    assert!(matches!(call.step, ModelStep::ToolCall { .. }));

    // **The count is the engine's, not the fold's, and this transcript proves the difference.**
    // Two reasoning chunks arrived; the server said it generated 41 tokens. A count of chunks
    // reports `2` for a turn that spent 41 — the failure this whole mechanism exists to remove.
    // The 39 the server counted and never streamed are the tool call it emitted and the
    // delimiters around it, and this turn never answered, so they are thinking.
    assert_eq!(
        reasoning_tokens, 41,
        "the settlement must charge what the engine says it generated, not what it streamed"
    );
}

#[test]
fn the_token_counts_reach_usage_so_the_budget_has_something_to_count() {
    // `stream_options.include_usage` is why these are on the wire at all. Without them the
    // budget's token accounting reads zero for every call and `Budget::exhausted` -- the backstop
    // this project has exercised exactly once, on a real turn -- never fires on tokens.
    let call = driver(TOOL_CALL_WIRE)
        .call(&view(), &tools(), limits())
        .expect("the scripted stream decodes");
    assert_eq!(call.usage.prompt_tokens, 6_737);
    assert_eq!(call.usage.completion_tokens, 41);
    // A local server costs nothing, and a run record that invented a price would carry a number no
    // provider reported.
    assert_eq!(call.usage.micros_usd, 0);
}

#[test]
fn llama_cpp_s_own_timings_are_read_when_the_usage_block_is_absent() {
    // The two are different SOURCES of one fact, not two readers of one source, and either can be
    // missing depending on the build and on `stream_options`. A zero token count would silently
    // disable the only hard cap on generation, so both are read and `usage` wins.
    let wire = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
        "data: {\"choices\":[],\"timings\":{\"prompt_n\":31,\"predicted_n\":7,",
        "\"prompt_ms\":5.9,\"predicted_ms\":64.1}}\n\n",
        "data: [DONE]\n\n",
    );
    let call = driver(wire).call(&view(), &tools(), limits()).expect("decodes");
    assert_eq!(call.usage.prompt_tokens, 31);
    assert_eq!(call.usage.completion_tokens, 7);
    assert_eq!(call.usage.wall_ms, 70, "prompt_ms + predicted_ms, rounded");
}

#[test]
fn text_arrives_in_pieces_rather_than_in_one_lump_at_the_end() {
    // **The assertion with teeth is the COUNT.** A driver that buffered the whole body and handed
    // the caller one delta satisfies "the text is correct" exactly as well as one that streams,
    // which is why "the reply arrived" is not the property. The blocking is in the reader, not
    // only in the request.
    let wire = concat!(
        "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"...\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"Not\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"es \"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"say hello.\"}}]}\n\n",
        "data: [DONE]\n\n",
    );
    let mut deltas: Vec<String> = Vec::new();
    let call = driver(wire)
        .call_streaming(&view(), &tools(), limits(), &mut |d| deltas.push(d.to_string()))
        .expect("decodes");
    assert_eq!(deltas, vec!["Not", "es ", "say hello."]);
    // `ModelDriver::call_streaming`'s contract: the deltas are exactly the text of the returned
    // step. The engine relies on it -- emitting both would double every reply.
    assert_eq!(call.step, ModelStep::Say("Notes say hello.".into()));
}

#[test]
fn speech_is_held_until_the_think_block_is_known_shut_and_a_close_tag_never_reaches_the_reply() {
    // **`closed` starts `!thinking`, which is the Ollama rule rather than OpenRouter's `true`.**
    // The GGUF's template emits the opening `<think>` as part of the generation prompt, so content
    // can begin INSIDE a block that never appeared on the wire. This is the
    // `--reasoning-format deepseek-legacy` shape: tags back inside `content`.
    //
    // The property is §B1's: everything after `<think>` stays in the container until `</think>` is
    // emitted. Streaming it and retracting later ends correct and still shows the wrong colour on
    // screen first, which is the thing the rule forbids.
    let wire = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"weighing it up\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"</think>\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"The answer is 4.\"}}]}\n\n",
        "data: [DONE]\n\n",
    );
    let mut speech: Vec<String> = Vec::new();
    let mut reasoning = String::new();
    let mut retracted = false;
    let call = driver(wire)
        .with_thinking(true)
        .call_streaming_split(
            &view(),
            &tools(),
            limits(),
            &mut |d| speech.push(d.to_string()),
            &mut |r, _| reasoning.push_str(r),
            &mut |_| retracted = true,
        )
        .expect("decodes");

    assert_eq!(call.step, ModelStep::Say("The answer is 4.".into()));
    assert_eq!(
        speech.join(""),
        "The answer is 4.",
        "text from before the close tag reached the reply: {speech:?}"
    );
    assert!(reasoning.contains("weighing it up"), "the held text must be routed, not dropped");
    for s in &speech {
        assert!(!s.contains("</think>"), "a close tag reached the reply: {s:?}");
    }
    // The control on the control: nothing was streamed and then taken back, because nothing was
    // streamed before the tag. `retract` firing here would mean speech DID reach the surface first.
    assert!(!retracted, "speech was rendered and then retracted, which is what holding prevents");
}

#[test]
fn an_error_object_inside_a_200_stream_fails_the_call_rather_than_returning_an_empty_reply() {
    // A server can open the response and then report a failure in-band. Ignoring it produces an
    // empty reply and a run that looks like the model chose to say nothing.
    let wire = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n",
        "data: {\"error\":{\"message\":\"context shift is disabled\"}}\n\n",
        "data: [DONE]\n\n",
    );
    let e = driver(wire).call(&view(), &tools(), limits()).expect_err("must fail");
    assert!(e.detail.contains("context shift is disabled"), "{}", e.detail);
    assert!(e.retriable, "a mid-stream fault is worth another attempt");
}

#[test]
fn the_done_sentinel_ends_the_stream_and_nothing_after_it_is_read() {
    // `[DONE]` is a sentinel, not JSON. A decoder that parsed every `data:` payload reports a
    // malformed frame on the last frame of EVERY successful call -- an error on the success path,
    // which is the kind that gets suppressed and then hides a real one.
    let wire = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"done.\"}}]}\n\n",
        "data: [DONE]\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\" and more\"}}]}\n\n",
    );
    let call = driver(wire).call(&view(), &tools(), limits()).expect("the sentinel is not an error");
    assert_eq!(call.step, ModelStep::Say("done.".into()), "text after [DONE] was read");
}

#[test]
fn the_loop_control_tools_are_routed_through_the_one_parse_step_that_exists() {
    // `ask`, `remember` and `run` are `ModelStep` variants, not tool-host executions. A provider
    // that mapped them to `ToolCall` sends them to a host with no executor -- the defect CLAUDE.md
    // records from M2's first real run, where the model spent 155 seconds acting on a failure it
    // could not interpret. There is ONE definition of that routing and this driver reuses it.
    let wire = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"a\",\"type\":\"function\",",
        "\"function\":{\"name\":\"ask\",\"arguments\":\"{\\\"question\\\":\\\"which file?\\\"}\"}}]}}]}\n\n",
        "data: [DONE]\n\n",
    );
    let call = driver(wire).call(&view(), &tools(), limits()).expect("decodes");
    assert_eq!(call.step, ModelStep::Ask("which file?".into()));
}

#[test]
fn a_fragment_that_does_not_reassemble_becomes_an_empty_object_rather_than_a_guess() {
    // A call with invented arguments is a harness action attributed to the model. The refusal that
    // follows -- "no declared target" -- is legible; a guessed path is not.
    let wire = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"a\",\"type\":\"function\",",
        "\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\"}}]}}]}\n\n",
        "data: [DONE]\n\n",
    );
    let call = driver(wire).call(&view(), &tools(), limits()).expect("decodes");
    let ModelStep::ToolCall { calls } = call.step else {
        panic!("a named tool is still a call: {:?}", call.step)
    };
    assert_eq!(calls[0].tool.as_str(), "read");
    assert!(
        calls[0].args.get("path").is_none(),
        "a truncated fragment must not become an argument: {:?}",
        calls[0].args
    );
}

#[test]
fn two_calls_in_one_reply_keep_their_order_and_stay_two_calls() {
    // The batch order is decided by a `BTreeMap` keyed on `index`, not by a `HashMap` whose
    // iteration order is randomised per process -- which would be a `repro` hash that varies per
    // process. The fragments below arrive INTERLEAVED, which is what makes the key load-bearing.
    let wire = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"a\",\"type\":\"function\",",
        "\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"b\",\"type\":\"function\",",
        "\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"one.md\\\"}\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"function\":{\"arguments\":\"\\\"two.md\\\"}\"}}]}}]}\n\n",
        "data: [DONE]\n\n",
    );
    let call = driver(wire).call(&view(), &tools(), limits()).expect("decodes");
    let ModelStep::ToolCall { calls } = call.step else { panic!("{:?}", call.step) };
    assert_eq!(calls.len(), 2, "taking only the first is a silent loss of an action");
    assert_eq!(calls[0].args.get("path").and_then(ArgValue::as_text), Some("one.md"));
    assert_eq!(calls[1].args.get("path").and_then(ArgValue::as_text), Some("two.md"));
}

#[test]
fn the_transport_saw_the_body_the_driver_actually_built() {
    // The point of the port: what is asserted downstream is the fate of bytes the REAL
    // `request_body` produced, not of a body assembled inside the test.
    let transport = ScriptedTransport::new(vec![TOOL_CALL_WIRE.as_bytes().to_vec()]);
    let seen = transport.seen();
    let mut d = LlamaCppDriver::with_transport(
        Box::new(transport),
        "qwen3.5:9b",
        builtin_registry().expect("builtins"),
        SamplingPlan::ServerDefaults,
    );
    d.call(&view(), &tools(), limits()).expect("decodes");

    let sent = seen.lock().expect("seen");
    assert_eq!(sent.len(), 1, "one call, one request");
    assert_eq!(sent[0]["stream"], serde_json::json!(true));
    assert_eq!(
        sent[0].pointer("/stream_options/include_usage"),
        Some(&serde_json::json!(true)),
        "without this the token counts above never arrive"
    );
    assert_eq!(sent[0]["tools"][0]["function"]["name"], serde_json::json!("read"));
}
/// **`timings_per_token` is the whole reason this counter can be called exact.**
///
/// Without it `usage` arrives once, on the last chunk, and a thinking line would sit still for the
/// length of a turn and then jump. With it every chunk carries the engine's running `predicted_n`,
/// so the number on screen is the model's own count while the model is still producing it.
///
/// The wire below is deliberately **not** one token per chunk: `predicted_n` steps 1, 3, 4, 9. That
/// is the case a frame count gets wrong, and it is not hypothetical — Ollama's thinking channel
/// does exactly this, measured 3-10% low. Charging the *increment* to the channel the chunk fed is
/// what makes the chunk shape irrelevant.
#[test]
fn the_engines_running_count_is_charged_to_the_channel_the_chunk_fed() {
    let wire = concat!(
        "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"one \"}}],",
        "\"timings\":{\"predicted_n\":1}}\n\n",
        // Two tokens in one chunk. A frame count reads 1 here and is already wrong.
        "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"two three\"}}],",
        "\"timings\":{\"predicted_n\":3}}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}],",
        "\"timings\":{\"predicted_n\":4}}\n\n",
        // Five generated and never streamed: a closing delimiter and a stop sequence. The last
        // channel open was speech, so they belong to no thinking block and are not charged to one.
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":9},",
        "\"timings\":{\"prompt_n\":11,\"predicted_n\":9,\"prompt_ms\":1.0,\"predicted_ms\":2.0}}\n\n",
        "data: [DONE]\n\n",
    );
    let mut speech = String::new();
    let mut reasoning = String::new();
    let mut reasoning_tokens = 0u64;
    driver(wire)
        .call_streaming_split(
            &view(),
            &tools(),
            limits(),
            &mut |d| speech.push_str(d),
            &mut |r, t| {
                reasoning.push_str(r);
                reasoning_tokens += t;
            },
            &mut |_| {},
        )
        .expect("decodes");

    assert_eq!(reasoning, "one two three");
    assert_eq!(speech, "hi");
    // 1 + 2. **Not 2**, which is what counting chunks gives, and not 13, which is what counting
    // characters gives — the two numbers this line has reported in its life, neither of them a
    // token count.
    assert_eq!(reasoning_tokens, 3, "the increment, not the chunk, is the token count");
}

/// The negative control for the test above: **take the engine's counter off the wire and the
/// number changes.**
///
/// Same three chunks, same text, no `timings` anywhere. The fold falls back to one token per chunk
/// and reads `2` where the engine would have said `3`. That degradation is deliberate — wrong by a
/// little beats frozen at zero, which reads as a hung model — and pinning it here is what stops
/// the fallback quietly becoming the only path.
#[test]
fn without_the_engines_counter_the_fold_falls_back_to_one_per_chunk() {
    let wire = concat!(
        "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"one \"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"two three\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
        "data: [DONE]\n\n",
    );
    let mut reasoning_tokens = 0u64;
    driver(wire)
        .call_streaming_split(
            &view(),
            &tools(),
            limits(),
            &mut |_| {},
            &mut |_, t| reasoning_tokens += t,
            &mut |_| {},
        )
        .expect("decodes");

    assert_eq!(
        reasoning_tokens, 2,
        "no counter on the wire means one charge per chunk, which is the weaker answer"
    );
}

/// **The request has to ASK for the per-token timings, or none of the above happens.**
///
/// Instance #16's question — *is there a line of code that reads this control?* — asked in the
/// other direction: is there a line that WRITES it. A fold reading `predicted_n` from chunks that
/// never carry it degrades silently to a chunk count, and every test above stays green.
#[test]
fn the_request_asks_for_per_token_timings() {
    let body = driver(TOOL_CALL_WIRE).request_body(&view(), &tools(), limits());
    assert_eq!(
        body.get("timings_per_token").and_then(serde_json::Value::as_bool),
        Some(true),
        "without this the engine reports its count once, at the end: {body}"
    );
}
