//! **ADR-060's hybrid, asserted on what a person reads.**
//!
//! # SHELVED 2026-08-27, and that is why this file did not shrink
//!
//! `marlowe_view::provider::PROVIDERS` lost the `ollama/llama.cpp` entry, so the picker no longer
//! offers it and `set_provider` no longer accepts it. **Nothing else was removed.** `HybridEngine`,
//! `EngineFailure`, the supervisor, `Offload`, `tier1_runtime_for` and the `Reserve` arithmetic all
//! still ship and are all still reachable — `marlowe::resolve_provider` still builds a `LlamaCpp`
//! config from `--provider llamacpp`, and `Daemon::open` starts an engine when the config says so.
//!
//! So five of the six tests below are untouched: their subject is code that runs. Only
//! `the_hybrids_name_is_one_string_and_every_reader_has_the_same_one` changed, in exactly one
//! clause, and it says so at its own doc comment. **A test is deleted when its subject is gone, not
//! when its subject is shelved.**
//!
//! # What each test would read on a build WITHOUT this change
//!
//! Stated per test, because a test that would pass either way is not a test of the change. Of the
//! six here: three do not compile without it (`EngineFailure`, `Offload` and `tier1_runtime_for`
//! are all new), two fail on a specific sentence, and **one would pass either way and says so in
//! its own doc comment** rather than being presented as evidence it is not.
//!
//! # The shape deliberately avoided everywhere in this file
//!
//! `assert!(matches!(engine, HybridEngine::FellBack { .. }))`. That asserts a **variant was
//! constructed**, which is the `inline_threshold_bytes == 0` family: green on a build where the
//! message says nothing, and green if the surface never rendered it. The subject here is the
//! **string** — the one §B5 puts in the status band on every frame — and the classification the
//! projection derives from it.

use marlowe_view::turn::DegradedPath;

/// **One name, two crates that declare it, and the readers that must agree on the spelling —
/// which now includes the picker AGREEING NOT TO OFFER IT.**
///
/// `ModelProviderChoice::name()` is what `--status` reports and `hybrid::HYBRID_PROVIDER_NAME` is
/// what the fallback line tells the user to type. `marlowe-provider` sits below `marlowe-view` and
/// cannot import from it, so the two constants are separate declarations — **and this is the seam
/// where they are checked**, which is the alternative to adding a dependency edge purely to make a
/// string reachable. That half of this test has not moved.
///
/// # The `PROVIDERS.contains` clause is inverted, and the inversion is the shelving itself
///
/// It asserted the hybrid was **in** the list the picker is built from. As of 2026-08-27 it must be
/// **absent**: *"Shelve the hybrid path. Don't remove it. But make it unavailable."* The reason is
/// tool calling, not speed — the model emitted raw `<tool_call>` XML into the reasoning channel and
/// the parser ate its own opening delimiter on single-call turns — and the mechanism is exactly
/// this one deletion, because `PROVIDERS` is what the picker offers, what `set_provider` validates
/// against and what the stub scripts.
///
/// **The constants stay and must stay.** `HYBRID` is still printed by `agent.rs`'s startup
/// announcement, by `announce.rs`, by `tui::spawn_args` and by every `EngineFailure::fallback_line`,
/// so a drift between the two crates' spellings is still the live defect it always was. Shelving
/// the path did not shelve the string.
///
/// **What this reads on a build without the shelving:** the absence assertion fails, naming the
/// array it found the entry in. It cannot pass on either build by accident.
#[test]
fn the_hybrids_name_is_one_string_and_every_reader_has_the_same_one() {
    assert_eq!(
        marlowe_provider::HYBRID_PROVIDER_NAME,
        marlowe_view::provider::HYBRID,
        "the provider crate's spelling and the view crate's have drifted; the fallback line would \
         tell the user to type a name that is not the one anything else uses"
    );
    let name = marlowe_daemon::ModelProviderChoice::LlamaCpp {
        endpoint: marlowe_provider::LocalEndpoint::new("127.0.0.1", 1).expect("loopback"),
        sampling: marlowe_provider::llamacpp::SamplingSource::ServerDefaults,
    }
    .name();
    assert_eq!(name, marlowe_view::provider::HYBRID);

    // ── THE INVERTED CLAUSE ──────────────────────────────────────────────────────────────
    assert!(
        !marlowe_view::provider::PROVIDERS.contains(&marlowe_view::provider::HYBRID),
        "the hybrid is back in the list the picker is built from. It is SHELVED, not removed: \
         parallel tool calls must parse, single tool calls must parse without eating their own \
         delimiter, and both must be measured on a prompt that provokes a BATCH before this entry \
         returns. Got: {:?}",
        marlowe_view::provider::PROVIDERS
    );
    assert!(
        !marlowe_view::provider::PROVIDERS.contains(&name),
        "`name()` produces `{name}` and the picker offers it, so the door is open again"
    );
    // The control: the array is not empty and did not lose the providers that ARE offered. Without
    // it both assertions above pass on a build where `PROVIDERS` is `&[]` and nothing is
    // selectable at all.
    assert!(
        marlowe_view::provider::PROVIDERS.contains(&marlowe_view::provider::OLLAMA)
            && marlowe_view::provider::PROVIDERS.contains(&marlowe_view::provider::OPENROUTER),
        "shelving the hybrid must not have taken the live providers with it: {:?}",
        marlowe_view::provider::PROVIDERS
    );

    // **The name says BOTH halves**, and it still matters while it is shelved: every announcement
    // and every fallback line still prints it. A user must be able to see which engine is serving,
    // and one entry called `llamacpp` would hide that Ollama is still the store while one called
    // `ollama` would hide that it is not the engine.
    assert!(
        marlowe_view::provider::HYBRID.contains("ollama")
            && marlowe_view::provider::HYBRID.contains("llama.cpp"),
        "the single entry must name both halves: {}",
        marlowe_view::provider::HYBRID
    );
}

