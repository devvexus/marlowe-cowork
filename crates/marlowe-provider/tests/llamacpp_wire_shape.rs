//! **The six ways a `llama-server` request differs from an Ollama one, asserted on the built body.**
//!
//! # Why every assertion here has the Ollama body beside it
//!
//! "The llamacpp body carries `type: function`" is true of a body that carries it and of a body
//! that carries it *everywhere*, including where it must not. What makes these assertions
//! discriminating is the pair: the same `ContextView`, the same `ExposedSet`, the same limits,
//! built by **both** adapters, and the difference read off. A change that collapsed the two
//! dialects into one would pass every single-sided assertion and fail every one below.
//!
//! That matters more here than in most places, because the two dialects are **mutually
//! incompatible** and it is measured rather than inferred. ADR-060's probe ran all four candidate
//! history shapes against both servers:
//!
//! | shape sent | llama-server | Ollama |
//! |---|---|---|
//! | id, no `type`, object `arguments` — what Ollama's adapter writes | **HTTP 500** `Missing tool call type` | 200, used the result |
//! | + `"type":"function"`, object `arguments` | 200 | 200 |
//! | full OpenAI: `type`, **string** `arguments`, `tool_call_id` | 200 | **HTTP 400** `Value looks like object, but can't find closing '}' symbol` |
//!
//! So neither adapter may adopt the other's shape, and *"just send OpenAI"* — the obvious instinct
//! — is the one that breaks the default provider.
//!
//! # The failure this closes is the seam class, not a unit
//!
//! The `type` field is missing on **iteration 2** of a tool-using turn: the first model call has no
//! assistant history to replay, so it succeeds, and the second dies with a 500. Nothing that tests
//! halves can see it — `parse_step` is right, the tool host is right, and the request in between is
//! malformed only once a tool has run.

use marlowe_contract::TrustClass;
use marlowe_loop::{
    Assembler, Block, CallLimits, ContextView, SessionId, SessionState, SourceKind, WireToolCall,
};
use marlowe_provider::llamacpp::{LlamaCppDriver, SamplingPlan, ScriptedTransport};
use marlowe_provider::ollama_store::Sampling;
use marlowe_provider::{LocalEndpoint, OllamaDriver, Routing};
use marlowe_tools::{builtin_registry, ExposedSet, ToolId};

/// A view holding one **completed tool round trip**: the user asked, the assistant called `read`,
/// and the result came back. That is iteration 2's window, which is the only shape in which the
/// `type` field exists to be missing.
fn view_after_a_tool_call() -> ContextView {
    let mut state = SessionState::new(SessionId::new(), "Marlowe.");
    state.push(Block::new(SourceKind::History, "read notes.md", TrustClass::UserAsserted));
    state.push(Block::assistant_turn(
        "Reading it now.",
        Some("The user wants the file.".to_string()),
        vec![WireToolCall {
            id: "call_1".into(),
            name: "read".into(),
            arguments: serde_json::json!({ "path": "notes.md" }),
        }],
    ));
    state.push(Block::tool_result_for(
        "12 lines · 400 B",
        "read",
        TrustClass::AgentObserved,
        Some("12 lines".to_string()),
        false,
        "call_1",
    ));
    Assembler::new(8_192, 1_024).assemble(&state)
}

fn tools() -> ExposedSet {
    ExposedSet::new(vec![ToolId::new("read")]).expect("one tool fits")
}

fn limits() -> CallLimits {
    CallLimits { max_output_tokens: 512, route: marlowe_loop::ModelRoute::Orchestrator }
}

/// The sampler `qwen3.5:9b`'s 65-byte `.params` blob actually holds, so what reaches the wire can
/// be compared against a real published value rather than a placeholder.
fn qwen_sampling() -> SamplingPlan {
    SamplingPlan::FromOllamaParams {
        model: "qwen3.5:9b".into(),
        sampling: Sampling {
            temperature: Some(1.0),
            top_k: Some(20),
            top_p: Some(0.95),
            presence_penalty: Some(1.5),
            ..Default::default()
        },
    }
}

fn llamacpp_body(sampling: SamplingPlan) -> serde_json::Value {
    LlamaCppDriver::with_transport(
        Box::new(ScriptedTransport::new(Vec::new())),
        "qwen3.5:9b",
        builtin_registry().expect("builtins"),
        sampling,
    )
    .with_context_tokens(32_768)
    .request_body(&view_after_a_tool_call(), &tools(), limits())
}

fn ollama_body() -> serde_json::Value {
    OllamaDriver::new(
        LocalEndpoint::default_ollama(),
        Routing::uniform("qwen3.5:9b").expect("a routable name"),
        builtin_registry().expect("builtins"),
    )
    .with_context_tokens(32_768)
    .request_body(&view_after_a_tool_call(), &tools(), limits())
}

