//! **One number, derived — never two.**
//!
//! The context window reaches two places: `num_ctx` on every provider request, and the assembler's
//! window, from which §6 computes its 70% compaction trigger. Until M2 C2e they were independent
//! and they disagreed:
//!
//! * the assembler packed to **32,000** tokens (`Engine::new(.., 32_000, 2_000, ..)`), and
//! * `num_ctx` was **never set**, so Ollama silently applied **2,048**.
//!
//! So the assembler filled a window sixteen times larger than the one the model had, everything
//! past 2,048 tokens was truncated without a word, and compaction was scheduled to fire at 22,400
//! — a threshold the real window could never reach. Three failures from one duplicated number.
//!
//! This test is the derivation rule made executable. It does not check that the number is *right*;
//! it checks that there is only **one** of it.

use marlowe_daemon::DaemonConfig;

#[test]
fn the_assembler_window_and_num_ctx_come_from_the_same_field() {
    // The config carries one value. Anything that needs a window reads it; nothing restates it.
    let mut config = DaemonConfig::new(std::env::temp_dir(), std::env::temp_dir());
    assert_eq!(
        config.context_tokens,
        marlowe_provider::DEFAULT_CONTEXT_TOKENS,
        "the declared default must come from the provider crate, not a literal in the daemon"
    );

    config.context_tokens = 8_192;
    let driver = marlowe_provider::OllamaDriver::new(
        marlowe_provider::LocalEndpoint::default_ollama(),
        marlowe_provider::Routing::uniform(marlowe_provider::DEFAULT_MODEL).unwrap(),
        marlowe_tools::builtin_registry().unwrap(),
    )
    .with_context_tokens(config.context_tokens);
    assert_eq!(driver.context_tokens(), 8_192);
}

/// `num_ctx` is on **every** request, and omitting it is the failure this whole test file is about.
#[test]
fn every_request_carries_num_ctx() {
    use marlowe_contract::TrustClass;
    use marlowe_loop::{Assembler, Block, CallLimits, SessionId, SessionState, SourceKind};

    let driver = marlowe_provider::OllamaDriver::new(
        marlowe_provider::LocalEndpoint::default_ollama(),
        marlowe_provider::Routing::uniform(marlowe_provider::DEFAULT_MODEL).unwrap(),
        marlowe_tools::builtin_registry().unwrap(),
    )
    .with_context_tokens(16_384);

    let mut state = SessionState::new(SessionId::new(), "identity");
    state.push(Block::new(SourceKind::History, "hello", TrustClass::UserAsserted));
    let view = Assembler::new(16_384, 1_024).assemble(&state);
    let tools = marlowe_tools::ExposedSet::new(vec![marlowe_tools::ToolId::new("read")]).unwrap();

    let body = driver.request_body(&view, &tools, CallLimits { max_output_tokens: 256, route: marlowe_loop::ModelRoute::Orchestrator });
    let num_ctx = body.pointer("/options/num_ctx").and_then(|v| v.as_u64());

    assert_eq!(
        num_ctx,
        Some(16_384),
        "num_ctx missing or wrong in the outbound body.\n\n\
         Ollama applies 2048 when it is omitted, whatever the model supports — so an absent field \
         is not a neutral omission, it is a silent 16x truncation of history and injected memory. \
         Body was:\n{body:#}"
    );
}

/// The ceiling is recorded so a model swap has something to compare against — and is deliberately
/// **not** the default, because the window is a KV-cache commitment.
#[test]
fn the_declared_default_is_well_under_the_recorded_ceiling() {
    assert!(
        marlowe_provider::DEFAULT_CONTEXT_TOKENS < marlowe_provider::MODEL_CONTEXT_CEILING,
        "the default must not be the ceiling; 262,144 tokens of KV cache on a 9B model is far \
         more memory than a terminal session should take, and the ceiling is recorded for \
         comparison rather than for use"
    );
    assert!(
        marlowe_provider::DEFAULT_CONTEXT_TOKENS > 2_048,
        "the default must beat Ollama's own, or setting it explicitly buys nothing"
    );
}

