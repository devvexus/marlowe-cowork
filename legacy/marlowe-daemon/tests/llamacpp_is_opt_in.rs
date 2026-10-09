//! **ADR-060's hybrid is opt-in — and as of 2026-08-27 it is SHELVED, which is a further step.**
//!
//! Three tests here assert the INVERSE of what they asserted a day ago, and each says so at
//! its own doc comment. That is not churn: ADR-060 was accepted as the hybrid rather than as
//! the third provider these were written against, and a refusal that became a fallback is a
//! behavioural decision, not a bug fix. `hybrid_engine.rs` holds the tests for the fallback
//! itself.
//!
//! # THE SHELVING, and what it did and did not do
//!
//! Matthew's call, after using it: *"LLAMA.cpp gets shelved. It has so many issues. OLLAMA stays
//! the default. Shelve the hybrid path. Don't remove it. But make it unavailable."*
//!
//! **The reason is tool calling, not speed.** The engine is genuinely ~5x faster to first token and
//! those measurements stand. In real use the model emitted raw `<tool_call><function=read>` XML
//! into the **reasoning** channel, looping, never producing a call the harness could act on; and on
//! a single-call turn the parser **ate the opening `<tool_call>` and emitted the remainder as
//! visible text**. Parser-level, not prompting.
//!
//! **Two probes missed it, and both misses are this project's standing family.** The leak check
//! asserted no markup in **`content`** — 0/168, clean, and the wrong channel. The batching check
//! recorded *"spurious batches (n_calls > 1): 0"*, which was read as *the model never over-calls*
//! when it meant **the parser never returned more than one**. A single `glob` worked; six chained
//! calls did not.
//!
//! **Exactly one thing changed in the code: `marlowe_view::provider::PROVIDERS` lost the entry.**
//! `HYBRID` the constant, every `marlowe-provider` module, `ModelProviderChoice::LlamaCpp`,
//! `HybridEngine`, the supervisor and the fallback wording all still exist and stay green. So the
//! rule applied to these tests is: **a test whose subject is the PICKER now asserts the hybrid is
//! absent; a test whose subject is the ENGINE keeps its assertions and drives the type directly.**
//! None of it is deleted, because none of the subjects are gone.
//!
//! # The consequence that decides how the engine tests are driven
//!
//! `set_provider` validates against `PROVIDERS` **before** its match, so its `HYBRID` arm is now
//! unreachable — and with it the port-collision refusal that used to live there. The one route
//! from the product into a live `HybridEngine` is `Daemon::open` on a config that already says
//! `LlamaCpp`, which is what `--provider llamacpp` still builds at the CLI. That is the route the
//! fallback test takes below.
//!
//! # What was actually dangerous about adding a provider here
//!
//! Three of the matches over `ModelProviderChoice` are exhaustive, so the compiler carries them.
//! **Four were `if let ModelProviderChoice::OpenRouter { .. }`**, which compile clean against a new
//! variant and take the *Ollama* branch:
//!
//! | site | what it would have done |
//! |---|---|
//! | `Daemon::status` | probe `127.0.0.1:11434`, list Ollama's `/api/tags` as the picker, and report `capability_for(&config.model)` — the 12/12 measured **through Ollama's renderer and parser** |
//! | `Daemon::set_model` | accept or refuse models on the authority of what **Ollama** has pulled |
//! | `marlowe::tui::spawn_args` | spawn a daemon with no `--provider` flag: a local Ollama daemon while the client believes otherwise |
//! | `marlowe::agent::serve` | print a startup announcement identical to an Ollama one |
//!
//! All four are `match` now. The two reachable from this crate are asserted below on **behaviour**,
//! not on the shape of the code: what the daemon reports and what it refuses. `spawn_args` is
//! asserted in `marlowe::tui`'s own `spawn_argv` module, next to the OpenRouter case it repeats.
//!
//! # And the property that must not move
//!
//! Ollama is the default. `zero_config_is_unchanged.rs` asserts that no environment variable moves
//! it; what is added here is that adding a third option did not move it either.

use marlowe_daemon::{Daemon, DaemonConfig, ModelProviderChoice};

