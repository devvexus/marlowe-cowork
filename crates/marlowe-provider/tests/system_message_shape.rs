//! **Where the `system` messages are — stated PER DRIVER, because that qualifier is the change.**
//!
//! # The constraint this file was written for, and it was real
//!
//! Reported live against a **qwen3-next** architecture model: Ollama answered HTTP 400 with
//!
//! ```text
//! Unable to generate parser for this template. Automatic parser generation failed:
//! ... raise_exception('System message must be at the beginnin ...
//! Error: Jinja Exception: System message must be at the beginning
//! ```
//!
//! The driver emitted **one `system` message per stable block and per context block** — a dozen of
//! them — plus a further one *after the user's turn* for injected memory. Nothing about that
//! diagnosis has been withdrawn: the dozen are still collapsed into one, and the leading message is
//! still at position 0 on both drivers.
//!
//! # What changed, and why the old assertion was measuring the wrong scope
//!
//! *"Exactly one system message"* was read off **one driver serving one model** and written down as
//! a property of the wire. It is not. It is a property of the **renderer**, and the renderer varies:
//!
//! | driver | renderer | trailing `system` |
//! |---|---|---|
//! | `ollama.rs` on `qwen3.5:9b` (the default) | Ollama's **built-in Go renderer** — it ignores the `TEMPLATE` field entirely | accepted |
//! | `wire.rs` (llama.cpp / OpenRouter) | the GGUF's own Jinja, or the hosted provider's | **refused**, and that is the exception quoted above |
//!
//! That the Go renderer is what runs is not inferred from the manifest — `/api/show` reports this
//! model's template as `{{ .Prompt }}`, thirteen characters rendering neither `.System` nor
//! `.Messages`, which reads like the system prompt being discarded and is not. This project
//! **announced that conclusion, was wrong, and caught it with a BANANA control** (a system message
//! saying *reply with exactly the word BANANA*, which came back `BANANA`). CLAUDE.md carries it.
//!
//! So the strict shape is kept exactly where it is enforced — `wire.rs` — and Ollama's driver is
//! allowed the second message, **last**, for a measured reason below. Each test below names its
//! driver in its own name. A test that did not would be the measurement-transfer family again, in
//! the same file that documents it.
//!
//! # Why it moved: a prompt cache is only reused for a byte-identical PREFIX
//!
//! Memory was chained onto the end of the leading system message — **position 0** — and memory is
//! re-retrieved every turn, so one changed fact invalidated the system prompt *and every turn of
//! conversation behind it*. Measured on this machine: a **148-byte** change at the front cost
//! **1.45 s**; over ten turns prompt evaluation climbed **689 → 1,320 ms**, monotone in six passes;
//! the counterfactual with the same content after the history was **flat at ~275 ms regardless of
//! turn count**.
//!
//! `..._leaves_the_..._prefix_byte_identical` is therefore the test with the teeth. Placement
//! assertions are a proxy for that property; the prefix comparison **is** it, and it is the one
//! that fails if a later change puts anything memory-derived back in front of the history.
//!
//! # The instrument that can turn this off, named so a failure points at itself
//!
//! `ollama.rs` reads `MARLOWE_MEMORY_AT_HEAD` and restores the old placement when it is set — an
//! A/B scaffold so both arms can be measured from one binary. Every test here asserts the
//! **shipped default**, and each says so in its failure message: a `cargo test` run from a shell
//! with that variable exported must read as a mis-set instrument, not as a regression.

use marlowe_contract::TrustClass;
use marlowe_loop::{
    Assembler, Block, CallLimits, ContextView, GovernanceConstraint, SessionId, SessionState,
    SourceKind,
};
use marlowe_provider::{OllamaDriver, Routing};
use marlowe_tools::{builtin_registry, ExposedSet, ToolId};
use serde_json::Value;

/// The name of the instrument that reverses the behaviour under test, quoted into every failure
/// so a mis-set shell reads as a mis-set shell.
const SCAFFOLD: &str = "MARLOWE_MEMORY_AT_HEAD";

fn driver() -> OllamaDriver {
    OllamaDriver::new(
        marlowe_provider::LocalEndpoint::default_ollama(),
        Routing::uniform(marlowe_provider::DEFAULT_MODEL).expect("a uniform route"),
        builtin_registry().expect("the builtins are compiled in"),
    )
}

const GREEN: &str = "the user's favourite colour is green";