/// **THE PROJECTION MUST NOT CALL THIS A FAILOVER.**
///
/// `DegradedPath::ProviderFailedOver`'s headline is *"failed over · secondary provider"* and its
/// reason is *"run state preserved across the switch"*. Both describe a **hosted secondary taking
/// over from a primary** — a provider the user may not have, possibly costing money, possibly off
/// this machine. Nothing of the sort happened: same model, same weights, same answers, one local
/// process instead of another.
///
/// **Without the change:** `classify_degradation`'s first arm is
/// `r.contains("ollama") || r.contains("model") || r.contains("provider")`, and the fallback line
/// contains all three words — so it classified as `ProviderFailedOver` and the band read *"failed
/// over · secondary provider"*. This test fails on that build, on the `assert_ne!`.
#[test]
fn an_engine_fallback_is_not_projected_as_a_hosted_failover() {
    let failure = marlowe_provider::EngineFailure::ExitedBeforeHealthy {
        code: "exit code 1".into(),
        last_log: "\"cudaMalloc failed: out of memory\"".into(),
    };
    let mut report = report_with_degraded(Some(failure.fallback_line()));
    let view = marlowe_daemon::view_from_status(&report);
    assert_eq!(
        view.status.degraded,
        Some(DegradedPath::EngineFellBackToOllama),
        "the fallback line must reach its own declared path"
    );
    assert_ne!(
        view.status.degraded,
        Some(DegradedPath::ProviderFailedOver),
        "a local engine change was announced as a hosted failover"
    );
    let headline = DegradedPath::EngineFellBackToOllama.headline();
    assert!(
        headline.contains("llama.cpp") && headline.contains("Ollama"),
        "the band's headline must name the engine that is not serving AND the one that is: \
         {headline}"
    );
    assert!(
        !headline.contains("failed over"),
        "the words that make the false claim: {headline}"
    );

    // **THE NEGATIVE CONTROL, and without it every assertion above passes on a build where
    // `classify_degradation` returns `EngineFellBackToOllama` for everything.** An ordinary Ollama
    // outage must still reach the general path, and a stale binary must not be dressed as an
    // engine fallback.
    report.degraded = Some("ollama is not running — start it with `ollama serve`".into());
    assert_eq!(
        marlowe_daemon::view_from_status(&report).status.degraded,
        Some(DegradedPath::ProviderFailedOver),
        "the classifier stopped discriminating: every degradation now reads as an engine fallback"
    );
    report.degraded = Some("the running daemon was built from source that has since changed".into());
    assert_ne!(
        marlowe_daemon::view_from_status(&report).status.degraded,
        Some(DegradedPath::EngineFellBackToOllama),
        "a stale binary was reported as a llama.cpp fallback"
    );
}

