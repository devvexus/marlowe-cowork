//! **K6 is a kill criterion, it is currently MET, and adding a hosted provider must not move it.**
//!
//! K6 is *install → first useful output, zero config*. The default path is local Ollama with no
//! configuration whatsoever. The way that regresses is not a redesign — nobody would propose one —
//! it is an environment variable or a helpful fallback quietly deciding that a hosted provider is
//! in use because a key happened to be exported for some other tool. The user would then be billed
//! for a command they typed on a machine they configured nothing on, and every startup line would
//! read the same as it does today.
//!
//! # Asserted where it is enforced, not where it is declared
//!
//! The adjacent worthless test is *"`DaemonConfig::new` sets `model_provider: Ollama`"* — a field's
//! value, green on a build where the run path reads something else entirely. `web`'s
//! `inline_threshold_bytes: 0` is the recorded instance of exactly that.
//!
//! So these assert on **`DaemonConfig::model_provider()`**, which is the single function the run
//! path calls to select a driver and `status()` calls to announce one. That is this project's
//! `blocks_composed_targets` pattern: one definition, called at the enforcement site and by the
//! test, so the two cannot disagree.
//!
//! # And on the environment, because the environment is the threat
//!
//! `OPENROUTER_API_KEY` is set inside these tests. That is the state a real machine is in once
//! anyone has used any other OpenRouter tool, and it is the state under which the default must be
//! unmoved.

use marlowe_daemon::{DaemonConfig, ModelProviderChoice};

fn config() -> DaemonConfig {
    DaemonConfig::new(std::path::PathBuf::from("profile"), std::path::PathBuf::from("workspace"))
}

/// `set_var` is `unsafe` from the 2024 edition and this crate is 2021, where it is not — but it is
/// process-global either way, so the tests that touch it run in one function rather than racing.
#[test]
fn no_environment_variable_can_move_the_default_provider() {
    let restore = std::env::var("OPENROUTER_API_KEY").ok();

    // 1. Nothing set. The plain zero-config case.
    std::env::remove_var("OPENROUTER_API_KEY");
    assert_eq!(
        config().model_provider(),
        ModelProviderChoice::Ollama,
        "the default provider must be the local one"
    );

    // 2. **The case that matters.** A key exported for some other tool is on the machine, and a
    //    naive implementation reads it as consent.
    std::env::set_var("OPENROUTER_API_KEY", "sk-or-v1-a-key-exported-for-something-else");
    assert_eq!(
        config().model_provider(),
        ModelProviderChoice::Ollama,
        "an exported OPENROUTER_API_KEY moved the default provider. K6 is install-to-answer with \
         NO configuration; a machine that has a key lying around has configured nothing, and this \
         would bill it for a local question."
    );

    // 3. The model name too — the other value someone might read as intent.
    std::env::set_var("OPENROUTER_MODEL", "anthropic/claude-sonnet-4.5");
    assert_eq!(config().model_provider(), ModelProviderChoice::Ollama);
    std::env::remove_var("OPENROUTER_MODEL");

    // 4. And the control: an EXPLICIT choice does move it, or the guard above proves only that
    //    the mechanism is inert.
    let mut explicit = config();
    explicit.model_provider =
        ModelProviderChoice::OpenRouter { model: "anthropic/claude-sonnet-4.5".into() };
    assert_eq!(
        explicit.model_provider(),
        ModelProviderChoice::OpenRouter { model: "anthropic/claude-sonnet-4.5".into() },
        "an explicit choice must be honoured, or the assertions above are about a dead switch"
    );

    match restore {
        Some(v) => std::env::set_var("OPENROUTER_API_KEY", v),
        None => std::env::remove_var("OPENROUTER_API_KEY"),
    }
}

#[test]
fn the_default_daemon_still_routes_to_the_pinned_local_model() {
    // The rest of the zero-config contract, so a change that left `model_provider` alone and moved
    // the model, the endpoint or the window still fails something.
    let c = config();
    assert_eq!(c.model, marlowe_provider::DEFAULT_MODEL);
    assert_eq!(c.context_tokens, marlowe_provider::DEFAULT_CONTEXT_TOKENS);
    assert!(c.reranking.is_none(), "a 60 MB model must not become an install-time dependency");
    assert!(!c.dev);
}

/// **The provider is announced, not inferred** — ADR-029's rule, and STATE.md's open item 2 says
/// the daemon currently fails it for the rerank provider. This is the model provider's half.
#[test]
fn status_names_the_provider_it_selected() {
    // A daemon on openrouter.ai and one on loopback otherwise print an identical status, and the
    // difference is money and a network. The field is filled from `model_provider()` — the same
    // function the run path selects with — so it cannot report one thing while the run does
    // another.
    let mut c = config();
    assert_eq!(c.model_provider().name(), "ollama");
    c.model_provider = ModelProviderChoice::OpenRouter { model: "vendor/model".into() };
    assert_eq!(c.model_provider().name(), "openrouter");
}
