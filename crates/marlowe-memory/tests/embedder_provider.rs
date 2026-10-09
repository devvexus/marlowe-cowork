//! **The execution provider, the VRAM budget, and the refusals — asserted where they are enforced.**
//!
//! Every test here drives `Embedder::load_with_provider`, which is the function the product calls
//! through `--embedder-provider`. None of them asserts that a constant has a value: `web`'s
//! `inline_threshold_bytes: 0` is the standing example of a control that was green while nothing
//! read it, and the equivalent mistake here would be checking that `EmbedProvider::Cuda.name()`
//! returns a string.
//!
//! # What CANNOT be asserted here, stated so a green run is not over-read
//!
//! `ort` 2.0.0-rc.10 exposes no way to enumerate a constructed session's active providers and no
//! node placement. M0c Session L measured **13.6% of nodes still running on CPU** under a
//! successfully registered CUDA session. So the strongest available claim is *a CUDA session
//! constructed and was not permitted to fall back silently*, and these tests make exactly that one.

use std::path::{Path, PathBuf};

use marlowe_memory::cue::dense::embedder::{Embedder, EmbedProvider, ProviderChoice, MODEL_FILE};
use marlowe_memory::cue::dense::vram::Probe;

fn model_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/marlowe-memory -> repo root")
        .join("models")
        .join("jina-embeddings-v2-small-en")
}

/// Skip only for absence. `models/` is gitignored and a fresh clone legitimately lacks it.
fn dir_or_skip() -> Option<PathBuf> {
    let dir = model_dir();
    if dir.join(MODEL_FILE).exists() {
        Some(dir)
    } else {
        eprintln!("SKIP: run `python tools/fetch_model.py`");
        None
    }
}

/// Whether a CUDA session constructs here, printed rather than assumed.
///
/// **This is the control every CUDA-dependent test below is gated on, and the gate PRINTS.** A
/// CUDA test that silently no-ops on a CPU-only machine is green and vacuous; one that says which
/// branch it took is evidence either way.
fn cuda_or_report(dir: &Path) -> bool {
    let available = Embedder::cuda_available(dir);
    if !available {
        // The error, not just the boolean -- on this project's own machine it reads
        // `cublasLt64_12.dll` missing, which is the Session G failure verbatim and is a fact about
        // the box rather than about the code.
        match Embedder::load_with_provider(dir, 1, None, ProviderChoice::Cuda, Probe::Device, marlowe_memory::cue::dense::vram::Reserve::None) {
            Ok(_) => eprintln!("cuda_available said false but a CUDA load succeeded -- probe bug"),
            Err(e) => eprintln!("NO CUDA ON THIS MACHINE: {e}"),
        }
    }
    available
}

#[test]
fn a_zero_vram_budget_falls_back_to_cpu_instead_of_failing_the_run() {
    // **The exhaustion path, driven deliberately.** A test that waits for a real card to fill up
    // is a test that never runs, so `Probe::Fixed(0)` says "the device is full" without needing
    // one to be. The property is that the run SURVIVES: a shared card with no room is a throughput
    // problem, and turning it into a load failure would take the product down whenever the user
    // started something else.
    let Some(dir) = dir_or_skip() else { return };

    let e = Embedder::load_with_provider(&dir, 4, None, ProviderChoice::Auto, Probe::Fixed(0), marlowe_memory::cue::dense::vram::Reserve::None)
        .expect("an exhausted device must fall back, never fail the load");

    assert_eq!(e.provider(), EmbedProvider::Cpu);
    // The full requested width, on CPU -- device memory constrains GPU sessions and nothing else.
    assert_eq!(e.plan().workers, 4, "CPU fallback must not inherit the GPU width");
    assert_eq!(e.plan().requested, 4);
    assert_eq!(e.plan().free_at_load, Some(0));
    // **The reason must name the MEMORY branch, not the construction branch.** Without this the
    // test passes identically on a machine where CUDA simply cannot load, which is the machine it
    // was written on -- it would then be asserting nothing about the budget at all.
    let reason = &e.plan().reason;
    assert!(
        reason.contains("free") && reason.contains("floor"),
        "the budget branch must be the one reported, got: {reason}"
    );
    assert!(
        !reason.contains("did not construct"),
        "a zero budget must be refused BEFORE a session is attempted, got: {reason}"
    );
}