/// **Every failure the hybrid can suffer produces a sentence naming that failure specifically.**
///
/// The requirement is *"the reason must be specific — port taken, blob unresolvable, GPU full,
/// server exited"*. A per-variant `cause()` that collapsed to one string would satisfy every
/// structural check and defeat the requirement entirely, so the assertion is that the sentences
/// **differ from each other** and each carries a token only it could carry.
///
/// **Without the change:** `EngineFailure` does not exist and this does not compile.
#[test]
fn each_named_failure_reaches_the_user_as_a_sentence_only_it_could_have_produced() {
    use marlowe_provider::EngineFailure as F;
    let cases: Vec<(F, &str)> = vec![
        (F::BinaryMissing { looked: "C:/x/llama-server.exe".into() }, "llama-server is not on"),
        (
            F::BlobUnresolvable {
                model: "qwen3.5:9b".into(),
                detail: "no manifest under registry.ollama.ai".into(),
            },
            "Ollama's model store",
        ),
        (F::PortUnavailable { port: 11437, detail: "occupied".into() }, "port 11437 is taken"),
        (
            F::ExitedBeforeHealthy {
                code: "exit code 1".into(),
                last_log: "\"cudaMalloc failed: out of memory\"".into(),
            },
            "cudaMalloc failed",
        ),
        (
            F::ExitedMidSession { code: "exit code 3".into(), last_log: "\"bye\"".into() },
            "part-way through the session",
        ),
        (
            F::GpuUnavailable {
                reading: marlowe_provider::Offload::Cpu { tok_per_s: 10.2 },
                detail: "the card is full".into(),
            },
            "GPU COULD NOT BE USED",
        ),
    ];
    let mut lines: Vec<String> = Vec::new();
    for (failure, must_contain) in &cases {
        let line = failure.fallback_line();
        assert!(
            line.contains(must_contain),
            "the cause must be specific: expected `{must_contain}` in `{line}`"
        );
        // Every one of them, without exception, carries the four clauses the requirement asks for.
        assert!(line.starts_with(marlowe_provider::FELL_BACK_MARKER), "{line}");
        assert!(line.contains("Ollama is"), "{line}");
        assert!(line.contains("Answers are unaffected"), "{line}");
        assert!(line.contains("/provider ollama/llama.cpp"), "{line}");
        lines.push(line);
    }
    for (i, a) in lines.iter().enumerate() {
        for (j, b) in lines.iter().enumerate() {
            if i != j {
                assert_ne!(a, b, "two different failures produced the same sentence");
            }
        }
    }
}

