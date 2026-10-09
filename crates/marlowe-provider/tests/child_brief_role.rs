//! **A child's window must contain a turn the child did not take.**
//!
//! `Engine::spawn` pushed the task as `SourceKind::History` at `TrustClass::AgentInferred`, and
//! both drivers derive the wire role from precisely that pair: `History | ChildResults` +
//! `AgentInferred` is `role: "assistant"`. So every spawned child was handed
//!
//! ```text
//! system:    <identity, governance>
//! assistant: <the task>
//! ```
//!
//! — a conversation with **no user turn in it at all**, ending on the model's own message with
//! nothing to answer. Observed live 2026-08-26 (journal seq 4597–4601): the child returned nothing
//! three times running and the loop failed it with *"the model produced no reply and no tool call
//! 3 times in a row"*. Four model calls, ~12k tokens, no output.
//!
//! **Every unit test in the workspace passed while this was true.** `parse_step` parsed correctly,
//! the budget arithmetic held, the contract validated, `spawn` created the child and settled it.
//! Each half was right; the seam between the spawn and the wire was wrong, and nothing that tests
//! halves can see a seam — the same shape as `done` being routed to a tool host with no executor.
//!
//! So this asserts on the **bytes the provider is sent**, not on the block that produced them.
//! Asserting `child_state`'s last block is `SourceKind::Brief` would be the declaration-site
//! check this project keeps catching itself making: green on a build where the role mapping does
//! the wrong thing, and green if the driver were deleted.

use marlowe_contract::TrustClass;
use marlowe_loop::{
    Assembler, Block, CallLimits, ContextView, GovernanceConstraint, SessionId, SessionState,
    SourceKind,
};
use marlowe_provider::{OllamaDriver, Routing};
use marlowe_tools::{builtin_registry, ExposedSet};

const TASK: &str = "Summarise this text: the quick brown fox.";

/// A freshly spawned child's window, built the way `Engine::spawn` builds one: inherited
/// governance, a fresh session, and the brief as its only volatile block.
fn a_freshly_spawned_child() -> ContextView {
    let mut state = SessionState::new(SessionId::new(), "Marlowe.");
    state.assert_governance(GovernanceConstraint::asserted("the workspace is scoped"));
    state.push(Block::new(
        SourceKind::Brief,
        format!("{TASK}\n\nReturn: what you found"),
        // **Unchanged, and deliberately so.** The parent's model composed this text. Pushing it
        // as `UserAsserted` would make the role come out right by laundering a trust class, which
        // is what `Provenance::new()` is reset to prevent two lines away in `spawn`.
        TrustClass::AgentInferred,
    ));
    Assembler::new(8_192, 1_024).assemble(&state)
}

fn driver() -> OllamaDriver {
    OllamaDriver::new(
        marlowe_provider::LocalEndpoint::default_ollama(),
        Routing::uniform(marlowe_provider::DEFAULT_MODEL).expect("a uniform route"),
        builtin_registry().expect("the builtins are compiled in"),
    )
}

fn messages(body: &serde_json::Value) -> Vec<(String, String)> {
    body.get("messages")
        .and_then(|m| m.as_array())
        .expect("the request carries messages")
        .iter()
        .map(|m| {
            (
                m.get("role").and_then(|r| r.as_str()).unwrap_or_default().to_string(),
                m.get("content").and_then(|c| c.as_str()).unwrap_or_default().to_string(),
            )
        })
        .collect()
}

#[test]
fn a_child_is_asked_its_task_rather_than_shown_it_as_its_own_words() {
    // No tools: the exact profile of the child that went silent.
    let tools = ExposedSet::new(vec![]).expect("a toolless child is constructible");
    let body =
        driver().request_body(&a_freshly_spawned_child(), &tools, CallLimits { max_output_tokens: 512, route: marlowe_loop::ModelRoute::Orchestrator });
    let messages = messages(&body);

    let brief = messages
        .iter()
        .find(|(_, content)| content.contains(TASK))
        .unwrap_or_else(|| panic!("the brief did not reach the wire at all: {messages:?}"));

    assert_eq!(
        brief.0, "user",
        "the child's brief went out as `{}`. A model handed its own message with nothing to \
         answer returns nothing, three times, and the run fails: {messages:?}",
        brief.0,
    );

    // The property underneath, stated so a future change that satisfies the line above by some
    // other route still has to satisfy this: the conversation ends on a turn addressed TO the
    // model. That is what makes it answerable.
    let last = messages.last().expect("at least one message");
    assert_eq!(
        last.0, "user",
        "a child's conversation ends on a `{}` turn, so there is nothing for it to reply to: \
         {messages:?}",
        last.0,
    );
}

/// The negative control. `History` + `AgentInferred` — the parent's own prior reply — must STILL
/// be `assistant`, or this fix has traded one silent model for the self-answering loop the role
/// mapping was written to end (*"Nothing in particular." → "Nothing yet either."*).
#[test]
fn the_parents_own_prior_reply_is_still_its_own() {
    let mut state = SessionState::new(SessionId::new(), "Marlowe.");
    state.push(Block::new(
        SourceKind::History,
        "what is the capital of France?".to_string(),
        TrustClass::UserAsserted,
    ));
    state.push(Block::new(
        SourceKind::History,
        "Paris.".to_string(),
        TrustClass::AgentInferred,
    ));
    let view = Assembler::new(8_192, 1_024).assemble(&state);
    let tools = ExposedSet::new(vec![]).expect("empty is constructible");
    let body = driver().request_body(&view, &tools, CallLimits { max_output_tokens: 512, route: marlowe_loop::ModelRoute::Orchestrator });
    let messages = messages(&body);

    let reply = messages
        .iter()
        .find(|(_, c)| c.contains("Paris."))
        .expect("the reply reached the wire");
    assert_eq!(
        reply.0, "assistant",
        "Marlowe's own answer went out as `{}` — it will read it as something the user said and \
         answer it: {messages:?}",
        reply.0,
    );
}
