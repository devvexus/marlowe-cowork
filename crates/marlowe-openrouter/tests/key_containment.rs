//! **The API key reaches exactly one place: an `Authorization` header on the wire.**
//!
//! # Why this test is shaped the way it is
//!
//! ADR-046 §5 asks for a test that the key cannot reach model context, the journal, or an error
//! string — *asserted where it would leak*. CLAUDE.md's most-logged failure family is a control
//! asserted where it is **declared** rather than where it is **enforced**, and the two examples it
//! gives are exactly the temptations here:
//!
//! * `web_is_inert_and_never_inlines` asserted `inline_threshold_bytes == 0` — the value of a
//!   field, not the fate of a byte — and was green on a build where nothing read the field.
//! * `persona_emission.rs` asserted on a body built inside the test process, and passed while the
//!   deployed daemon served a binary from before the persona commit.
//!
//! The adjacent, worthless version of this file would assert that `ApiKey` has no `Display`, or
//! that `redact()` replaces a substring. Both are true of a build where the driver never calls
//! either. So instead the tests below **drive the real `OpenRouterDriver`** — real request
//! assembly, real header construction, real SSE decoding, real error mapping — with only the
//! socket replaced by [`ScriptedTransport`], and then assert on:
//!
//! 1. what the transport was handed (the body: model context; the headers: the wire),
//! 2. what came back out (the `ModelStep`: the transcript and the journal),
//! 3. what an error path produced (`ProviderError::detail`: the screen and the journal),
//! 4. what the `--dev` sink saw (the diagnostic that prints the outbound request).
//!
//! Each is a place the key would actually appear if the containment were broken.

use marlowe_contract::TrustClass;
use marlowe_loop::{
    Assembler, Block, CallLimits, ContextView, ModelDriver, ModelStep, SessionId, SessionState,
    SourceKind,
};
use marlowe_openrouter::{
    ApiKey, OpenRouterDriver, Reply, ScriptedTransport, REDACTED,
};
use marlowe_tools::{builtin_registry, ExposedSet, ToolId};

/// A key shaped like a real one. **Distinctive**, so a partial leak is visible: a test key of
/// `"k"` would be found inside a hundred innocent words and a leak of it would be invisible.
const KEY: &str = "sk-or-v1-6f4bd2a09c17e3558d4c1a7b0e29f3d6a8b5c4e7f1029384756abcdef0123456";

fn view() -> ContextView {
    let mut state = SessionState::new(SessionId::new(), "Marlowe.");
    state.push(Block::new(
        SourceKind::History,
        "what is in notes.md?".to_string(),
        TrustClass::UserAsserted,
    ));
    Assembler::new(8_192, 1_024).assemble(&state)
}

fn tools() -> ExposedSet {
    ExposedSet::new(vec![ToolId::new("read")]).expect("one tool fits")
}

fn limits() -> CallLimits {
    CallLimits { max_output_tokens: 512, route: marlowe_loop::ModelRoute::Orchestrator }
}

/// A minimal successful SSE stream.
fn ok_stream() -> String {
    "data: {\"id\":\"gen-1\",\"provider\":\"Anthropic\",\"model\":\"anthropic/claude-sonnet-4.5\",\
     \"choices\":[{\"delta\":{\"content\":\"Notes.\"}}]}\n\n\
     data: {\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":2,\"cost\":0.000123}}\n\n\
     data: [DONE]\n\n"
        .to_string()
}

/// **1. The body is model context. The key is not in it, and the header is the only place it is.**
#[test]
fn the_key_is_in_exactly_one_header_and_in_no_part_of_the_model_s_context() {
    let transport = ScriptedTransport::new(vec![Reply::ok(&ok_stream())]);
    let seen = transport.seen();
    let mut driver = OpenRouterDriver::new(
        Box::new(transport),
        ApiKey::from_secret(KEY),
        "anthropic/claude-sonnet-4.5",
        builtin_registry().expect("builtins"),
    );

    driver.call(&view(), &tools(), limits()).expect("the scripted call succeeds");

    let seen = seen.lock().expect("seen");
    let call = seen.first().expect("one call was made");

    // The BODY is what the model reads. Nothing in it may carry the credential — not the
    // messages, not the tool schemas, not a stray field.
    assert!(
        !call.body.contains(KEY),
        "the API key reached the request body, which is the model's context"
    );

    // The HEADERS are the wire. Exactly one carries it, and it is the one that has to.
    let carrying: Vec<&str> = call
        .headers
        .iter()
        .filter(|(_, v)| v.contains(KEY))
        .map(|(k, _)| k.as_str())
        .collect();
    assert_eq!(
        carrying,
        vec!["Authorization"],
        "the key must appear in the Authorization header and nowhere else; it appeared in {carrying:?}"
    );

    // ...and the control: if the header assembly stopped sending the key at all, the test above
    // would still pass with `carrying == []`. It must not.
    assert_eq!(carrying.len(), 1, "the request was sent with NO credential at all");
}

/// **2. The transcript and the journal. What comes back out of the driver.**
#[test]
fn nothing_the_driver_returns_can_carry_the_key() {
    let transport = ScriptedTransport::new(vec![Reply::ok(&ok_stream())]);
    let mut driver = OpenRouterDriver::new(
        Box::new(transport),
        ApiKey::from_secret(KEY),
        "anthropic/claude-sonnet-4.5",
        builtin_registry().expect("builtins"),
    );

    let mut streamed = String::new();
    let call = driver
        .call_streaming(&view(), &tools(), limits(), &mut |d| streamed.push_str(d))
        .expect("the scripted call succeeds");

    // The step is what the loop pushes into history and what the recorder journals.
    match &call.step {
        ModelStep::Say(text) => assert!(!text.contains(KEY), "the key reached the reply"),
        other => panic!("expected prose, got {other:?}"),
    }
    assert!(!streamed.contains(KEY), "the key reached a streamed delta");
    assert!(!format!("{:?}", call.step).contains(KEY), "the key reached the step's Debug");

    // The attribution record is serialised into the run record, which reaches the journal.
    let record = serde_json::to_string(driver.attribution()).expect("attribution serialises");
    assert!(!record.contains(KEY), "the key reached the run record: {record}");
    // ...and the control: the record must actually contain something, or this proves nothing.
    assert!(record.contains("Anthropic"), "the run record is empty: {record}");
}