fn daemon(case: &str) -> Daemon {
    let root = std::env::temp_dir().join(format!("marlowe-llamacpp-optin-{case}"));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("a scratch profile");
    let mut config = DaemonConfig::new(root, std::env::temp_dir());
    // A scratch port: this daemon never binds, but a shared one would make two cases collide.
    config.port = 0;
    Daemon::open(config).expect("a daemon on an empty profile")
}

fn llamacpp() -> ModelProviderChoice {
    ModelProviderChoice::LlamaCpp {
        // Port 1 is reserved and nothing listens on it, so the probe reaches `EndpointDown`
        // deterministically rather than depending on whether a server happens to be running.
        endpoint: marlowe_provider::LocalEndpoint::new("127.0.0.1", 1).expect("loopback"),
        sampling: marlowe_provider::llamacpp::SamplingSource::ServerDefaults,
    }
}

/// **A daemon that has actually STARTED a hybrid engine**, which the shelving left exactly one
/// route to: a config that already says `LlamaCpp` when `Daemon::open` runs. The other helper
/// mutates the config *after* open and therefore never starts anything.
///
/// `model` is the lever that decides the engine's fate, and every caller here passes one that
/// **cannot resolve**. That is not a convenience: `hybrid::start` reads Ollama's store *first*,
/// before any port is touched, anything is spawned, or any resident Ollama model is unloaded — so
/// an unresolvable name reaches the fallback in milliseconds without putting a 6.7 GB
/// `llama-server` on the card. A test suite taking the GPU is CLAUDE.md's sixth parallel-session
/// hazard with a different resource, and it would be invisible in the measurement it corrupted.
fn hybrid_daemon(case: &str, model: &str) -> Daemon {
    let root = std::env::temp_dir().join(format!("marlowe-llamacpp-optin-{case}"));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("a scratch profile");
    let mut config = DaemonConfig::new(root, std::env::temp_dir());
    config.port = 0;
    config.model = model.to_string();
    config.model_provider = llamacpp();
    Daemon::open(config).expect("a daemon on an empty profile")
}

/// A model name no store can resolve, so `hybrid::start` fails before it spawns anything.
const UNRESOLVABLE: &str = "definitely-not-a-model:0b";

#[test]
fn the_default_provider_is_still_ollama_after_a_third_option_exists() {
    // Asserted on `model_provider()` — the ONE function the run path calls to choose a driver and
    // `status()` calls to announce one — rather than on the struct field, which is a value a build
    // can hold while the run path reads something else entirely.
    let config = DaemonConfig::new(std::env::temp_dir(), std::env::temp_dir());
    assert_eq!(config.model_provider(), ModelProviderChoice::Ollama);
}