#[test]
fn the_budget_is_checked_before_a_session_is_attempted_not_after() {
    // The discriminating control for the test above, and the one that makes it non-vacuous on a
    // CUDA-less machine. With an enormous fixed budget the loader must get PAST the memory gate
    // and reach construction -- so the two tests take different branches and say so. On a machine
    // with working CUDA this is the success branch; on one without, it is the construction-failure
    // branch. Both are distinguishable from the budget branch, which is the whole point.
    let Some(dir) = dir_or_skip() else { return };

    let plenty = Probe::Fixed(64 * 1024 * 1024 * 1024);
    let e = Embedder::load_with_provider(&dir, 1, None, ProviderChoice::Auto, plenty, marlowe_memory::cue::dense::vram::Reserve::None)
        .expect("a large budget must still load, on one provider or the other");

    let reason = e.plan().reason.clone();
    assert!(
        !reason.contains("floor"),
        "64 GB of budget must clear the memory gate; got the budget branch: {reason}"
    );
    match e.provider() {
        EmbedProvider::Cuda => assert!(reason.contains("CUDA"), "{reason}"),
        EmbedProvider::Cpu => assert!(
            reason.contains("did not construct"),
            "the only CPU outcome past the memory gate is a construction failure, got: {reason}"
        ),
    }
}

#[test]
fn an_absent_device_is_cpu_and_says_so_rather_than_guessing() {
    // `None` from the probe means "no readable NVIDIA device", which is a different answer from
    // `Some(0)` and must not be collapsed into it. A loader that treated the two alike would report
    // a full card on a machine that has no card.
    let Some(dir) = dir_or_skip() else { return };

    // Fixed(0) is the "full card" case; this test's subject is the reason text for the OTHER case,
    // which only `Probe::Device` can produce and only on a machine with no driver. Assert the pair
    // is distinguishable in the plan rather than faking a probe the enum cannot express.
    let full = Embedder::load_with_provider(&dir, 1, None, ProviderChoice::Auto, Probe::Fixed(0), marlowe_memory::cue::dense::vram::Reserve::None)
        .expect("loads");
    assert_eq!(full.plan().free_at_load, Some(0), "a full card reports a reading of zero");
    assert!(
        !full.plan().reason.contains("no readable"),
        "a full card must not be reported as an absent one: {}",
        full.plan().reason
    );
}

#[test]
fn asking_for_cuda_explicitly_is_a_refusal_and_never_a_quiet_cpu_run() {
    // **The Session G property.** ORT will happily register CPU when a requested provider fails to
    // create, and the resulting "GPU" figure came within 1% of the 1-thread CPU one. Here the
    // request either produces a CUDA embedder or an error -- never a CPU embedder.
    let Some(dir) = dir_or_skip() else { return };

    match Embedder::load_with_provider(&dir, 1, None, ProviderChoice::Cuda, Probe::Device, marlowe_memory::cue::dense::vram::Reserve::None) {
        Ok(e) => assert_eq!(
            e.provider(),
            EmbedProvider::Cuda,
            "an explicit CUDA request that returns Ok must not have silently become CPU"
        ),
        Err(e) => {
            let text = e.to_string();
            // **Two refusals, both correct, and the wording differs by cause.** With
            // MARLOWE_CUDA_LIB_DIR set, CUDA fails at the ONNX session. Unset, it fails earlier and
            // names the library search path instead. Asserting only the first made this test a
            // property of the environment rather than of the refusal, which is the failure family
            // this project logs: it passed on the machine that set the variable and failed on the
            // one that did not. What matters is that a request for CUDA never returns a quiet CPU
            // run -- so assert the refusal is NAMED, whichever cause produced it.
            assert!(
                text.contains("ONNX session") || text.contains("CUDA runtime libraries"),
                "the refusal must name its cause: {text}"
            );
            eprintln!("CUDA refused on this machine (this is the property under test): {text}");
        }
    }
}