/// **The GPU-full case says GPU, and does not read as a generic slowdown.**
///
/// The live defect: a `llama-server` on the CPU answers `/health` 200, reports a complete `/props`
/// with `supports_tools: true`, and **beats Ollama on TTFT** while being five times slower on a
/// whole turn — because TTFT is prompt eval and prompt eval is the part a CPU does acceptably.
/// A user told only *"degraded"* would have no way to reach the cause.
///
/// **Without the change:** `Offload` and `GpuUnavailable` do not exist and this does not compile.
/// With a `GpuUnavailable` whose message said only "the engine is slow", it fails on the first
/// assertion — which is the point: the requirement is the words, not the variant.
#[test]
fn a_cpu_server_is_a_fallback_and_the_reason_names_the_gpu() {
    let line = marlowe_provider::EngineFailure::GpuUnavailable {
        reading: marlowe_provider::Offload::Cpu { tok_per_s: 10.2 },
        detail: "a leftover llama-server is holding the card".into(),
    }
    .fallback_line();
    assert!(line.contains("GPU COULD NOT BE USED"), "{line}");
    assert!(line.contains("10 tok/s"), "the measured rate must survive into the sentence: {line}");
    assert!(
        line.contains("slower than Ollama"),
        "a CPU llama-server is WORSE than the engine it replaced, and keeping it would be wrong \
         twice; the line must say so: {line}"
    );
    assert!(
        !line.to_lowercase().contains("degraded"),
        "`degraded` is the word this whole design exists to replace with a cause: {line}"
    );
}

/// **The VRAM double-count, asserted on the function that decides it.**
///
/// The reserve reads `ollama ps` for residency and `ollama list` for size. A blob served by
/// `llama-server` is in `list` and **not** in `ps`, so ~6.6 GB gets reserved on top of the ~9.5 GB
/// already out of `memory.free`, the card reads smaller than it is, and the embedder resolves to
/// CPU with a reason string that is internally coherent and false in every clause.
///
/// `tier1_runtime_for` is a **function** precisely so this test can read the deciding code rather
/// than a reconstruction of it — the alternative was seven lines inline in `Daemon::open`, where a
/// test could only have asserted on a copy.
///
/// **Without the change:** the function does not exist (it was inline), so this does not compile.
/// The inline version also answered `NotOnThisCard` for a fallen-back hybrid, which is the fourth
/// assertion below and is an *under*-reserve — Ollama is holding the card in that state.
#[test]
fn the_tier1_runtime_follows_the_engine_that_is_actually_serving() {
    use marlowe_daemon::{tier1_runtime_for, HybridEngine, ModelProviderChoice};
    use marlowe_memory::cue::dense::vram::Tier1Runtime as T;

    let hybrid = ModelProviderChoice::LlamaCpp {
        endpoint: marlowe_provider::LocalEndpoint::new("127.0.0.1", 1).expect("loopback"),
        sampling: marlowe_provider::llamacpp::SamplingSource::ServerDefaults,
    };

    assert_eq!(tier1_runtime_for(&ModelProviderChoice::Ollama, &HybridEngine::NotSelected), T::Ollama);
    assert_eq!(
        tier1_runtime_for(
            &ModelProviderChoice::OpenRouter { model: "v/m".into() },
            &HybridEngine::NotSelected
        ),
        T::NotOnThisCard,
        "nothing on this machine runs a hosted model, so nothing is reserved for it"
    );
    // **The fallback row, and it is the one the inline version got wrong.** Ollama is serving, so
    // Ollama holds tier 1 and its weights must be left room for. Answering `NotOnThisCard` here
    // under-reserves, the embedder takes memory belonging to the language model, and Ollama
    // evicts its own model rather than failing — a symptom that appears only in a log this
    // process cannot see.
    assert_eq!(
        tier1_runtime_for(&hybrid, &HybridEngine::FellBack { line: "…".into() }),
        T::Ollama,
        "a fallen-back hybrid is served by Ollama; the reserve must apply"
    );
}

