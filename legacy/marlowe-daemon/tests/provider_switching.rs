//! **`/provider` — switching the provider from the session, and the model list following it.**
//!
//! ADR-046 made the provider a launch-time choice and it stayed one everywhere: `--tui` accepted
//! `--provider` and discarded it, the Windows Terminal profile ignored it, and changing your mind
//! meant a restart. `STATE.md` has carried it as outstanding since.
//!
//! # What is asserted, and where
//!
//! The headline property is **the model list is a consequence of the provider**, so that is
//! asserted on `Daemon::status()` — the function the client actually re-projects — rather than on
//! the field that feeds it. A test on `config.openrouter_models` would be green on a build where
//! `status()` read something else, which is `web`'s `inline_threshold_bytes: 0` again.
//!
//! Every case is paired with the control that fails when the mechanism is absent: the *other*
//! provider's list is asserted in the same test, so "the list changed" cannot pass by both being
//! empty, both being the same, or nothing having run.

use marlowe_daemon::{Daemon, DaemonConfig, ModelProviderChoice};
use std::sync::atomic::{AtomicU32, Ordering};

static SEQ: AtomicU32 = AtomicU32::new(0);

fn daemon(name: &str) -> Daemon {
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir()
        .join(format!("marlowe-provider-{name}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let workspace = root.join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let config = DaemonConfig::new(root.join("profile"), workspace);
    Daemon::open(config).expect("the daemon opens")
}

// ─────────────────────────────────────────────────────────────────────────────────────────

/// **The property the feature exists for: the list follows the provider.**
///
/// Set up by hand rather than by calling `set_provider("openrouter")`, which needs a key and a
/// network round trip. What is under test is the *consequence* — that `status()` reads the hosted
/// catalogue when the provider is hosted — and that is reachable without either.
#[test]
fn switching_the_provider_replaces_what_the_model_picker_is_built_from() {
    let mut d = daemon("list-follows");

    // **A slug nothing could have pulled**, so its presence is decided by the branch under test
    // and by nothing else. An earlier version of this asserted the local list held no `/` — which
    // is false of Ollama, whose HuggingFace tags look like `hf.co/unsloth/Qwen3.8-27B-GGUF:...`.
    // The heuristic was wrong about the world; this asserts the property directly.
    const ONLY_HOSTED: &str = "test-vendor/model-no-machine-has-pulled";

    let local = d.status();
    assert_eq!(local.model_provider, "ollama");
    assert!(
        !local.models.iter().any(|m| m == ONLY_HOSTED),
        "the local list must not hold hosted catalogue entries: {:?}",
        local.models
    );

    d.config_mut().openrouter_models =
        vec!["anthropic/claude-sonnet-4.5".to_string(), ONLY_HOSTED.to_string()];
    d.config_mut().model_provider =
        ModelProviderChoice::OpenRouter { model: "anthropic/claude-sonnet-4.5".into() };

    let hosted = d.status();
    assert_eq!(hosted.model_provider, "openrouter");
    assert!(
        hosted.models.iter().any(|m| m == ONLY_HOSTED),
        "the hosted list must be the catalogue: {:?}",
        hosted.models
    );

    // The control, and it is the half that makes this more than "a list exists": the two lists
    // must actually differ. Both being the machine's Ollama inventory would satisfy everything
    // above on a build where the branch was never taken.
    assert_ne!(
        local.models, hosted.models,
        "the list did not change with the provider, which is the whole feature"
    );
}

/// The configured slug is selectable even when the catalogue is empty — a picker that omits the
/// value it is currently reporting is internally inconsistent, which is the same rule the Ollama
/// branch already follows for the configured model.
#[test]
fn a_hosted_daemon_with_no_catalogue_still_offers_the_model_it_is_using() {
    let mut d = daemon("no-catalogue");
    d.config_mut().model_provider =
        ModelProviderChoice::OpenRouter { model: "vendor/only-one".into() };

    let s = d.status();
    assert_eq!(s.models, vec!["vendor/only-one".to_string()]);
    assert_eq!(s.model, "vendor/only-one");
}

/// A slug the catalogue does not list is refused **by name**, rather than accepted and failing as
/// a 404 on the next turn — which would read as a provider fault instead of a bad choice.
#[test]
fn a_model_that_is_not_in_the_hosted_catalogue_is_refused_by_name() {
    let mut d = daemon("unknown-slug");
    d.config_mut().openrouter_models = vec!["anthropic/claude-sonnet-4.5".to_string()];
    d.config_mut().model_provider =
        ModelProviderChoice::OpenRouter { model: "anthropic/claude-sonnet-4.5".into() };

    let err = d.set_model("vendor/typo").expect_err("a slug nobody serves must be refused");
    assert!(err.contains("vendor/typo"), "the refusal must name what was asked for: {err}");

    // The control: a slug that IS in the catalogue is accepted. Without this the assertion above
    // would pass on a build that refused every hosted model, which was the behaviour before this
    // change and is what the change exists to end.
    d.set_model("anthropic/claude-sonnet-4.5").expect("a listed model is selectable");
}

/// **One definition of the provider set.** The picker is built from `project::PROVIDERS` and
/// `set_provider` validates against it, so an option a user can see is an option the daemon
/// accepts. Two lists would be the second-source shape: one gains an entry, the other refuses it.
#[test]
fn every_provider_the_picker_offers_is_one_the_daemon_accepts() {
    for p in marlowe_daemon::PROVIDERS {
        let mut d = daemon("offered");
    // **A model that is not in Ollama's store, so the engine cannot start and this test cannot
    // spawn a 6.7 GB server.** It was spawning one: `set_provider` starts the engine, and on this
    // machine that meant a real `llama-server` on the card for ~2 s per test. CLAUDE.md's sixth
    // parallel-session hazard is a build stealing CPU from a timed measurement; a test suite
    // taking the GPU is the same hazard with a different resource, and it would be invisible in
    // the measurement it corrupted. The subject here is the NAME being accepted, not the engine.
        d.config_mut().model = "definitely-not-a-model:0b".to_string();
        let outcome = d.set_provider(p);
        // `openrouter` may legitimately fail here for want of a key or a network — what must not
        // happen is it failing because the NAME was not recognised, which is the mismatch this
        // guards.
        if let Err(e) = outcome {
            assert!(
                !e.contains("is not a provider this build has"),
                "`{p}` is offered by the picker and rejected by name: {e}"
            );
            // **The second error string, and forbidding only the first is why this test passed
            // while lying.** `set_provider`'s catch-all arm answers *"is listed as a provider and
            // has no implementation"* — a DIFFERENT sentence for the same defect, so a `PROVIDERS`
            // entry with no arm sailed past the assertion above. Both are the mismatch this
            // guards; neither is a legitimate failure for an offered name.
            assert!(
                !e.contains("has no implementation"),
                "`{p}` is offered by the picker and has no arm in `set_provider`: {e}"
            );
        }
    }

    // The control: a name that is NOT in the set is rejected exactly that way, so the assertion
    // above is discriminating rather than vacuously true of every string.
    let mut d = daemon("not-offered");
    let e = d.set_provider("anthropic").expect_err("an unknown provider must be refused");
    assert!(e.contains("is not a provider this build has"), "{e}");
    // **Derived from `PROVIDERS`, not spelled out, and that is a fix rather than tidying.** This
    // read `e.contains("ollama") && e.contains("openrouter") && e.contains("ollama/llama.cpp")` —
    // a second hand-maintained copy of the set, in an assertion, which went stale the moment the
    // set changed and failed for a reason that had nothing to do with the property. The refusal is
    // built by `PROVIDERS.join(", ")`; reading the same array is what makes this a test of the
    // message rather than of somebody's memory of it.
    for offered in marlowe_daemon::PROVIDERS {
        assert!(e.contains(offered), "the refusal must list `{offered}`: {e}");
    }
    // **And the shelved hybrid must NOT be listed.** `ollama/llama.cpp` is a provider this build
    // still has code for and deliberately does not offer, so naming it in the remedy would send a
    // user to a switch that is refused. Asserted rather than assumed: the list and the validation
    // are the same array, so this is what keeps the array honest in both directions.
    assert!(
        !e.contains(marlowe_view::provider::HYBRID),
        "the refusal offered the shelved hybrid as a remedy: {e}"
    );
}

/// Switching to a provider that is already active is a no-op rather than a re-fetch, so `/provider
/// ollama` on a local daemon does not cost a network round trip to openrouter.ai.
#[test]
fn selecting_the_provider_already_in_use_changes_nothing() {
    let mut d = daemon("noop");
    let before = d.status().model_provider;
    d.set_provider("ollama").expect("already there");
    assert_eq!(d.status().model_provider, before);
}

/// **The picker reports what the daemon said, not a guess.**
///
/// `Picker::new(PROVIDERS, 0)` with a hardcoded selection would be green on a hosted daemon while
/// showing `ollama` — the field-value-versus-fate shape this project keeps logging. Asserted here
/// rather than in `marlowe-surface`, which cannot see this crate on purpose: a surface that can
/// build a view can hold state the daemon does not have (`c2d_boundary.rs`).
#[test]
fn the_provider_picker_is_built_from_the_daemons_own_report() {
    let report = |provider: &str| marlowe_daemon::StatusReport {
        version: "t".into(),
        workspace: "/ws".into(),
        model: "m".into(),
        model_disclosure: "d".into(),
        degraded: None,
        rerank_provider: "cpu".into(),
        model_provider: provider.into(),
        live_runs: 0,
        models: vec!["m".into()],
        // Nothing has been announced into this fixture and nothing has been up.
        announcements: Vec::new(),
        uptime_ms: 0,
    };

    let local = marlowe_daemon::view_from_status(&report("ollama"));
    let p = local.picker(marlowe_view::ControlId::Provider);
    assert_eq!(
        p.options,
        marlowe_daemon::PROVIDERS.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        "the picker is built from `project::PROVIDERS`, so this list changing means that list did"
    );
    assert_eq!(p.options[p.selected], marlowe_view::provider::OLLAMA);

    // The control, and it is the one that matters: a different report must select differently.
    // Without it, a hardcoded `selected: 0` passes everything above.
    let hosted = marlowe_daemon::view_from_status(&report("openrouter"));
    let p = hosted.picker(marlowe_view::ControlId::Provider);
    assert_eq!(
        p.options[p.selected], "openrouter",
        "the picker showed a provider the daemon did not report"
    );

    // ── THE SHELVED HYBRID, AND THE GAP IT LEFT ──────────────────────────────────────────
    //
    // **This clause is inverted, and the inversion exposes a defect rather than tidying one away.**
    //
    // It used to assert that a daemon reporting `ollama/llama.cpp` selected `ollama/llama.cpp` --
    // the guard against `name()` and `PROVIDERS` drifting apart, because `position()` falls back to
    // `unwrap_or(0)` and a mismatch renders that daemon as plain `ollama` with nothing failing.
    //
    // The shelving removed the entry from `PROVIDERS` and **did not** remove `--provider llamacpp`
    // from `marlowe::resolve_provider`. So that state is still reachable in the product, the name
    // is now one `PROVIDERS` does not hold, and `unwrap_or(0)` does exactly what the old comment
    // warned about: **a daemon whose engine is llama.cpp renders in the picker as `ollama`.**
    //
    // It is asserted here as a CHARACTERISATION -- the current behaviour, pinned, with the defect
    // named -- rather than deleted. Deleting it would leave the drift unguarded; asserting the old
    // expectation would fail on a decision that was deliberately taken. If the CLI door is closed
    // later, or the picker is taught to show an unrecognised report, this test fails and points
    // here.
    let shelved = marlowe_daemon::view_from_status(&report(marlowe_view::provider::HYBRID));
    let p = shelved.picker(marlowe_view::ControlId::Provider);
    assert!(
        !p.options.iter().any(|o| o == marlowe_view::provider::HYBRID),
        "the shelving must reach the picker: {:?}",
        p.options
    );
    assert_eq!(
        p.options[p.selected],
        marlowe_view::provider::OLLAMA,
        "KNOWN GAP, not an expectation: `--provider llamacpp` still builds a hybrid daemon at the \
         CLI, and the picker has no entry for what it reports, so `position().unwrap_or(0)` shows \
         `ollama` while llama.cpp is the engine. If this assertion starts failing, someone fixed \
         it -- update this test rather than restoring the fallback."
    );
}