fn assistant_call(body: &serde_json::Value) -> serde_json::Value {
    body["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .find(|m| m.get("tool_calls").is_some())
        .expect("the view holds a completed tool round trip, so a call must be replayed")["tool_calls"][0]
        .clone()
}

#[test]
fn the_replayed_call_carries_a_type_here_and_does_not_on_the_ollama_path() {
    // **The HTTP 500 on iteration 2.** `Failed to parse messages: Missing tool call type`.
    let here = assistant_call(&llamacpp_body(qwen_sampling()));
    assert_eq!(here["type"], serde_json::json!("function"), "{here}");

    // The control. Without it this asserts a constant: a builder that stamped `type` onto every
    // dialect would pass above and break the default provider, which is the direction that
    // matters because Ollama is what a zero-config install runs.
    let there = assistant_call(&ollama_body());
    assert!(
        there.get("type").is_none(),
        "the Ollama body grew a `type` field; that dialect does not take one: {there}"
    );
}

#[test]
fn arguments_are_a_json_string_here_and_a_json_object_on_the_ollama_path() {
    // Ollama answers HTTP 400 to a string here — `Value looks like object, but can't find closing
    // '}' symbol` — and llama-server answers 400 to an object. The two are not interchangeable.
    let here = assistant_call(&llamacpp_body(qwen_sampling()));
    let args = here["function"]["arguments"].as_str().expect("a STRING in this dialect");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(args).expect("and it parses"),
        serde_json::json!({ "path": "notes.md" }),
        "the string must round-trip to the arguments that were sent"
    );

    let there = assistant_call(&ollama_body());
    assert!(
        there["function"]["arguments"].is_object(),
        "the Ollama body must keep an OBJECT here: {there}"
    );
}