/// **The reserve that closes the double-count, and the control that proves the branch matters.**
///
/// `LlamaServerLoaded` must reserve **zero** — the bytes are already out of `memory.free`. Zero is
/// produced by five branches of `Reserve::read`, so the number alone cannot say which one ran and
/// the assertion is on the **reason**.
///
/// **Without the change** this specific test still passes, and that is stated rather than hidden:
/// `Tier1Runtime::LlamaServerLoaded` predates this session's work. What is new is that
/// `tier1_runtime_for` above can now *reach* it for the right reason, and that `Daemon::open`
/// starts the engine before the embedder loads so the reading is true when it is taken. **This is
/// the guard on the arithmetic; the test above is the guard on the routing.** Both are needed and
/// neither substitutes for the other.
#[test]
fn a_llama_server_holding_the_weights_reserves_nothing_and_names_the_branch() {
    use marlowe_memory::cue::dense::vram::{Reserve, Tier1Runtime};
    let served =
        Reserve::ForTier1 { model: "marlowe-red:9b", runtime: Tier1Runtime::LlamaServerLoaded }
            .read();
    assert_eq!(
        served.bytes, 0,
        "reserving on top of a loaded llama-server double-counts, reads as a smaller card, and \
         pushes the embedder to CPU for a reason that does not exist: {}",
        served.reason
    );
    // `llama-server` hyphenated: `ollama` contains the substring `llama`, so the unhyphenated
    // spelling would match the Ollama branch's own sentences and prove nothing.
    assert!(served.reason.contains("llama-server"), "the branch must be named: {}", served.reason);

    // **The control, and it deliberately does NOT use the Ollama arm.**
    //
    // Its job is to prove the dispatch discriminates — that `read()` is not returning
    // zero-and-a-fixed-string for every input, which would pass both assertions above. ANY second
    // branch does that, and `NotOnThisCard` is a pure early return: zero bytes, a different
    // sentence, no subprocess.
    //
    // **It used to use `Tier1Runtime::Ollama`, and that made this the single worst hang in the
    // workspace suite.** That arm is the only one that shells out — `ollama ps`, then `ollama
    // list` — and Ollama serialises. Alone the test passed in **5.07 s**; under `cargo test
    // --workspace --jobs 4` it sat for **over 60 seconds** while every other binary in the run
    // finished in under 10, and the suite hit its ceiling without completing. `bounded_output`'s
    // 5 s-per-command ceiling was never the cause: queueing behind a dozen other test processes
    // was.
    //
    // The lesson is not "that test was slow". It is that **a control reaching a shared external
    // service is a control whose cost depends on what else is running**, and a parallel suite is
    // exactly where that bites. The property being controlled for needed no external call at all.
    let other =
        Reserve::ForTier1 { model: "marlowe-red:9b", runtime: Tier1Runtime::NotOnThisCard }.read();
    assert!(
        !other.reason.contains("already net of it"),
        "a second runtime took the llama-server branch, so the dispatch is not discriminating: {}",
        other.reason
    );
    assert_ne!(
        other.reason, served.reason,
        "two different runtimes produced the same sentence, so the reason cannot say which branch \
         answered — which is the whole job of the reason, since five branches return zero bytes"
    );
}

// **The `PATH` half of the launch is asserted in
// `marlowe-provider/tests/ollama_store_resolution.rs::the_launch_command_carries_the_blob_the_backend_dll_the_window_and_the_path_prefix`,
// not here.** It belongs next to the resolver that produces the plan, and a second copy in this
// file would be two tests of one property — the shape where one of them gets updated and the other
// keeps passing on the old behaviour.

/// A `StatusReport` with everything but the field under test held constant.
fn report_with_degraded(degraded: Option<String>) -> marlowe_daemon::StatusReport {
    marlowe_daemon::StatusReport {
        version: "0.1.0".into(),
        workspace: "/ws".into(),
        model: "qwen3.5:9b".into(),
        model_disclosure: "unmeasured".into(),
        degraded,
        rerank_provider: "cpu-sequential".into(),
        model_provider: marlowe_view::provider::HYBRID.to_string(),
        live_runs: 0,
        models: vec!["qwen3.5:9b".into()],
        // Nothing has been announced into this fixture and nothing has been up.
        announcements: Vec::new(),
        uptime_ms: 0,
    }
}