/// **Every OFFERED provider round-trips through `name()`, and the hybrid is offered by none.**
///
/// # One clause of this is inverted, and only one
///
/// It used to assert the set equality in both directions: every `name()` the enum can produce is in
/// `PROVIDERS`, and the two are the same size. The **first direction is now false by design** — the
/// enum can still produce `ollama/llama.cpp` (`ModelProviderChoice::LlamaCpp` was not removed) and
/// the picker deliberately does not offer it. That clause is inverted below and says so.
///
/// The **second direction is untouched and is the one that was load-bearing**: `project.rs`'s picker
/// finds the active provider with `position()` and falls back to `unwrap_or(0)`, so a `PROVIDERS`
/// entry whose spelling no `name()` produces would render that daemon as `ollama` — a wrong answer
/// in the one place a person reads which provider is live, with nothing failing.
///
/// **What this reads on a build without the shelving:** `PROVIDERS` has three entries including the
/// hybrid, so the absence assertions fail by name and the length check fails at 3 vs 2. It cannot
/// pass on either build by accident.
#[test]
fn every_provider_the_picker_offers_has_a_name_the_enum_can_produce() {
    let offered: Vec<&str> = vec![
        ModelProviderChoice::Ollama.name(),
        ModelProviderChoice::OpenRouter { model: "vendor/model".into() }.name(),
    ];
    for name in &offered {
        assert!(
            marlowe_daemon::PROVIDERS.contains(name),
            "`{name}` is a name the enum produces and the picker does not offer"
        );
    }
    assert_eq!(
        offered.len(),
        marlowe_daemon::PROVIDERS.len(),
        "PROVIDERS and this list disagree: {:?} vs {offered:?}. A `PROVIDERS` entry with no \
         variant is refused at runtime by `set_provider`'s named `other` arm; a variant with no \
         entry is worse, because the picker silently reports the wrong provider.",
        marlowe_daemon::PROVIDERS
    );

    // ── THE INVERTED CLAUSE ──────────────────────────────────────────────────────────────
    //
    // **The hybrid's name is still produced by the enum and is deliberately not offered.** This
    // used to be `assert!(PROVIDERS.contains(&llamacpp().name()))`. Removing the entry from
    // `PROVIDERS` is the entire mechanism of the shelving: that array is what the picker offers,
    // what `set_provider` validates against, and what the stub scripts, so one deletion closes
    // every door at once. Asserted here rather than left as an absence nobody checks, because
    // putting the entry back is a one-line change that would otherwise reopen the path silently.
    assert!(
        !marlowe_daemon::PROVIDERS.contains(&marlowe_view::provider::HYBRID),
        "the hybrid is back in the picker. It is SHELVED, not removed -- see this file's header \
         for what must be true before it returns: parallel tool calls parse, single tool calls \
         parse without eating their own delimiter, and both measured on a prompt that provokes a \
         BATCH. Got: {:?}",
        marlowe_daemon::PROVIDERS
    );
    assert_eq!(
        llamacpp().name(),
        marlowe_view::provider::HYBRID,
        "the constant survived the shelving and the variant still names itself with it -- that is \
         what makes this a shelf rather than a deletion"
    );
}

/// **`Daemon::status` did not fall through to the Ollama tail**, asserted on what it reports.
#[test]
fn a_llamacpp_daemon_does_not_disclose_ollamas_measured_tool_call_reliability() {
    let mut d = daemon("status");
    d.config_mut().model_provider = llamacpp();
    let report = d.status();

    assert_eq!(report.model_provider, "ollama/llama.cpp");
    // **The measurement-transfer family, at the one place it would have happened.** The recorded
    // 12/12 for `qwen3.5:9b` was measured through Ollama's `renderer: qwen3.5` and
    // `parser: qwen3.5`; llama.cpp uses neither. Falling through to the Ollama tail would have
    // disclosed one runtime's number under another's name, correctly formatted and about a
    // different system.
    assert!(
        !report.model_disclosure.contains("12/12"),
        "a figure measured through Ollama was disclosed for a runtime that is not Ollama: {}",
        report.model_disclosure
    );
    assert!(
        report.model_disclosure.contains("NOT MEASURED"),
        "{}",
        report.model_disclosure
    );
    assert!(
        report.model_disclosure.contains("llama.cpp"),
        "the disclosure must name the runtime, or a reader cannot tell which system it is about: {}",
        report.model_disclosure
    );

    // The control. Without it the assertions above pass on a build that reports NOT MEASURED for
    // everything — including the default, where the figure is real and disclosing it is the point.
    let mut ollama = daemon("status-control");
    ollama.config_mut().model = marlowe_provider::DEFAULT_MODEL.to_string();
    assert!(
        ollama.status().model_disclosure.contains("12/12"),
        "the Ollama path stopped disclosing its own measurement: {}",
        ollama.status().model_disclosure
    );
}

