//! The adapter as a **measurement instrument**: attribution, cost, and a bounded retry.
//!
//! # The failure this file exists to prevent
//!
//! OpenRouter routes one model *name* to several upstream providers, at different quantizations,
//! and may change that routing between two requests without the name changing. Two benchmark runs
//! can therefore differ materially while **every label in the output reads identical** — this
//! project's most-logged failure family (*a measurement is scoped to the system it was taken on*)
//! arriving through a boundary that moves on its own, with nobody choosing anything.
//!
//! So the tests below assert that the serving upstream, the resolved model, the generation id, the
//! token counts and the cost reach the run record — and, in the control, that an upstream which
//! was **not** reported is recorded as unreported rather than filled in.

use marlowe_contract::TrustClass;
use marlowe_loop::{
    Assembler, Block, CallLimits, ContextView, ModelDriver, SessionId, SessionState, SourceKind,
};
use marlowe_openrouter::{
    retry::MAX_ATTEMPTS, ApiKey, OpenRouterDriver, Reply, ScriptedTransport, NOT_REPORTED,
};
use marlowe_tools::{builtin_registry, ExposedSet, ToolId};

fn view() -> ContextView {
    let mut state = SessionState::new(SessionId::new(), "Marlowe.");
    state.push(Block::new(SourceKind::History, "hello", TrustClass::UserAsserted));
    Assembler::new(8_192, 1_024).assemble(&state)
}

fn tools() -> ExposedSet {
    ExposedSet::new(vec![ToolId::new("read")]).expect("one tool fits")
}

fn limits() -> CallLimits {
    CallLimits { max_output_tokens: 512, route: marlowe_loop::ModelRoute::Orchestrator }
}

fn driver(transport: ScriptedTransport) -> OpenRouterDriver {
    OpenRouterDriver::new(
        Box::new(transport),
        ApiKey::from_secret("sk-or-v1-test"),
        "anthropic/claude-sonnet-4.5",
        builtin_registry().expect("builtins"),
    )
}