/// **3. The error path — and the one the type system cannot reach on its own.**
///
/// An upstream is free to quote the credential it was sent back inside its own error body. Several
/// APIs do. That body travels into `ProviderError::detail`, onto the user's screen, and into the
/// journal — so the redaction is asserted against a server that does exactly that.
#[test]
fn a_key_an_upstream_echoes_back_is_redacted_before_it_becomes_an_error() {
    let echoed = format!(
        "{{\"error\":{{\"code\":401,\"message\":\"No auth credentials found for key {KEY}\"}}}}"
    );
    let transport = ScriptedTransport::new(vec![Reply::status(401, &echoed)]);
    let mut driver = OpenRouterDriver::new(
        Box::new(transport),
        ApiKey::from_secret(KEY),
        "anthropic/claude-sonnet-4.5",
        builtin_registry().expect("builtins"),
    );

    let err = driver.call(&view(), &tools(), limits()).expect_err("a 401 must fail the call");

    assert!(!err.detail.contains(KEY), "the key reached the error string: {}", err.detail);
    assert!(
        err.detail.contains(REDACTED),
        "the key was echoed and should have been REDACTED rather than merely absent — if it is \
         absent because the body was dropped, the user loses the upstream's message: {}",
        err.detail
    );
    // The remedy survives the redaction. A redacted error that says nothing is a crash with
    // better manners.
    assert!(
        err.detail.contains(marlowe_openrouter::KEY_VAR),
        "a 401 must still name what to fix: {}",
        err.detail
    );
    assert!(!err.retriable, "a bad credential will be bad on the next attempt too");
}

/// The same property for the retry path, which builds its message from a different branch.
///
/// **A second error constructor is exactly how a redaction gets skipped**, so the branch that
/// formats a rate-limit failure is asserted separately rather than assumed to share the first
/// one's protection.
#[test]
fn the_rate_limited_error_path_redacts_too() {
    let echoed = format!("{{\"error\":{{\"message\":\"quota for key {KEY} exhausted\"}}}}");
    let transport = ScriptedTransport::new(vec![])
        .then_forever(Reply { status: 429, retry_after: Some("0".into()), body: echoed });
    let mut driver = OpenRouterDriver::new(
        Box::new(transport),
        ApiKey::from_secret(KEY),
        "anthropic/claude-sonnet-4.5",
        builtin_registry().expect("builtins"),
    );

    let err = driver.call(&view(), &tools(), limits()).expect_err("exhausted retries fail");
    assert!(!err.detail.contains(KEY), "the key reached the retry error: {}", err.detail);
    assert!(err.detail.contains(REDACTED), "{}", err.detail);
}

/// **4. `--dev`'s outbound-request dump.** The instrument that prints the request is the obvious
/// place for a credential to appear in a terminal and in a saved log.
#[test]
fn the_dev_request_dump_never_sees_the_key() {
    let transport = ScriptedTransport::new(vec![Reply::ok(&ok_stream())]);
    let dumped = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let sink_target = std::sync::Arc::clone(&dumped);

    let mut driver = OpenRouterDriver::new(
        Box::new(transport),
        ApiKey::from_secret(KEY),
        "anthropic/claude-sonnet-4.5",
        builtin_registry().expect("builtins"),
    )
    .with_request_dump(Box::new(move |body| {
        sink_target.lock().expect("dump").push(body.to_string());
    }));

    driver.call(&view(), &tools(), limits()).expect("the scripted call succeeds");

    let dumped = dumped.lock().expect("dump");
    assert_eq!(dumped.len(), 1, "the sink must have fired, or this test asserts nothing");
    assert!(!dumped[0].contains(KEY), "the key reached the --dev dump: {}", dumped[0]);
    // Control: the dump is a real request, not an empty object.
    assert!(dumped[0].contains("claude-sonnet-4.5"), "{}", dumped[0]);
}

/// The negative control for this whole file.
///
/// **Every assertion above is of the form "X does not contain the key".** That family passes
/// trivially against a driver that sends nothing, returns nothing and errors on everything — so
/// one test asserts that the key IS present where it belongs. Without this, deleting the
/// `Authorization` header would make the file greener.
#[test]
fn the_credential_is_actually_sent_or_none_of_the_above_means_anything() {
    let transport = ScriptedTransport::new(vec![Reply::ok(&ok_stream())]);
    let seen = transport.seen();
    let mut driver = OpenRouterDriver::new(
        Box::new(transport),
        ApiKey::from_secret(KEY),
        "anthropic/claude-sonnet-4.5",
        builtin_registry().expect("builtins"),
    );
    driver.call(&view(), &tools(), limits()).expect("the scripted call succeeds");

    let seen = seen.lock().expect("seen");
    let auth = seen[0]
        .headers
        .iter()
        .find(|(k, _)| k == "Authorization")
        .map(|(_, v)| v.clone())
        .expect("an Authorization header was sent");
    assert_eq!(auth, format!("Bearer {KEY}"), "the credential must reach the wire intact");
}