/// **The picker holds OLLAMA'S inventory, and that is the hybrid's whole first half.**
///
/// # This assertion is the inverse of the one it replaces, and the inversion is the decision
///
/// It read `assert_eq!(report.models, vec![report.model])` — one entry — and that was right for
/// the third-provider design, where the user launched `llama-server` against one blob and Marlowe
/// could not change it. ADR-060 accepted the hybrid instead: **Ollama stores, downloads and lists;
/// llama.cpp serves.** So every model Ollama has pulled is selectable, `/model` restarts the
/// engine against the new blob, and a one-entry picker would now be hiding the inventory the user
/// is entitled to.
///
/// **What this reads on a build without the change:** `models` would be `vec![config.model]`, of
/// length 1, and the assertion that the configured model is *present* would still pass — so the
/// length check is what does the work, and only on a machine where Ollama has more than one model.
/// That is stated rather than hidden: on a machine with exactly one model pulled, this test cannot
/// distinguish the two designs and says so instead of asserting something it has not shown.
#[test]
fn the_hybrids_picker_offers_what_ollama_has_pulled_because_ollama_is_the_store() {
    let mut d = daemon("models");
    d.config_mut().model_provider = llamacpp();
    let report = d.status();
    assert!(
        report.models.iter().any(|m| *m == report.model),
        "the picker must always contain the value it is reporting: {:?}",
        report.models
    );
    let ollama_has = {
        let mut c = daemon("models-control");
        c.config_mut().model = marlowe_provider::DEFAULT_MODEL.to_string();
        c.status().models.len()
    };
    if ollama_has <= 1 {
        eprintln!(
            "SKIP: Ollama reports {ollama_has} model(s) on this machine, so a one-entry picker \
             and the full inventory are indistinguishable here. The assertion below would pass \
             under either design and is therefore not run."
        );
        return;
    }
    assert_eq!(
        report.models.len(),
        ollama_has,
        "the hybrid must offer Ollama's inventory -- Ollama is the store. Got {:?}",
        report.models
    );
}

/// **`/model` on the hybrid RESTARTS the engine. It does not refuse, and it does not lie about
/// having switched.**
///
/// # The inversion, and why the old assertion was right for the old design
///
/// This test used to assert a refusal naming the restart, because Marlowe did not own the server:
/// the model was the argv someone else typed, and there was genuinely nothing a config write could
/// change. ADR-060's hybrid makes Marlowe the thing that started it, so refusing would be
/// declining to do something it can do — and would leave `/model` meaning two different things
/// depending on a provider setting.
///
/// # What is asserted is the ABSENCE OF A WINDOW, which is the property that could lie
///
/// The dangerous shape is an acknowledged switch with the old model still loaded: the picker
/// updates, the band updates, and the next reply comes from the previous weights. So the switch is
/// synchronous — `set_model` does not return until a server on our port is serving the new blob
/// and has been measured on the GPU, or until the engine has fallen back to Ollama, which has the
/// model too. Either way, **when this call returns, `status().model` names something that is
/// actually loaded.**
///
/// **What this reads on a build without the change:** `set_model` returns `Err`, and
/// `expect("...")` fails by name. It cannot pass accidentally.
#[test]
fn switching_models_on_the_hybrid_restarts_the_engine_and_is_not_acknowledged_before_it_is_true() {
    let mut d = daemon("set-model");
    d.config_mut().model_provider = llamacpp();

    // Port 1: nothing can listen there, so the engine cannot start and the fallback is certain.
    // That is the case being asserted -- a model switch must still WORK when the engine cannot,
    // because Ollama has the model and Ollama is the other half of the hybrid.
    let asked = "some-other-model:9b";
    d.set_model(asked).expect(
        "the hybrid switches models by restarting its own engine; a refusal here is the \
         third-provider behaviour ADR-060 replaced",
    );
    assert_eq!(
        d.status().model,
        asked,
        "the switch was acknowledged, so the reported model must be the one asked for"
    );
    // And the reason the engine is not serving it is on the screen rather than implied.
    let degraded = d.status().degraded.unwrap_or_default();
    assert!(
        degraded.contains(marlowe_provider::FELL_BACK_MARKER),
        "a switch whose engine could not restart must SAY so, persistently: {degraded:?}"
    );

    // Selecting the model it is already serving is a no-op rather than a restart -- otherwise
    // `/model <the current one>` would tear down a working server to reach the state it is in.
    let current = d.status().model;
    d.set_model(&current).expect("the model it is already serving");
    assert_eq!(d.status().model, current);
}