/// A view shaped like a real mid-conversation turn: identity and governance in the stable tier,
/// a user turn, an assistant turn, **and an injected memory** — the block whose placement is the
/// subject of this file.
///
/// `SessionId::from_name` rather than `SessionId::new`, so two views built here differ in the one
/// field the caller varied and in nothing else. The prefix comparison depends on that.
fn view_recalling(memory: &str) -> ContextView {
    let mut state = SessionState::new(SessionId::from_name("system-message-shape"), "Marlowe.");
    state.assert_governance(GovernanceConstraint::asserted("the workspace is scoped"));
    state.assert_governance(GovernanceConstraint::asserted("finish by replying"));
    state.push(Block::new(
        SourceKind::History,
        "what did I say my favourite colour was?".to_string(),
        TrustClass::UserAsserted,
    ));
    state.push(Block::new(
        SourceKind::InjectedMemory,
        memory.to_string(),
        TrustClass::AgentInferred,
    ));
    state.push(Block::new(
        SourceKind::History,
        "Green.".to_string(),
        TrustClass::AgentInferred,
    ));
    Assembler::new(8_192, 1_024).assemble(&state)
}

fn realistic_view() -> ContextView {
    view_recalling(GREEN)
}

fn tools() -> ExposedSet {
    ExposedSet::new(vec![ToolId::new("read")]).expect("one tool fits")
}

/// The messages **as the Ollama driver would put them on the wire**, not a reconstruction: the
/// body is built by `request_body` and the array is read out of it.
fn ollama_messages(view: &ContextView) -> Vec<Value> {
    driver()
        .request_body(view, &tools(), CallLimits { max_output_tokens: 512, route: marlowe_loop::ModelRoute::Orchestrator })
        .get("messages")
        .and_then(|m| m.as_array())
        .cloned()
        .expect("the request carries messages")
}

/// The messages the llama.cpp / OpenRouter driver sends. One function, both of them — see
/// `wire::openai_messages`.
fn openai_messages(view: &ContextView) -> Vec<Value> {
    marlowe_provider::wire::openai_messages(view)
}

fn roles(messages: &[Value]) -> Vec<String> {
    messages
        .iter()
        .filter_map(|m| m.get("role").and_then(|r| r.as_str()))
        .map(str::to_string)
        .collect()
}

fn content(message: &Value) -> &str {
    message.get("content").and_then(|c| c.as_str()).unwrap_or("")
}

/// Which messages carry the recalled fact, and under which role. **Read off the wire**, so a
/// driver that dropped the memory produces an empty vector rather than a passing test.
fn carrying<'a>(messages: &'a [Value], memory: &str) -> Vec<&'a str> {
    messages
        .iter()
        .filter(|m| content(m).contains(memory))
        .filter_map(|m| m.get("role").and_then(|r| r.as_str()))
        .collect()
}

// ─────────────────────────────────────────────────────────────────────────────────────────
// Placement
// ─────────────────────────────────────────────────────────────────────────────────────────

/// **THE OLLAMA DRIVER NOW SENDS TWO `system` MESSAGES, AND THE SECOND ONE IS LAST.**
///
/// # This is the inverse of `there_is_exactly_one_system_message_and_it_is_first`, deliberately
///
/// That test asserted `system_count == 1`. It is inverted rather than deleted because the reason it
/// existed has not gone away — it has been **scoped**. The Jinja exception it was written against
/// is a property of a template, the template is the model's, and Ollama serving `qwen3.5:9b` does
/// not use one: it applies a built-in Go renderer for the architecture and ignores the `TEMPLATE`
/// field. The strict shape is still asserted, on the driver where the constraint is live, by
/// `the_openai_wire_still_carries_exactly_one_system_message_and_it_leads` below.
///
/// **What this reads on a build without the change:** `system_count` is 1 and the first assertion
/// fails by name. It cannot pass on the old placement.
///
/// # The residual risk, stated rather than left implicit
///
/// A future Ollama model whose architecture *does* go through Jinja would refuse the trailing
/// message. That failure is **loud** — an immediate template error on every turn, not a silent
/// degradation — and the remedy is written down in `ollama.rs`: move onto the assistant+`recall`
/// rails `wire.rs` already uses. The shipped default model is `qwen3.5:9b`, asserted here so that a
/// change of default lands next to this note.
#[test]
fn the_ollama_wire_carries_a_second_system_message_and_it_comes_last() {
    assert_eq!(
        marlowe_provider::DEFAULT_MODEL, "qwen3.5:9b",
        "the trailing system message is safe because OLLAMA'S GO RENDERER serves this \
         architecture. A different default model is a different renderer, and this file's \
         reasoning has to be re-read rather than inherited."
    );

    let messages = ollama_messages(&realistic_view());
    let roles = roles(&messages);

    assert_eq!(
        roles.iter().filter(|r| *r == "system").count(),
        2,
        "the leading tier and the recalled memory are two messages now. One means memory is back \
         at the head — check {SCAFFOLD} is not set. Roles were: {roles:?}"
    );
    assert_eq!(
        roles.first().map(String::as_str),
        Some("system"),
        "the stable tier still leads at position 0; only memory moved. Roles were: {roles:?}"
    );
    assert_eq!(
        roles.last().map(String::as_str),
        Some("system"),
        "the second system message must be LAST. Anywhere else and it is neither the cache fix \
         nor the old shape — it is a third thing nothing has measured. Roles were: {roles:?}"
    );
    assert!(
        roles.len() > 2,
        "the control: a view with a conversation in it must put turns BETWEEN the two system \
         messages, or `first` and `last` could be the same message and both assertions above \
         would be one assertion twice. Roles were: {roles:?}"
    );
}

