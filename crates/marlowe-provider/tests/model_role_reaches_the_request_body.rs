//! **Does the role a spawner named reach the bytes?**
//!
//! This asserts on `request_body`'s `"model"` field, not on `CallLimits::route`'s value. The
//! difference is the whole file. `assert_eq!(limits.route, ModelRoute::Worker)` is green on a
//! build where `request_body` hardcodes the orchestrator — which is exactly what it did until M3
//! Session C, with `CapabilityProfile::model_route()` sitting there with **zero callers in the
//! workspace** and `profile.rs`'s own unit test asserting the string `"worker"` beside it. That
//! is instance #16 with its green proxy test already written.
//!
//! # The negative control is in the same test, and it is not decoration
//!
//! Production runs `Routing::uniform`, where all three columns answer one name. A test written
//! against a uniform routing reads `"one"` whether or not the route is consulted at all, so
//! every assertion here runs against a routing with **three distinct names**, and the uniform
//! case is asserted alongside it as the thing that cannot discriminate. That pairing is what
//! makes a pass mean something.
//!
//! # What this file deliberately does NOT assert
//!
//! **Nothing about `llama.cpp`.** `llama-server` serves whatever weight set it was launched with
//! and does not route on the `"model"` field (`llamacpp.rs`'s own doc comment says so, and so
//! does `ModelProviderChoice::LlamaCpp`'s in the daemon). An assertion that `body["model"]`
//! differed by route on that arm would be green over unrouted weights: instance #15 manufactured
//! inside the fix for instance #16. **No test on the llama.cpp arm may assert that
//! `body["model"]` differs by route.**
//!
//! **Nothing about the shipped daemon.** Every production `Routing` is `Routing::uniform`, so
//! this wiring changes not one byte on the wire today and a green run here proves nothing about
//! the running product. Only a `--dev` outbound dump from the shipped binary under a three-tag
//! routing can — the `persona/v1.md`-loaded-versus-in-the-request-body lesson, one subsystem
//! over.

use marlowe_contract::TrustClass;
use marlowe_loop::{
    Assembler, Block, CallLimits, ContextView, ModelRoute, SessionId, SessionState, SourceKind,
};
use marlowe_provider::{OllamaDriver, Routing};
use marlowe_tools::{builtin_registry, ExposedSet, ToolId};

fn view() -> ContextView {
    let mut state = SessionState::new(SessionId::from_name("model-role"), "Marlowe.");
    state.push(Block::new(
        SourceKind::History,
        "summarise this".to_string(),
        TrustClass::UserAsserted,
    ));
    Assembler::new(8_192, 1_024).assemble(&state)
}

fn tools() -> ExposedSet {
    ExposedSet::new(vec![ToolId::new("read")]).expect("one tool fits")
}

fn model_sent(routing: Routing, route: ModelRoute) -> String {
    let driver = OllamaDriver::new(
        marlowe_provider::LocalEndpoint::default_ollama(),
        routing,
        builtin_registry().expect("the builtins are compiled in"),
    );
    driver
        .request_body(&view(), &tools(), CallLimits { max_output_tokens: 256, route })
        .get("model")
        .and_then(|m| m.as_str())
        .expect("every request names a model")
        .to_string()
}

fn three_roles() -> Routing {
    // Three DISTINCT names. Under `Routing::uniform` this test cannot fail, which is the point
    // of the control at the bottom.
    Routing::new("role-orch", "role-work", "role-summ").expect("three local tags")
}

/// *Mutation:* restore `model_for(marlowe_loop::ModelRoute::Orchestrator)` in
/// `OllamaDriver::request_body` — the second and third assertions read
/// `left: "role-orch", right: "role-work"` / `"role-summ"`.
#[test]
fn the_model_in_the_ollama_request_body_follows_the_route() {
    let orch = model_sent(three_roles(), ModelRoute::Orchestrator);
    let work = model_sent(three_roles(), ModelRoute::Worker);
    let summ = model_sent(three_roles(), ModelRoute::Summarizer);

    // Printed, not just asserted: a failure should say what went out.
    println!("orchestrator -> {orch}\nworker       -> {work}\nsummarizer   -> {summ}");

    assert_eq!(orch, "role-orch");
    assert_eq!(work, "role-work");
    assert_eq!(summ, "role-summ");
    assert_ne!(orch, work, "two roles must be able to reach two models");

    // ── THE NEGATIVE CONTROL: production's own configuration cannot discriminate ─────────
    //
    // Every production `Routing` is `uniform`, so a test written against one would be green on
    // a build where `limits.route` is thrown away. This asserts that reading, so nobody later
    // mistakes a uniform-routing pass for evidence about the wiring.
    let u = || Routing::uniform("only-one").expect("a local tag");
    assert_eq!(model_sent(u(), ModelRoute::Orchestrator), "only-one");
    assert_eq!(model_sent(u(), ModelRoute::Worker), "only-one");
    assert_eq!(
        model_sent(u(), ModelRoute::Orchestrator),
        model_sent(u(), ModelRoute::Worker),
        "under a uniform routing the two roles are indistinguishable ON THE WIRE. That is what \
         the shipped daemon runs, and it is why the assertions above use three tags"
    );
}

/// **Two roles resolving to one model is the ORDINARY case, not a degenerate one.**
///
/// `DECISIONS.md` 2026-08-30: the human's testing configuration points Agent-High and
/// Agent-Medium at the same tag. Nothing here may assume the tiers differ — and the consequence
/// that matters downstream is that anything counting capacity keys on the **resolved model**,
/// never on the role: two roles sharing a model share one set of weights and multiply only the
/// KV cache, where two roles on two models multiply weights.
///
/// *Mutation:* make `Routing::models()` return one entry per column rather than the distinct
/// set — the `models().len()` assertions read 3 and 3.
#[test]
fn two_roles_may_resolve_to_one_model_and_the_table_says_so() {
    let shared = Routing::new("big-one", "big-one", "small-one").expect("two distinct tags");
    assert_eq!(shared.model_for(ModelRoute::Orchestrator), shared.model_for(ModelRoute::Worker));
    assert_eq!(
        shared.models().len(),
        2,
        "capacity is a property of the RESOLVED MODEL, not of the role: two roles on one tag are \
         one weight set"
    );
    assert_eq!(three_roles().models().len(), 3);
    assert_eq!(Routing::uniform("one").unwrap().models().len(), 1);
}