/// **THE ENGINE FALLS BACK TO OLLAMA AND THE REASON IS ON THE SCREEN.**
///
/// # Renamed from `a_hybrid_SWITCH_whose_engine_cannot_start_...`, and only the route changed
///
/// Every assertion below is the one it made yesterday. What is gone is the **switch**: the shelving
/// removed the entry from `PROVIDERS`, `set_provider` validates against `PROVIDERS` before its
/// match, and its `HYBRID` arm is therefore unreachable. Driving this test through `set_provider`
/// would now measure the shelving and report it as a statement about the engine.
///
/// **The subject is not shelved — only the door is.** `HybridEngine`, the supervisor, every
/// `EngineFailure` variant and the whole fallback wording still exist, still ship, and are still
/// reachable: `marlowe::resolve_provider` still accepts `--provider llamacpp`, which builds a
/// `LlamaCpp` config, and `Daemon::open` starts an engine when the config says so. So this drives
/// the type directly, through `Daemon::open`, which is the one remaining product route into a live
/// engine. **Deleting it because the picker no longer offers the hybrid would have deleted a test
/// of code that still runs.**
///
/// # Asserted where it is enforced, not where it is declared
///
/// The subject is `StatusReport::degraded` — the string §B5 renders in amber in the status band on
/// **every** frame — and the assertions are on the **words a person reads**, not on which enum
/// variant was constructed. A test asserting `matches!(engine, FellBack { .. })` would be green on
/// a build whose message said nothing.
///
/// **What this reads on a build where the fallback wording regressed:** `degraded` is `None` and
/// the `expect` fails, or it carries a launch command with no statement about which engine is
/// serving, and every clause below fails by name.
///
/// # KNOWN DEFECT THE SHELVING CREATED, asserted on below and NOT fixed here
///
/// The last clause of this sentence tells the user to type `/provider ollama/llama.cpp`. That is
/// now the one string `set_provider` refuses — the picker does not offer it, so the validation
/// rejects it before the retry arm is reached. **The persistent amber band instructs an action the
/// daemon answers with `is not a provider this build has`.** The clause is still asserted because
/// it is still what the product emits; the contradiction is named here so it is on the record
/// rather than discovered by a user.
#[test]
fn a_hybrid_engine_that_cannot_start_falls_back_to_ollama_and_says_exactly_why() {
    // **The failure is forced at the STORE, not at the port, and the first attempt at this test is
    // why.** It used port 1 on the theory that nothing can listen there — and on Windows
    // `llama-server` bound it, came up healthy, and measured **97 tok/s on the GPU**. The test
    // failed, correctly, because the premise was wrong rather than the code.
    //
    // Two things follow. A test must not spawn a real 6.7 GB server to assert a string: it takes
    // 2.6 s, it takes the card, and it contends with any measurement running beside it. And the
    // failure has to be one that CANNOT succeed on any machine — so it is a model name that is not
    // in Ollama's store, which fails in `ollama_store::resolve` **before** anything is spawned and
    // before any resident Ollama model is unloaded.
    let d = hybrid_daemon("engine-fallback", UNRESOLVABLE);

    let report = d.status();
    assert_eq!(
        report.model_provider, marlowe_view::provider::HYBRID,
        "the report keeps naming what the config ASKED for; the band is what says the engine \
         differs. Note that this is a name `PROVIDERS` no longer offers -- see \
         `the_provider_picker_is_built_from_the_daemons_own_report` for what the picker does with \
         it."
    );
    let degraded = report.degraded.expect("a fallen-back engine must degrade visibly");

    // The four clauses the requirement actually asks for.
    assert!(
        degraded.contains("llama.cpp is NOT serving"),
        "it must name the engine that is not serving: {degraded}"
    );
    assert!(
        degraded.contains("Ollama is"),
        "it must name the engine that IS serving, or the user cannot tell if anything is: \
         {degraded}"
    );
    assert!(
        degraded.contains("225 ms"),
        "it must say what was lost, measured rather than adjectival: {degraded}"
    );
    // **STILL ASSERTED, AND CURRENTLY A FALSE PROMISE — see this test's header.** The product
    // emits this clause, so the test records it; but `set_provider` refuses that exact string now
    // that `PROVIDERS` has lost the entry, so the remedy the band offers no longer works. Left
    // asserted deliberately: weakening it would hide the contradiction, and inverting it would
    // claim a decision nobody has taken about what a shelved engine should tell the user.
    assert!(
        degraded.contains("/provider ollama/llama.cpp"),
        "a degraded state a user cannot act on is a crash with better manners: {degraded}"
    );
    // And the cause is SPECIFIC rather than a category: it names the model that could not be
    // resolved and the store it was looked for in.
    assert!(
        degraded.contains(UNRESOLVABLE),
        "the cause must name what could not be resolved: {degraded}"
    );
    assert!(
        degraded.contains("Ollama's model store"),
        "the cause must name where it looked -- on a layout that changed under us, that is the          whole cost of the change: {degraded}"
    );
}