/// A stream carrying everything OpenRouter reports about who answered.
fn attributed_stream() -> String {
    "data: {\"id\":\"gen-9f2c\",\"provider\":\"Amazon Bedrock\",\
     \"model\":\"anthropic/claude-sonnet-4.5\",\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
     data: {\"usage\":{\"prompt_tokens\":1200,\"completion_tokens\":34,\"cost\":0.0041525}}\n\n\
     data: [DONE]\n\n"
        .to_string()
}

#[test]
fn the_serving_upstream_and_the_resolved_model_reach_the_run_record() {
    // **The requirement that makes this an instrument.** A benchmark number without the backing
    // provider recorded is not reproducible, and this project treats an unreproducible number as
    // not existing.
    let mut d = driver(ScriptedTransport::new(vec![Reply::ok(&attributed_stream())]));
    d.call(&view(), &tools(), limits()).expect("the call succeeds");

    let run = d.attribution();
    let call = run.calls.first().expect("one call recorded");
    assert_eq!(call.upstream_provider.as_deref(), Some("Amazon Bedrock"));
    assert_eq!(call.served_model.as_deref(), Some("anthropic/claude-sonnet-4.5"));
    assert_eq!(call.requested_model, "anthropic/claude-sonnet-4.5");
    assert_eq!(
        call.generation_id.as_deref(),
        Some("gen-9f2c"),
        "the generation id is the only way to reconcile the exact settled cost afterwards"
    );
    assert!(call.upstream_is_recorded());
}

#[test]
fn cost_and_tokens_come_from_the_providers_own_fields_and_reach_usage() {
    // `Event::Done`'s `spend_micros_usd` has been zero for the whole of M2 because nothing ever
    // put a number in it. This is where it stops being zero — and the number is **reported**, not
    // estimated from a price table in this repo that would be stale the week it was written.
    let mut d = driver(ScriptedTransport::new(vec![Reply::ok(&attributed_stream())]));
    let call = d.call(&view(), &tools(), limits()).expect("the call succeeds");

    assert_eq!(call.usage.prompt_tokens, 1_200);
    assert_eq!(call.usage.completion_tokens, 34);
    // 0.0041525 USD -> 4153 micros (round-half-up on .5).
    assert_eq!(call.usage.micros_usd, 4_153, "the cost is USD micros, from `usage.cost`");
    assert_eq!(d.attribution().total_micros_usd(), 4_153);
    assert_eq!(d.attribution().total_tokens(), 1_234);
}

#[test]
fn an_unreported_upstream_is_recorded_as_unreported_rather_than_guessed() {
    // **The control.** Every assertion above would also pass against an implementation that
    // filled `upstream_provider` in from the requested model — and that implementation would make
    // a call whose upstream was never reported indistinguishable from one whose was, which is the
    // whole failure the field exists to catch.
    let bare = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n";
    let mut d = driver(ScriptedTransport::new(vec![Reply::ok(bare)]));
    d.call(&view(), &tools(), limits()).expect("the call succeeds");

    let call = &d.attribution().calls[0];
    assert_eq!(call.upstream_provider, None);
    assert!(!call.upstream_is_recorded());
    assert!(call.disclosure().contains(NOT_REPORTED), "{}", call.disclosure());
    assert!(
        !call.disclosure().contains("upstream anthropic/claude-sonnet-4.5"),
        "the requested model was substituted for the upstream: {}",
        call.disclosure()
    );
}

#[test]
fn a_run_whose_upstream_changes_between_calls_says_so() {
    // OpenRouter is allowed to do this and it is often why a run finished at all. It is not an
    // error; it is a fact the run record must carry, because a per-run average across two
    // different quantizations is a number about no system.
    let first = "data: {\"provider\":\"Anthropic\",\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\n\ndata: [DONE]\n\n";
    let second = "data: {\"provider\":\"Google Vertex\",\"choices\":[{\"delta\":{\"content\":\"b\"}}]}\n\ndata: [DONE]\n\n";
    let mut d = driver(ScriptedTransport::new(vec![Reply::ok(first), Reply::ok(second)]));
    d.call(&view(), &tools(), limits()).expect("call 1");
    d.call(&view(), &tools(), limits()).expect("call 2");

    let run = d.attribution();
    assert!(run.upstream_changed_mid_run());
    assert!(run.disclosure().contains("CHANGED"), "{}", run.disclosure());
    assert_eq!(run.upstreams(), vec!["Anthropic", "Google Vertex"], "sorted, so a run record is stable");
}

#[test]
fn the_attribution_sink_fires_once_per_call_so_a_harness_can_stream_it() {
    // A benchmark harness wants this per case, not at the end of a run it may not reach.
    let recorded = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let target = std::sync::Arc::clone(&recorded);
    let mut d = driver(ScriptedTransport::new(vec![
        Reply::ok(&attributed_stream()),
        Reply::ok(&attributed_stream()),
    ]))
    .with_attribution_sink(Box::new(move |c| {
        target.lock().expect("sink").push(c.disclosure());
    }));

    d.call(&view(), &tools(), limits()).expect("call 1");
    d.call(&view(), &tools(), limits()).expect("call 2");

    let recorded = recorded.lock().expect("sink");
    assert_eq!(recorded.len(), 2, "the sink must fire per call, or this test asserts nothing");
    assert!(recorded[0].contains("Amazon Bedrock"), "{}", recorded[0]);
}

// ─────────────────────────────────────────────────────────────────────────────────────────
// Retry
// ─────────────────────────────────────────────────────────────────────────────────────────

#[test]
fn a_rate_limit_is_retried_and_the_run_record_says_how_many_attempts_it_took() {
    // A benchmark is thousands of calls and some will be rate-limited. Not retrying makes the
    // instrument fragile — but a retry that is invisible makes the LATENCY number a lie, so the
    // attempt count is recorded rather than absorbed.
    let mut d = driver(ScriptedTransport::new(vec![
        Reply::rate_limited(Some("0")),
        Reply::rate_limited(Some("0")),
        Reply::ok(&attributed_stream()),
    ]));
    d.call(&view(), &tools(), limits()).expect("the third attempt succeeds");

    assert_eq!(
        d.attribution().calls[0].attempts,
        3,
        "the run record must show that this call cost three round trips"
    );
}

#[test]
fn retrying_is_bounded_and_the_bound_is_the_named_constant() {
    // **Audit finding E8's shape.** An unbounded retry loop looks like resilience until the day it
    // looks like a hang. The assertion is on the COUNT OF ATTEMPTS the transport saw — not on the
    // error message, which would be identical for a loop that ran forever and was killed.
    let transport =
        ScriptedTransport::new(vec![]).then_forever(Reply::rate_limited(Some("0")));
    let seen = transport.seen();
    let mut d = driver(transport);

    let err = d.call(&view(), &tools(), limits()).expect_err("a permanent 429 must fail");

    // **Two assertions, and the second is the one with teeth.**
    //
    // The count check reads `MAX_ATTEMPTS`, so it is green for every value of it — a mutation
    // raising the constant to 40 left this test passing, which is a proxy that moves with the
    // thing it is supposed to check. It stays because it pins the loop to the constant; the
    // property that a turn cannot be stalled is asserted against a literal ceiling in
    // `retry::MAX_TOTAL_RETRY_WAIT`.
    assert_eq!(
        seen.lock().expect("seen").len(),
        MAX_ATTEMPTS as usize,
        "the transport was called a different number of times than MAX_ATTEMPTS allows"
    );
    assert!(
        marlowe_openrouter::retry::worst_case_total_wait()
            <= marlowe_openrouter::retry::MAX_TOTAL_RETRY_WAIT,
        "this call's retry schedule can stall a turn past the stated ceiling"
    );
    assert!(
        err.detail.contains(&format!("of {MAX_ATTEMPTS}")),
        "the error must say how many attempts were made, not report the last one as the only one: {}",
        err.detail
    );
    assert!(err.retriable, "a rate limit is worth a failover to another provider");
}

#[test]
fn a_credential_failure_is_not_retried() {
    // The control for the test above. Retrying a 401 spends time and money to receive the same
    // answer, and it turns a two-second fix into a slow one.
    let transport = ScriptedTransport::new(vec![]).then_forever(Reply::status(
        401,
        "{\"error\":{\"message\":\"No auth credentials found\"}}",
    ));
    let seen = transport.seen();
    let mut d = driver(transport);

    let err = d.call(&view(), &tools(), limits()).expect_err("a 401 must fail");
    assert_eq!(
        seen.lock().expect("seen").len(),
        1,
        "a bad credential must be attempted exactly once"
    );
    assert!(!err.retriable);
    assert!(err.detail.contains(marlowe_openrouter::KEY_VAR), "{}", err.detail);
}

#[test]
fn a_transport_fault_is_retried_and_then_reported_with_a_remedy_rather_than_a_panic() {
    // Invariant 4: an absent network is a declared failure, not a crash. `ScriptedTransport` with
    // an empty script and no `then_forever` returns a transport fault on every call.
    let transport = ScriptedTransport::new(vec![]);
    let seen = transport.seen();
    let mut d = driver(transport);

    let err = d.call(&view(), &tools(), limits()).expect_err("an unreachable host must fail");
    assert_eq!(seen.lock().expect("seen").len(), MAX_ATTEMPTS as usize);
    assert!(err.retriable, "an unreachable host is worth a failover");
    assert!(err.detail.contains("openrouter.ai"), "the error must name the host: {}", err.detail);
}