#[test]
fn the_tool_result_pairs_by_id_alone_and_carries_no_tool_name_or_thinking() {
    let body = llamacpp_body(qwen_sampling());
    let messages = body["messages"].as_array().expect("messages");
    let result = messages
        .iter()
        .find(|m| m["role"] == "tool")
        .expect("the tool result kept its role, which means it was linked to its call");
    assert_eq!(result["tool_call_id"], serde_json::json!("call_1"));
    assert!(result.get("tool_name").is_none(), "not a field in this dialect: {result}");
    for m in messages {
        assert!(m.get("thinking").is_none(), "reasoning is not replayed in this dialect: {m}");
    }

    // The control: the Ollama body DOES carry both, and losing them there is a defect this project
    // has already paid for — five results, nothing that called them, and a model that narrated
    // instead of acting.
    let there = ollama_body();
    let there_result = there["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .find(|m| m["role"] == "tool")
        .expect("a tool message");
    assert_eq!(there_result["tool_name"], serde_json::json!("read"));
}

#[test]
fn the_context_window_is_absent_here_because_it_is_a_launch_flag() {
    // `num_ctx` is *always* sent on the Ollama path -- omitting it is Ollama's 2048 whatever the
    // model supports, which silently truncated history, memory and tool results until M2 C2e.
    // Here the window belongs to the process, set with `-c` before Marlowe ever connects, so the
    // absence is correct and the DIVERGENCE is what has to be checked instead:
    // `Availability::ContextTooSmall` is the reader, and `llamacpp_context_window.rs` is its test.
    let here = llamacpp_body(qwen_sampling());
    assert!(here.pointer("/options/num_ctx").is_none(), "{here}");
    assert!(here.get("num_ctx").is_none(), "{here}");

    // The control, and it is the one that would catch a regression on the DEFAULT path.
    assert_eq!(
        ollama_body().pointer("/options/num_ctx"),
        Some(&serde_json::json!(32_768)),
        "the Ollama body must never stop sending the window"
    );
}

#[test]
fn the_sampler_reaches_the_wire_and_its_absence_is_a_different_body_rather_than_the_same_one() {
    // **Moving the runtime moves the sampler.** Ollama applies the model's `.params` layer;
    // `llama-server` pointed at the raw blob applies its own defaults — temperature 0.8, top_k 40,
    // presence_penalty 0 — so the same weights answer differently under the same label with
    // nothing reporting the change.
    let with = llamacpp_body(qwen_sampling());
    assert_eq!(with["temperature"], serde_json::json!(1.0));
    assert_eq!(with["top_k"], serde_json::json!(20));
    assert_eq!(with["top_p"], serde_json::json!(0.95));
    assert_eq!(with["presence_penalty"], serde_json::json!(1.5));

    // The control. Without it, a `request_body` that hardcoded these four values would pass, and
    // `--llamacpp-sampling server` would be a flag that changed nothing.
    let without = llamacpp_body(SamplingPlan::ServerDefaults);
    for key in ["temperature", "top_k", "top_p", "presence_penalty"] {
        assert!(
            without.get(key).is_none(),
            "`{key}` was sent under ServerDefaults, so the flag is decorative: {without}"
        );
    }
}

#[test]
fn the_thinking_switch_is_declared_in_this_dialects_spelling_and_moves_with_the_setting() {
    let on = LlamaCppDriver::with_transport(
        Box::new(ScriptedTransport::new(Vec::new())),
        "qwen3.5:9b",
        builtin_registry().expect("builtins"),
        SamplingPlan::ServerDefaults,
    )
    .with_thinking(true)
    .request_body(&view_after_a_tool_call(), &tools(), limits());
    assert_eq!(on.pointer("/chat_template_kwargs/enable_thinking"), Some(&serde_json::json!(true)));

    // The control: a constant `true` would pass the line above and make `--no-thinking` a flag
    // that is read, threaded, and then dropped one function short of the wire.
    let off = LlamaCppDriver::with_transport(
        Box::new(ScriptedTransport::new(Vec::new())),
        "qwen3.5:9b",
        builtin_registry().expect("builtins"),
        SamplingPlan::ServerDefaults,
    )
    .with_thinking(false)
    .request_body(&view_after_a_tool_call(), &tools(), limits());
    assert_eq!(
        off.pointer("/chat_template_kwargs/enable_thinking"),
        Some(&serde_json::json!(false))
    );
}

#[test]
fn an_empty_tool_set_omits_the_key_rather_than_sending_an_empty_array() {
    // Layer 1's quarantined reader is `ExposedSet::empty()`, and `[]` is a schema violation in this
    // dialect rather than "no tools" — which is how *every* quarantined read on the hosted provider
    // became a malformed request. Same wire rule, third adapter.
    let body = LlamaCppDriver::with_transport(
        Box::new(ScriptedTransport::new(Vec::new())),
        "qwen3.5:9b",
        builtin_registry().expect("builtins"),
        SamplingPlan::ServerDefaults,
    )
    .request_body(&view_after_a_tool_call(), &ExposedSet::empty(), limits());
    assert!(body.get("tools").is_none(), "an empty tool set must OMIT the key: {body}");

    // The control: a populated set still reaches the wire, with the tool's own description on it.
    let populated = llamacpp_body(SamplingPlan::ServerDefaults);
    let schema = populated["tools"].as_array().expect("one tool");
    assert_eq!(schema.len(), 1);
    assert_eq!(schema[0]["function"]["name"], serde_json::json!("read"));
    assert!(
        schema[0]["function"]["description"].as_str().is_some_and(|d| !d.is_empty()),
        "the tool's own words must reach the model: {}",
        schema[0]
    );
}

#[test]
fn a_required_parameter_is_marked_required_and_carries_the_tools_own_words() {
    // **`required` is not optional and neither is the description.** Omitting `required` makes
    // every parameter optional, so a model that leaves out the one thing the tool needs produces
    // a call that is VALID against the schema it was given and is then refused by the permission
    // layer for "no declared target" -- our own schema's failure, reported as the model's.
    //
    // `grep`, because `read` deliberately has no required parameter (`path` OR `ref`), so
    // asserting on `read` would assert an empty array and pass on a build that never emitted one.
    let body = LlamaCppDriver::with_transport(
        Box::new(ScriptedTransport::new(Vec::new())),
        "qwen3.5:9b",
        builtin_registry().expect("builtins"),
        SamplingPlan::ServerDefaults,
    )
    .request_body(
        &view_after_a_tool_call(),
        &ExposedSet::new(vec![ToolId::new("grep")]).expect("one tool fits"),
        limits(),
    );
    let params = &body["tools"][0]["function"]["parameters"];
    assert!(
        params["required"].as_array().expect("required").iter().any(|r| r == "pattern"),
        "{params}"
    );
    // And the parameter's own prose, not a sentence generated from its Rust type. This branch
    // existed only in the Ollama copy of `param_description` until ADR-060 collapsed the two, so
    // asserting it here is asserting the collapse rather than the original.
    let described = params["properties"]["pattern"]["description"]
        .as_str()
        .expect("a description");
    assert!(described.starts_with("REQUIRED."), "arity leads: {described}");
    assert!(
        described.len() > "REQUIRED. text.".len(),
        "the generated fallback reached the model instead of the tool's own words: {described}"
    );
}

#[test]
fn the_output_cap_is_bounded_by_the_window_exactly_as_the_other_two_adapters_bound_it() {
    // 200,000 tokens of output against a 32,768-token window is unbounded generation with extra
    // steps, and most of why a reasoning model appeared to run forever.
    let body = LlamaCppDriver::with_transport(
        Box::new(ScriptedTransport::new(Vec::new())),
        "qwen3.5:9b",
        builtin_registry().expect("builtins"),
        SamplingPlan::ServerDefaults,
    )
    .with_context_tokens(32_768)
    .request_body(
        &view_after_a_tool_call(),
        &tools(),
        CallLimits { max_output_tokens: 200_000, route: marlowe_loop::ModelRoute::Orchestrator },
    );
    assert_eq!(body["max_tokens"], serde_json::json!(8_192), "a quarter of the window");
}
