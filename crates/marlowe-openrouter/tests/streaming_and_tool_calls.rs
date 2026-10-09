//! The reply streams, and a tool call reassembles from fragments.
//!
//! M2 C2e made *"the reply streams again"* a hard requirement, and the hazard it recorded is that
//! **the blocking is in the reader, not only in the request**: a decoder that collects the whole
//! body before returning is correct and fatal to streaming, and nothing about the request shows
//! it. So the assertions here are about **when** text arrives, not only that it does.
//!
//! The tool-call half is the wire difference that actually bites. OpenAI-shaped streams deliver
//! `function.arguments` as a **string in fragments**, keyed by `index`. A driver that treats each
//! fragment as a complete call produces a call whose arguments are truncated — valid against the
//! schema it was given, and useless. ADR-034 already records that shape from the other direction.

use marlowe_contract::TrustClass;
use marlowe_loop::{
    Assembler, Block, CallLimits, ContextView, ModelDriver, ModelStep, SessionId, SessionState,
    SourceKind,
};
use marlowe_openrouter::{ApiKey, OpenRouterDriver, Reply, ScriptedTransport};
use marlowe_permission::ArgValue;
use marlowe_tools::{builtin_registry, ExposedSet, ToolId};

fn view() -> ContextView {
    let mut state = SessionState::new(SessionId::new(), "Marlowe.");
    state.push(Block::new(
        SourceKind::History,
        "read notes.md".to_string(),
        TrustClass::UserAsserted,
    ));
    Assembler::new(8_192, 1_024).assemble(&state)
}

fn driver(body: &str) -> OpenRouterDriver {
    OpenRouterDriver::new(
        Box::new(ScriptedTransport::new(vec![Reply::ok(body)])),
        ApiKey::from_secret("sk-or-v1-test"),
        "anthropic/claude-sonnet-4.5",
        builtin_registry().expect("builtins"),
    )
}

fn tools() -> ExposedSet {
    ExposedSet::new(vec![ToolId::new("read")]).expect("one tool fits")
}

fn limits() -> CallLimits {
    CallLimits { max_output_tokens: 512, route: marlowe_loop::ModelRoute::Orchestrator }
}

#[test]
fn text_arrives_in_pieces_rather_than_in_one_lump_at_the_end() {
    // **The assertion with teeth is the COUNT.** A driver that buffered the whole body and then
    // handed the caller one delta would satisfy "the text is correct" exactly as well as one that
    // streams — which is why "the reply arrived" is not the property C2e asked for.
    let wire = "data: {\"choices\":[{\"delta\":{\"content\":\"Not\"}}]}\n\n\
                data: {\"choices\":[{\"delta\":{\"content\":\"es \"}}]}\n\n\
                data: {\"choices\":[{\"delta\":{\"content\":\"say \"}}]}\n\n\
                data: {\"choices\":[{\"delta\":{\"content\":\"hello.\"}}]}\n\n\
                data: [DONE]\n\n";

    let mut deltas: Vec<String> = Vec::new();
    let call = driver(wire)
        .call_streaming(&view(), &tools(), limits(), &mut |d| deltas.push(d.to_string()))
        .expect("the scripted call succeeds");

    assert_eq!(deltas, vec!["Not", "es ", "say ", "hello."], "the pieces must arrive as pieces");
    assert_eq!(call.step, ModelStep::Say("Notes say hello.".into()));
}

#[test]
fn the_streamed_deltas_are_exactly_the_text_of_the_returned_step() {
    // `ModelDriver::call_streaming`'s contract, stated in `driver.rs`: *"on_delta receives exactly
    // the text that ends up in the returned ModelStep::Say, in order"*. The engine relies on it —
    // emitting both would double every reply, which is a bug this project has already shipped once.
    let wire = "data: {\"choices\":[{\"delta\":{\"content\":\"one \"}}]}\n\n\
                data: {\"choices\":[{\"delta\":{\"content\":\"two\"}}]}\n\n\
                data: [DONE]\n\n";
    let mut streamed = String::new();
    let call = driver(wire)
        .call_streaming(&view(), &tools(), limits(), &mut |d| streamed.push_str(d))
        .expect("the scripted call succeeds");
    match call.step {
        ModelStep::Say(text) => assert_eq!(streamed, text),
        other => panic!("expected prose, got {other:?}"),
    }
}