/// **THE STRICT SHAPE, KEPT WHERE THE CONSTRAINT IS ACTUALLY LIVE.**
///
/// `wire.rs` serves llama.cpp — which renders the GGUF's own Jinja — and OpenRouter, which renders
/// whatever the hosted vendor uses. Neither is Ollama's Go renderer, so
/// `System message must be at the beginning` is a real failure mode on this path and exactly one
/// `system` message, first, is the requirement.
///
/// **What this reads on a build without the change:** identical. This assertion did not move, and
/// that is the point of writing it out separately — the old single test was carrying this property
/// and Ollama's, and only one of them changed.
#[test]
fn the_openai_wire_still_carries_exactly_one_system_message_and_it_leads() {
    let messages = openai_messages(&realistic_view());
    let roles = roles(&messages);

    assert_eq!(
        roles.iter().filter(|r| *r == "system").count(),
        1,
        "a qwen3-next template answers `Jinja Exception: System message must be at the beginning` \
         and refuses the whole request. Roles were: {roles:?}"
    );
    assert_eq!(
        roles.first().map(String::as_str),
        Some("system"),
        "and it must be first, which is the clause the exception names. Roles were: {roles:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────────────────
// The memory still arrives, and still is not attributed to the person
// ─────────────────────────────────────────────────────────────────────────────────────────

/// **The regression that matters more than the count: the memory must still REACH the model.**
///
/// # Replaces `an_injected_memory_reaches_the_model_without_starting_a_second_system_message`
///
/// Only the `without_starting_a_second_system_message` clause was wrong. The two properties that
/// test existed for are unchanged and are asserted harder here:
///
/// * **it arrives at all** — dropping it would make retrieval silently useless while every shape
///   test stayed green, which is a far worse bug than mis-roling it;
/// * **it is not attributed to the person** — §B1 keeps memory out of the interface, and that
///   starts with not pretending it was spoken. A recalled fact arriving as a `user` turn is
///   indistinguishable from something they just said.
///
/// What is added is **where**: at the tail, and **not** inside the leading system message, which is
/// the byte the cache is keyed on.
///
/// **What this reads on a build without the change:** the memory is inside `messages[0]`, so
/// `carrying` returns the leading message rather than the last one and the position assertion
/// fails; the "not in the prefix" assertion fails too.
#[test]
fn an_injected_memory_reaches_ollama_at_the_tail_and_is_not_attributed_to_the_person() {
    let messages = ollama_messages(&realistic_view());

    assert_eq!(
        carrying(&messages, GREEN),
        vec!["system"],
        "the memory must reach the model, exactly once, under the `system` role — not `user` \
         (indistinguishable from something just said) and not `assistant` (the model did not say \
         it). An EMPTY result here means it was dropped entirely."
    );
    assert_eq!(
        content(messages.last().expect("messages")),
        GREEN,
        "and it must be the LAST message, alone — everything before it is what the server's \
         prompt cache is keyed on. If {SCAFFOLD} is set this reads the old placement."
    );
    assert!(
        !content(&messages[0]).contains(GREEN),
        "the memory leaked back into the leading system message, which is position 0 and is the \
         whole cost this change exists to remove: {}",
        content(&messages[0])
    );
}

/// **The same three properties on the driver that cannot take a trailing `system` message.**
///
/// It arrives the way a spawned child's return does: an assistant turn announcing a `recall`,
/// paired **by id** with a `tool` message carrying the text. `recall` is a real tool in the builtin
/// registry, so this is a shape the model already understands rather than a fiction — and the
/// pairing is load-bearing: `unorphan_tool_messages` demotes a `tool` message whose call is missing
/// to `user`, which is precisely the attribution this test forbids.
///
/// **What this reads on a build without the change:** the memory is in `messages[0]`, so
/// `carrying` returns `["system"]` and the assertion fails naming the role it got.
#[test]
fn an_injected_memory_reaches_the_openai_wire_as_a_recall_result_paired_by_id() {
    let messages = openai_messages(&realistic_view());

    assert_eq!(
        carrying(&messages, GREEN),
        vec!["tool"],
        "on this driver the memory is a tool result. `user` would make a recalled fact \
         indistinguishable from something just said; `system` is refused by the template; an \
         EMPTY result means it was dropped."
    );

    let last = messages.last().expect("messages");
    let call = &messages[messages.len() - 2];
    assert_eq!(content(last), GREEN, "the memory must be the final message");
    assert_eq!(
        call.get("role").and_then(|r| r.as_str()),
        Some("assistant"),
        "a `tool` message must be preceded by the assistant turn that called it, or \
         `unorphan_tool_messages` demotes it to `user` and the attribution this test forbids \
         happens by a side door: {call}"
    );
    assert_eq!(
        call.pointer("/tool_calls/0/function/name").and_then(|n| n.as_str()),
        Some("recall"),
        "the announced call must be `recall`, which is a real tool in the exposed set: {call}"
    );
    assert_eq!(
        call.pointer("/tool_calls/0/id"),
        last.get("tool_call_id"),
        "the call and its result must be paired by id; an unpaired pair is what the demotion \
         above is triggered by"
    );
    assert!(
        !content(&messages[0]).contains(GREEN),
        "the memory leaked back into the leading system message: {}",
        content(&messages[0])
    );
}

// ─────────────────────────────────────────────────────────────────────────────────────────
// The property the change exists for
// ─────────────────────────────────────────────────────────────────────────────────────────

/// **CHANGING A MEMORY MUST NOT CHANGE ONE BYTE IN FRONT OF IT.** This is the measurement, as a
/// test.
///
/// # Why this and not the placement assertions above
///
/// Placement is a **proxy**: it moves with the property but does not state it. A future change
/// could keep the trailing message and still put something memory-derived into the prefix — a
/// count, a token budget, a "3 memories recalled" line — and every assertion above would stay
/// green while the 1.45 s came straight back. This asserts the thing itself: two turns of the same
/// session whose retrieval returned different facts must produce **byte-identical** messages up to
/// the tail, because a server reuses its prompt cache only for a byte-identical prefix and the only
/// lever a client has on it is emitting identical bytes.
///
/// **What this reads on a build without the change:** the two prefixes differ at `messages[0]` —
/// the memory is chained onto it — and the assertion fails printing both.
#[test]
fn changing_a_memory_leaves_the_ollama_prefix_byte_identical() {
    let a = ollama_messages(&view_recalling(GREEN));
    let b = ollama_messages(&view_recalling("the user's favourite colour is a much longer blue"));

    assert_eq!(a.len(), b.len(), "the two views differ only in one block's text");
    assert_ne!(
        a.last(),
        b.last(),
        "the control: the tail MUST differ, or this test would pass on a build that dropped the \
         memory from the wire entirely"
    );
    assert_eq!(
        &a[..a.len() - 1],
        &b[..b.len() - 1],
        "a changed memory rewrote a byte in front of the tail. That invalidates the server's \
         prompt cache for the system prompt AND every turn of conversation behind it — measured \
         here at 1.45 s for a 148-byte change, and 689 -> 1,320 ms over ten turns."
    );
}

/// The same property on the llama.cpp / OpenRouter wire, where the tail is **two** messages: the
/// announced `recall` and its result.
///
/// **What this reads on a build without the change:** the prefixes differ at `messages[0]`.
#[test]
fn changing_a_memory_leaves_the_openai_prefix_byte_identical() {
    let a = openai_messages(&view_recalling(GREEN));
    let b = openai_messages(&view_recalling("the user's favourite colour is a much longer blue"));

    assert_eq!(a.len(), b.len(), "the two views differ only in one block's text");
    assert_ne!(a.last(), b.last(), "the control: the tail MUST differ");
    assert_eq!(
        &a[..a.len() - 2],
        &b[..b.len() - 2],
        "a changed memory rewrote a byte in front of the recall pair, so the prefix the server \
         caches is no longer stable across turns"
    );
}

/// The stable tier still leads, so the prefix stays cache-friendly and the persona still comes
/// first inside the leading system message.
#[test]
fn the_identity_still_leads_the_system_message() {
    let messages = ollama_messages(&realistic_view());
    let system = content(&messages[0]);

    assert!(
        system.starts_with("Marlowe."),
        "identity leads the stable tier and therefore the system message: {:?}",
        &system[..system.len().min(80)]
    );
    assert!(
        system.contains("the workspace is scoped"),
        "governance must survive the concatenation"
    );
}