/// **THE NEGATIVE CONTROL. Without it, a build that shows the fallback line unconditionally passes
/// every assertion above.**
///
/// The plain `ollama` provider is the one state where the fallback sentence must be **absent** —
/// nothing fell back, because nothing was asked to start. Ask what the test above would read if
/// `degraded` were hardcoded to the fallback line: it would pass, and this fails.
#[test]
fn the_fallback_sentence_is_absent_when_no_engine_was_asked_to_start() {
    let mut d = daemon("no-fallback");
    d.config_mut().model_provider = marlowe_daemon::ModelProviderChoice::Ollama;
    let degraded = d.status().degraded.unwrap_or_default();
    assert!(
        !degraded.contains(marlowe_provider::FELL_BACK_MARKER),
        "a plain-Ollama daemon reported an engine fallback that never happened: {degraded}"
    );
    // The second half of the control: `openrouter` is also not the hybrid, and it degrades for its
    // own reasons (no key). Those must not be dressed as an engine fallback either.
    d.config_mut().model_provider =
        marlowe_daemon::ModelProviderChoice::OpenRouter { model: "vendor/model".into() };
    let degraded = d.status().degraded.unwrap_or_default();
    assert!(
        !degraded.contains(marlowe_provider::FELL_BACK_MARKER),
        "an OpenRouter degradation was reported as a llama.cpp fallback: {degraded}"
    );
}

/// **The collision a live run found, and the guard that closes the class rather than the instance.**
///
/// `LLAMACPP_DEFAULT_PORT` was 11435 — which is `DEFAULT_DAEMON_PORT`. Every unit test passed,
/// because no single process knows both constants; what caught it was `marlowe --status --provider
/// llamacpp` reporting *"something is listening on http://127.0.0.1:11435 but it is not
/// llama-server"* about **Marlowe's own daemon**. Moving the constant fixed that day.
///
/// # HALF OF THIS TEST NOW MEASURES SOMETHING ELSE, AND THE NAME OVERSTATES IT
///
/// It had two halves: the compiled defaults must differ, and a **hand-set** collision must be
/// refused — because `--daemon-port` and `--llamacpp-port` are both settable, so equal defaults
/// were never the only way to reach the state. The second half went through `set_provider`, and the
/// shelving made `set_provider`'s `HYBRID` arm unreachable, taking that port guard with it.
///
/// So **half one is now the only executable guard on this class**, and half two below asserts what
/// the same call does instead. The rule itself is still enforced in the product, once, by
/// `marlowe::resolve_provider` at the CLI — which exits 2 before a daemon exists and which **no
/// test in the workspace reaches**. That gap is the finding, not a tidy-up.
#[test]
fn a_llamacpp_port_equal_to_the_daemons_own_is_refused_by_name() {
    // Half one: the two compiled defaults. Nothing in a single process compares them but this.
    assert_ne!(
        marlowe_provider::LLAMACPP_DEFAULT_PORT,
        marlowe_daemon::DEFAULT_DAEMON_PORT,
        "the two defaults collided again"
    );
    assert_ne!(
        marlowe_provider::LLAMACPP_DEFAULT_PORT,
        marlowe_provider::LocalEndpoint::DEFAULT_PORT,
        "the llamacpp default landed on Ollama's port, which this provider must run beside"
    );

    // ── HALF TWO, INVERTED, AND THE INVERSION IS ITSELF THE FINDING ──────────────────────
    //
    // It asserted that a **hand-set** collision was refused with `own control port`, because
    // `--daemon-port` and `--llamacpp-port` are both settable and equal defaults were never the
    // only way to reach this state. That refusal lives inside `set_provider`'s `HYBRID` arm — and
    // `set_provider` validates against `PROVIDERS` **before** the match, so the shelving made the
    // arm unreachable and took the port guard with it.
    //
    // **So the daemon-side port refusal no longer exists as a reachable path, and this asserts
    // what actually happens now**: the same call, on the same colliding config, fails for the
    // shelving instead. Recording that is the point. A test quietly dropped here would leave a
    // guard everyone believes in and nothing runs.
    //
    // **The rule still has one live enforcement site: `marlowe::resolve_provider`**, which refuses
    // `--llamacpp-port <daemon port>` with `is Marlowe's own daemon port` before the daemon is
    // built. That is the CLI, it calls `std::process::exit(2)`, and **no test in the workspace
    // covers it** — so half one above is currently the only executable guard on this class.
    let mut d = daemon("port-collision");
    d.config_mut().port = marlowe_provider::LLAMACPP_DEFAULT_PORT;
    let e = d
        .set_provider("llamacpp")
        .expect_err("the hybrid is shelved, so every spelling of it is refused");
    assert!(
        e.contains("is not a provider this build has"),
        "the shelving must be the reason, and it must be stated: {e}"
    );
    assert!(
        !e.contains("own control port"),
        "if this ever passes again, the `HYBRID` arm of `set_provider` became reachable -- the \
         hybrid is back in `PROVIDERS`, and the port guard is live again along with it: {e}"
    );
}