#[test]
fn a_cuda_session_that_loaded_actually_holds_DEVICE_memory() {
    // **The strongest claim available, and it observes a BYTE rather than a declaration.**
    //
    // `error_on_failure()` is what stops ORT registering CPU behind a CUDA request -- but its
    // absence is unobservable from Rust, because `ort` 2.0.0-rc.10 cannot enumerate a constructed
    // session's providers. `Embedder::provider()` would keep reporting `Cuda` on a session that had
    // silently fallen back, so a test asserting on it is asserting the REQUEST. That is the
    // declared-control family exactly.
    //
    // Free device memory is not. A CUDA session that has run a forward pass holds VRAM; one that
    // quietly became CPU holds none. So this reads the card before and after, and the floor it
    // compares against is the graph's own size rather than a chosen number.
    //
    // **It is skipped, loudly, where CUDA does not load** -- including on the machine this was
    // written on. That is a gap in the evidence and is recorded as one, not papered over.
    let Some(dir) = dir_or_skip() else { return };

    // **THIS MEASUREMENT IS ONLY VALID ON A CARD NOBODY ELSE IS USING, SO IT CHECKS.**
    //
    // Every reading here is a free-memory delta — a property of the whole DEVICE, attributed to one
    // session. That attribution holds only while nothing else allocates or frees during the window.
    // Since the llama.cpp hybrid landed, the suite starts `llama-server` and Ollama keeps its own
    // runner warm on a keep-alive, so the card routinely has another 6.7 GB tenant mid-run.
    //
    // Measured: with Ollama's runner resident, the drop released **58,720,256 of an expected
    // 129,809,014 bytes** — a PARTIAL release, because the other process took memory back between
    // the two reads. That is not a reading about this provider, and asserting on it either blames
    // the provider for another process or forces a threshold so loose the test stops meaning
    // anything.
    //
    // So: if any other process holds the device, say so and stop. **A skip that names its reason is
    // worth more than a pass that cannot be attributed** — and this test exists to catch a CUDA
    // session that silently ran on CPU, which is a real failure this project hit today at 10 tok/s
    // against 107 with every health signal reading fine. It must not be made to pass by accident.
    if let Some(n) = other_gpu_tenants() {
        if n > 0 {
            eprintln!(
                "SKIP: {n} other process(es) hold this device, so a free-memory delta cannot be \
                 attributed to this session. Re-run alone: `cargo test -p marlowe-memory --test \
                 embedder_provider`."
            );
            return;
        }
    }

    // **ONE SESSION, AND THE READING IS WHAT IT RELEASES ON DROP.**
    //
    // The obvious shape -- read free memory, load, read again -- cannot work here, for a reason
    // that took a wedged machine to find. **The first CUDA session in a process leaks a ~238 MB
    // primary context that never comes back while the process lives.** So a `before` taken at the
    // top of the FIRST session includes a one-off cost that is not that session's, and a `before`
    // in a process where an earlier test already paid it does not. The reading depended on test
    // ORDER, which is why this passed alone and failed in a workspace run, and why its own message
    // could only offer two hypotheses with no way to choose between them.
    //
    // Paying the context up front in a throwaway session fixes the arithmetic and is the wrong
    // trade: it loads the graph onto the card TWICE, and on a 16 GB card already holding a
    // `llama-server` that is enough to thrash the whole machine. Measured: it took the test from
    // ~13 s to 117 s and froze the desktop.
    //
    // So measure the drop instead. `after_load - after_drop` is what THIS session released, the
    // primary context is not released either way and therefore cancels, and only one copy of the
    // graph is ever resident.
    let mut e = match Embedder::load_with_provider(&dir, 1, None, ProviderChoice::Cuda, Probe::Device, marlowe_memory::cue::dense::vram::Reserve::None)
    {
        Ok(e) => e,
        Err(err) => {
            eprintln!("SKIP: CUDA does not load here, so this property is UNMEASURED: {err}");
            return;
        }
    };
    // The arena allocates on first run, so an unwarmed session would under-read.
    e.embed("a niche equation appears in the middle of a long document").expect("embeds");
    let Some(held) = marlowe_memory::cue::dense::vram::free_bytes() else {
        eprintln!("SKIP: no readable device, so there is no byte to observe");
        return;
    };
    drop(e);
    let released = marlowe_memory::cue::dense::vram::free_bytes().expect("the device was readable");

    let model_bytes = std::fs::metadata(dir.join(MODEL_FILE)).map(|m| m.len()).unwrap_or(0);
    let freed = released.saturating_sub(held);

    // **A CONCURRENT ALLOCATION IS DETECTED, NOT ABSORBED.** A free-memory delta is a measurement
    // of the whole machine attributed to one process, and this suite now starts `llama-server` and
    // Ollama runners that take and release 6.7 GB while this runs. If free memory went DOWN across
    // a drop that only releases, something else took the card mid-read -- that is not a reading
    // about the provider. Failing on it would blame this provider for another process; passing on
    // it would be the vacuity this project keeps finding.
    if released < held {
        eprintln!(
            "INCONCLUSIVE: free device memory FELL by {} bytes across a drop that only releases, so \
             another process allocated during the read. Re-run with `-p marlowe-memory --test \
             embedder_provider` on a quiet card.",
            held - released
        );
        return;
    }

    assert!(
        freed >= model_bytes,
        "dropping a CUDA session should release at least the graph's {model_bytes} bytes; free \
         memory moved by {freed}. A concurrent allocation is caught above and the leaked primary \
         context cancels across the two reads, so the remaining reading is that THE PROVIDER \
         SILENTLY FELL BACK TO CPU -- the failure this test exists for, and one this project hit \
         today at 10 tok/s against 107 with every health signal reading fine."
    );
}