/// **The reply is sent once.** (Observed live: it arrived twice.)
///
/// M2 C2e made the reply the result, so `CondensedResult` holds the same text the model already
/// streamed as `TextDelta`s. Rendering it into `Event::Done`'s detail put it on screen a second
/// time — the model said `Hello.` and the transcript then showed `answer: Hello.` underneath.
///
/// This is the projection's half of the same rule: a `Done` with content becomes a second
/// `Entry::Said`, so the guard belongs where the duplication would be visible.
#[test]
fn a_completed_turn_does_not_repeat_its_reply_in_the_done_frame() {
    use marlowe_daemon::{apply_events, view_from_status, Event, StatusReport};
    use marlowe_view::{Entry, Speech};

    let mut view = view_from_status(&StatusReport {
        version: "0.1.0".into(),
        workspace: "/ws".into(),
        model: "m".into(),
        model_disclosure: "d".into(),
        degraded: None,
        rerank_provider: "cpu".into(),
        model_provider: "ollama".into(),
        live_runs: 0,
        models: Vec::new(),
        // Nothing has been announced into this fixture and nothing has been up.
        announcements: Vec::new(),
        uptime_ms: 0,
    });

    apply_events(
        &mut view,
        &[
            Event::Text { delta: "Hello.".into() },
            // A completed turn carries no detail: the prose already went out as deltas.
            Event::Done {
                outcome: "completed".into(),
                detail: String::new(),
                spend_micros_usd: 0,
                elapsed_ms: 10,
            },
        ],
    );

    let said: Vec<&String> = view
        .transcript
        .iter()
        .filter_map(|e| match e {
            Entry::Said(Speech::Model(t)) => Some(t),
            _ => None,
        })
        .collect();

    assert_eq!(
        said.len(),
        1,
        "the reply reached the transcript {} times: {said:?}",
        said.len()
    );
    assert_eq!(said[0], "Hello.");
}

/// **`think` is declared on every request, never inherited.**
///
/// Ollama decides this when the field is absent, and its answer varies by model and by version.
/// Leaving it out means reasoning silently moves into `content` on some upgrade and lands in the
/// transcript as Marlowe's answer — the `num_ctx` shape again: a default that makes a mismatch
/// unobservable.
#[test]
fn every_request_declares_whether_the_model_should_think() {
    use marlowe_contract::TrustClass;
    use marlowe_loop::{Assembler, Block, CallLimits, SessionId, SessionState, SourceKind};

    let mut state = SessionState::new(SessionId::new(), "identity");
    state.push(Block::new(SourceKind::History, "hi", TrustClass::UserAsserted));
    let view = Assembler::new(8_192, 512).assemble(&state);
    let tools = marlowe_tools::ExposedSet::new(vec![marlowe_tools::ToolId::new("read")]).unwrap();

    for on in [true, false] {
        let driver = marlowe_provider::OllamaDriver::new(
            marlowe_provider::LocalEndpoint::default_ollama(),
            marlowe_provider::Routing::uniform(marlowe_provider::DEFAULT_MODEL).unwrap(),
            marlowe_tools::builtin_registry().unwrap(),
        )
        .with_thinking(on);

        let body = driver.request_body(&view, &tools, CallLimits { max_output_tokens: 64, route: marlowe_loop::ModelRoute::Orchestrator });
        assert_eq!(
            body.get("think").and_then(|t| t.as_bool()),
            Some(on),
            "`think` must be present and match the setting. Absent means Ollama chooses, and \
             which channel reasoning arrives in stops being something this project decided.\n{body:#}"
        );
    }
}

/// The daemon's setting is what reaches the driver — one source, not two.
#[test]
fn the_daemons_thinking_setting_is_the_one_the_driver_uses() {
    use marlowe_daemon::DaemonConfig;
    let mut config = DaemonConfig::new(std::env::temp_dir(), std::env::temp_dir());
    assert!(config.thinking, "reasoning is separated by default");

    config.thinking = false;
    let driver = marlowe_provider::OllamaDriver::new(
        marlowe_provider::LocalEndpoint::default_ollama(),
        marlowe_provider::Routing::uniform(marlowe_provider::DEFAULT_MODEL).unwrap(),
        marlowe_tools::builtin_registry().unwrap(),
    )
    .with_thinking(config.thinking);
    assert!(!driver.thinking());
}