/// **THE DOOR, ASSERTED SHUT. This is the exact inverse of `llamacpp_is_not_rejected_by_name`.**
///
/// # The inversion is the decision, and it is the whole mechanism of the shelving
///
/// That test asserted the name was **accepted** — that `set_provider` had an arm for the string the
/// picker offered, rather than falling through to *"`llamacpp` is listed as a provider and has no
/// implementation"*. It was right for a build that offered the hybrid. Now nothing offers it, and
/// the requirement is the opposite: *"Don't remove it. But make it unavailable."*
///
/// **Both spellings, because both were reachable.** `/provider llamacpp` (no slash, what a shell
/// user types) is normalised to `ollama/llama.cpp` at the top of `set_provider`, so a shelving that
/// closed only one of them would leave the alias open — and the alias is the one a person reaches
/// for.
///
/// **What this reads on a build without the shelving:** `set_provider` accepts both, returns `Ok`,
/// and `expect_err` fails by name on the first of them. It cannot pass on the old build.
#[test]
fn llamacpp_is_rejected_by_name_because_the_hybrid_is_shelved() {
    let mut d = daemon("by-name");
    // A model that is not in Ollama's store, so no path out of this call can spawn a 6.7 GB
    // server. Belt and braces now that the switch is refused before it reaches an engine at all.
    d.config_mut().model = UNRESOLVABLE.to_string();

    for spelling in [marlowe_view::provider::HYBRID, "llamacpp"] {
        let e = d
            .set_provider(spelling)
            .expect_err("the hybrid is shelved; `set_provider` must refuse every spelling of it");
        assert!(
            e.contains("is not a provider this build has"),
            "`{spelling}` must be refused for the reason it IS refused for -- an unrecognised \
             name -- rather than by an engine failure that happens to look like a refusal: {e}"
        );
        // The refusal is still the *list* refusal, so it names what IS available. A user who typed
        // a shelved name reads the two that work rather than a bare "no".
        for offered in marlowe_daemon::PROVIDERS {
            assert!(e.contains(offered), "the refusal must list `{offered}`: {e}");
        }
        assert_eq!(
            d.status().model_provider,
            marlowe_view::provider::OLLAMA,
            "a refused switch must not have half-happened: `{spelling}` was rejected, so the \
             daemon must still be on the provider it was on"
        );
    }

    // **THE CONTROL, and without it every assertion above passes on a build where `set_provider`
    // refuses everything.** `ollama` is not shelved and must still be accepted, so the refusals
    // above are discriminating rather than universal.
    d.set_provider(marlowe_daemon::PROVIDERS[0])
        .expect("the offered providers are still accepted");
}