#[test]
fn a_tool_call_split_across_chunks_reassembles_into_one_call() {
    // The OpenAI streaming shape: the id and name on the first fragment, the arguments arriving a
    // few characters at a time, all joined by `index`. A driver that assigned rather than appended
    // would produce `read` with `path` truncated — a well-formed call to the wrong file.
    let wire = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_abc\",\
                \"function\":{\"name\":\"read\",\"arguments\":\"\"}}]}}]}\n\n\
                data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":\
                {\"arguments\":\"{\\\"pa\"}}]}}]}\n\n\
                data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":\
                {\"arguments\":\"th\\\":\\\"notes.md\\\"}\"}}]}}]}\n\n\
                data: [DONE]\n\n";

    let call = driver(wire).call(&view(), &tools(), limits()).expect("the call succeeds");
    match call.step {
        ModelStep::ToolCall { calls } => {
            assert_eq!(calls.len(), 1, "the fragments are ONE call, not three");
            assert_eq!(calls[0].tool.as_str(), "read");
            assert_eq!(
                calls[0].args.get("path").and_then(ArgValue::as_text),
                Some("notes.md"),
                "the argument fragments must be joined, not overwritten"
            );
        }
        other => panic!("expected a tool call, got {other:?}"),
    }
}

#[test]
fn two_calls_in_one_message_stay_two_calls_and_keep_their_order() {
    // A batch is one step because the model composed every call before seeing any result — which
    // is what makes computing taint once for the batch correct. Dropping or reordering one is
    // silent: the model believes it took an action that never happened.
    let wire = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[\
                {\"index\":0,\"id\":\"a\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\\\"a.md\\\"}\"}},\
                {\"index\":1,\"id\":\"b\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\\\"b.md\\\"}\"}}\
                ]}}]}\n\ndata: [DONE]\n\n";
    let call = driver(wire).call(&view(), &tools(), limits()).expect("the call succeeds");
    match call.step {
        ModelStep::ToolCall { calls } => {
            let paths: Vec<Option<&str>> =
                calls.iter().map(|c| c.args.get("path").and_then(ArgValue::as_text)).collect();
            assert_eq!(paths, vec![Some("a.md"), Some("b.md")]);
        }
        other => panic!("expected two tool calls, got {other:?}"),
    }
}

#[test]
fn the_loop_control_tools_are_routed_through_the_one_parse_step_that_exists() {
    // `ask` is a `ModelStep` variant, not a tool-host execution. A second `parse_step` in this
    // crate would be a second idea of what `ask` means, and the first real end-to-end run of the
    // Ollama adapter is the record of what that costs: 155 seconds acting on a failure the model
    // could not interpret.
    let wire = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"q\",\
                \"function\":{\"name\":\"ask\",\"arguments\":\"{\\\"question\\\":\\\"which file?\\\"}\"}}]}}]}\n\n\
                data: [DONE]\n\n";
    let call = driver(wire).call(&view(), &tools(), limits()).expect("the call succeeds");
    assert_eq!(call.step, ModelStep::Ask("which file?".into()));
}

#[test]
fn reasoning_goes_to_the_reasoning_channel_and_never_to_the_reply() {
    // A `</think>` on screen is the loudest possible statement that the channel split was wrong.
    // OpenRouter normalises every upstream's chain of thought into `reasoning`.
    let wire = "data: {\"choices\":[{\"delta\":{\"reasoning\":\"the user wants notes.md\"}}]}\n\n\
                data: {\"choices\":[{\"delta\":{\"content\":\"Here it is.\"}}]}\n\n\
                data: [DONE]\n\n";
    let mut speech = String::new();
    let mut thought = String::new();
    let call = driver(wire)
        .call_streaming_split(
            &view(),
            &tools(),
            limits(),
            &mut |d| speech.push_str(d),
            &mut |r, _| thought.push_str(r),
            &mut |_| {},
        )
        .expect("the call succeeds");
    assert_eq!(thought, "the user wants notes.md");
    assert_eq!(speech, "Here it is.");
    assert_eq!(call.step, ModelStep::Say("Here it is.".into()));
}

#[test]
fn an_error_object_inside_a_200_stream_fails_the_call_rather_than_returning_an_empty_reply() {
    // OpenRouter opens the response and then reports a mid-stream upstream failure as an `error`
    // object. Ignoring it produces an empty reply — and an empty reply, under M2 C2e's rule that
    // completion is the absence of an action, ENDS THE TURN. The model would look as though it
    // chose to say nothing.
    let wire = "data: {\"choices\":[{\"delta\":{\"content\":\"Let me \"}}]}\n\n\
                data: {\"error\":{\"code\":502,\"message\":\"upstream disconnected\"}}\n\n";
    let err = driver(wire)
        .call(&view(), &tools(), limits())
        .expect_err("a mid-stream error must fail the call");
    assert!(err.detail.contains("upstream disconnected"), "{}", err.detail);
    assert!(err.retriable, "a dropped upstream is worth another attempt");
}

#[test]
fn the_request_carries_the_output_cap_the_window_and_the_determinism_controls() {
    // Every one of these is a number that, omitted, is chosen by somebody else. `num_ctx` was
    // omitted for the whole of M2 and Ollama silently applied 2048; the same shape here is
    // `max_tokens` unset and the provider picking one.
    let d = OpenRouterDriver::new(
        Box::new(ScriptedTransport::new(vec![])),
        ApiKey::from_secret("k"),
        "anthropic/claude-sonnet-4.5",
        builtin_registry().expect("builtins"),
    )
    .with_context_tokens(32_768)
    .with_seed(Some(7))
    .with_pinned_upstream(vec!["Anthropic".into()]);

    let body = d.request_body(&view(), &tools(), CallLimits { max_output_tokens: 200_000, route: marlowe_loop::ModelRoute::Orchestrator });

    assert_eq!(
        body["max_tokens"], 8_192,
        "the cap must be clamped to a quarter of the window; 200k of output against a 32k window \
         is unbounded generation with extra steps"
    );
    assert_eq!(body["temperature"], 0.0);
    assert_eq!(body["seed"], 7);
    assert_eq!(body["usage"]["include"], true, "without this there is no cost and no token count");
    assert_eq!(body["provider"]["order"][0], "Anthropic");
    assert_eq!(
        body["provider"]["allow_fallbacks"], false,
        "with fallbacks allowed the `order` is a preference, and a pin that is a preference is \
         the unrecorded-upstream problem with a control that looks like it closed it"
    );
    assert_eq!(body["stream"], true);
}

#[test]
fn a_replayed_assistant_tool_call_serialises_its_arguments_as_a_string() {
    // The OpenAI dialect wants `function.arguments` as a JSON **string**. Sending an object gets
    // the request rejected with a 400 on the turn AFTER a tool ran, which reads like a tool bug.
    let mut state = SessionState::new(SessionId::new(), "Marlowe.");
    state.push(Block::new(
        SourceKind::History,
        "read notes.md".to_string(),
        TrustClass::UserAsserted,
    ));
    let mut assistant = Block::new(
        SourceKind::History,
        String::new(),
        TrustClass::AgentInferred,
    );
    assistant.wire = Some(marlowe_loop::WireTurn {
        thinking: None,
        tool_calls: vec![marlowe_loop::WireToolCall {
            id: "call_1".into(),
            name: "read".into(),
            arguments: serde_json::json!({ "path": "notes.md" }),
        }],
        tool_name: None,
        tool_call_id: None,
        tool_summary: None,
        tool_failed: false,
    });
    state.push(assistant);
    let view = Assembler::new(8_192, 1_024).assemble(&state);

    let d = OpenRouterDriver::new(
        Box::new(ScriptedTransport::new(vec![])),
        ApiKey::from_secret("k"),
        "m",
        builtin_registry().expect("builtins"),
    );
    let body = d.request_body(&view, &tools(), limits());
    let args = body["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .find_map(|m| m.pointer("/tool_calls/0/function/arguments").cloned())
        .expect("a replayed assistant tool call");
    assert!(args.is_string(), "arguments must be a JSON string, got {args}");
    assert_eq!(args.as_str().unwrap(), "{\"path\":\"notes.md\"}");
}