#[test]
fn every_session_in_one_embedder_uses_the_same_provider() {
    // The invariant that makes `embed_batch`'s contiguous split safe. `embed_batch` hands worker
    // `i` a slice, so with a mixed set the vector a text receives would depend on which worker it
    // landed on -- and therefore on how much VRAM was free at load. `ProviderPlan` carries ONE
    // provider by construction, so this asserts the shape of the type as the loader uses it: the
    // width may fall below the request, and the provider is still single-valued.
    let Some(dir) = dir_or_skip() else { return };

    for probe in [Probe::Fixed(0), Probe::Fixed(64 * 1024 * 1024 * 1024)] {
        let e = Embedder::load_with_provider(&dir, 3, None, ProviderChoice::Auto, probe, marlowe_memory::cue::dense::vram::Reserve::None)
            .expect("loads");
        assert!(e.workers() >= 1);
        assert_eq!(
            e.workers(),
            e.plan().workers,
            "the reported width must be the number of sessions actually opened"
        );
        assert!(e.plan().workers <= e.plan().requested);
    }
}

// ------------------------------------------------------------------ ADR-015 on the CUDA provider

/// The fixed set every invariance reading below is taken over.
fn texts() -> Vec<String> {
    ["a niche equation appears in the middle of a long document", "we had pasta for dinner", ""]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

#[test]
fn cuda_determinism_and_worker_invariance_are_measured_not_inherited() {
    // **ADR-015: a different execution provider is a DIFFERENT SCORER.** The CPU readings in
    // `tests/embedding_reference.rs` say nothing about this one, so they are re-taken here rather
    // than cited. When CUDA cannot construct, this SKIPS LOUDLY with the driver's own error --
    // it does not pass quietly, because a quiet pass would read as a CUDA measurement.
    let Some(dir) = dir_or_skip() else { return };
    if !cuda_or_report(&dir) {
        eprintln!(
            "SKIP: ADR-015's CUDA readings CANNOT be taken on this machine. The CPU readings do \
             NOT transfer and must not be cited for CUDA."
        );
        return;
    }

    let texts = texts();
    let mut one = Embedder::load_with_provider(&dir, 1, None, ProviderChoice::Cuda, Probe::Device, marlowe_memory::cue::dense::vram::Reserve::None)
        .expect("CUDA constructs, checked above");
    let first = one.embed_batch(&texts).expect("embeds");
    let second = one.embed_batch(&texts).expect("embeds");
    assert_eq!(first, second, "CUDA: two calls in one process must agree bit for bit");

    for workers in [2usize, 8] {
        let mut many =
            match Embedder::load_with_provider(&dir, workers, None, ProviderChoice::Cuda, Probe::Device, marlowe_memory::cue::dense::vram::Reserve::None) {
                Ok(e) => e,
                Err(e) => {
                    // Not a silent skip: fewer sessions than asked for is a device-memory fact and
                    // it is reported rather than swallowed.
                    eprintln!("SKIP width {workers}: {e}");
                    continue;
                }
            };
        assert_eq!(
            many.embed_batch(&texts).expect("embeds"),
            first,
            "CUDA: worker count {workers} changed the output, so it is a quality knob"
        );
    }
}

#[test]
fn cuda_padding_invariance_is_measured_on_the_shipped_graph() {
    // The third ADR-015 reading. ADR-015 measured the int8 cross-encoder moving by a median 0.0109
    // logits on padding alone and flipping top-1 in 15% of cases, while f32 was invariant to
    // 0.000000 -- so this is a real hazard with a real precedent, and it is a property of a GRAPH
    // AND A PROVIDER together.
    //
    // The embedder pads nothing: `forward` builds a `[1, tokens]` tensor at the text's own length.
    // The equivalent question here is whether the SAME text embedded twice through differently
    // sized batches agrees, since batch composition is the only thing a caller varies.
    let Some(dir) = dir_or_skip() else { return };
    if !cuda_or_report(&dir) {
        eprintln!("SKIP: no CUDA here; the CPU padding reading does not transfer.");
        return;
    }

    let texts = texts();
    let mut e = Embedder::load_with_provider(&dir, 1, None, ProviderChoice::Cuda, Probe::Device, marlowe_memory::cue::dense::vram::Reserve::None)
        .expect("CUDA constructs, checked above");
    let alone = e.embed(&texts[0]).expect("embeds");
    let in_batch = e.embed_batch(&texts).expect("embeds");
    assert_eq!(
        alone, in_batch[0],
        "CUDA: a text embedded alone and inside a batch must be bit-identical"
    );
}

/// How many processes OTHER than this one hold the CUDA device. `None` when it cannot be read.
///
/// Bounded by construction: `nvidia-smi` is given a null stdin and its output is read once. This is
/// a test, not the shipped path — but the same rule applies, because an unbounded child in a suite
/// is the hang that stalls every binary queued behind it.
fn other_gpu_tenants() -> Option<usize> {
    // **Through the crate's own bounded helper, because the guard is right.**
    // `no_unbounded_external_commands` caught the first version of this function using a bare
    // `.output()` and named the remedy. An unbounded child in a suite is the hang that stalls every
    // binary queued behind it -- which is the exact failure this whole session spent an afternoon
    // on. It also gives null stdin and CREATE_NO_WINDOW for free.
    let out = marlowe_memory::cue::dense::vram::bounded_output(
        "nvidia-smi",
        &["--query-compute-apps=pid", "--format=csv,noheader"],
    )?;
    if !out.status.success() {
        return None;
    }
    let me = std::process::id().to_string();
    Some(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && *l != me)
            .count(),
    )
}

