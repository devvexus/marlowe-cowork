//! **The rerank stage's execution provider, the tier-1 reserve, and the CUDA readings ADR-015
//! forbids inheriting — asserted where they are enforced.**
//!
//! Every test here drives `CrossEncoder::load_auto`, which is the function the product calls
//! through `--rerank-provider` *and* the function the daemon calls with `RerankChoice::Auto`.
//! None of them asserts that a constant has a value.
//!
//! # What CANNOT be asserted here, stated so a green run is not over-read
//!
//! `ort` 2.0.0-rc.10 exposes no way to enumerate a constructed session's active providers and no
//! node placement. M0c Session L measured **13.6% of nodes still running on CPU** under a
//! successfully registered CUDA session. So the strongest available claim is *a CUDA session
//! constructed and was not permitted to fall back silently*, and these tests make exactly that one.

#[path = "../../marlowe-loop/tests/common/exclusive.rs"]
mod exclusive;
use exclusive::exclusive;

use std::path::{Path, PathBuf};

use marlowe_memory::cue::dense::vram::{Probe, Reserve, Tier1Runtime};
use marlowe_memory::rerank::{
    CrossEncoder, RerankChoice, RerankProvider, MAX_BATCH, MODEL_FILE, SHIPPED_THREADS,
};

fn model_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/marlowe-memory -> repo root")
        .join("models")
        .join("ms-marco-MiniLM-L-2-v2-ft-session-j")
}

/// Skip only for absence. `models/` is gitignored and a fresh clone legitimately lacks it.
fn dir_or_skip() -> Option<PathBuf> {
    let dir = model_dir();
    if dir.join(MODEL_FILE).exists() {
        Some(dir)
    } else {
        eprintln!("SKIP: run `python tools/fetch_rerankers.py`");
        None
    }
}

/// Whether a CUDA session constructs here, **printed rather than assumed**.
///
/// A CUDA test that silently no-ops on a CPU-only machine is green and vacuous; one that says
/// which branch it took is evidence either way.
fn cuda_or_report(dir: &Path) -> bool {
    match CrossEncoder::load_auto(dir, SHIPPED_THREADS, RerankChoice::Cuda, Probe::Device, Reserve::None)
    {
        Ok(_) => true,
        Err(e) => {
            eprintln!("NO CUDA RERANK ON THIS MACHINE: {e}");
            false
        }
    }
}

/// A slate that is heterogeneous in length **on purpose**.
///
/// Session K's Python batch check used synthetic documents differing only by a trailing integer —
/// near-identical lengths, so a length-heterogeneity effect inside the batch could not have
/// appeared. That is the exact gap Session L's slate-based measurement closed, and repeating the
/// weaker fixture here would repeat the weaker measurement.
fn slate() -> (&'static str, Vec<&'static str>) {
    (
        "which database did the analytics warehouse move to",
        vec![
            "I moved the analytics warehouse off Postgres in April after the ingest job timed out.",
            "short",
            "The quarterly review covered headcount, the hiring freeze, the revised travel policy, \
             the new expense tool, and a long tail of small operational items that nobody had time \
             to discuss properly, including the database migration which was mentioned once in \
             passing and then dropped entirely from the agenda for the rest of the meeting.",
            "Postgres was fine for the transactional side.",
            "We picked ClickHouse for the warehouse.",
            "unrelated: the coffee machine is broken again",
            "Migration notes: dual-write for two weeks, then cut over reads, then drop the old \
             tables once the backfill verifier is clean.",
            "a",
            "The analytics team asked about retention windows.",
            "Nothing to do with databases at all, this is about the office move.",
        ],
    )
}

// ─────────────────────────────────────────────────────────────────────────────────────────────
// The tier-1 reserve — ADR-045 §4
// ─────────────────────────────────────────────────────────────────────────────────────────────

#[test]
fn a_reserve_for_an_uninstalled_model_is_zero_and_says_why() {
    // The safe branch, and it must be legible rather than merely zero. A machine with no tier 1
    // has nothing to yield to, and a reserve that silently read zero would be indistinguishable
    // from one that failed to read at all — which is the whole family this project logs.
    let r = Reserve::ForTier1 { model: "definitely-not-a-model:0b", runtime: Tier1Runtime::Ollama }.read();
    assert_eq!(r.bytes, 0, "an uninstalled model cannot claim device memory");
    assert!(
        r.reason.contains("not installed") || r.reason.contains("no `ollama`"),
        "the branch must be named, got {:?}",
        r.reason
    );
}

/// **ADR-060 §3, and it fixes a defect that shipped before ADR-060 existed.**
///
/// The reserve used to know a model NAME and ask Ollama two questions about it. With a hosted
/// provider — or a `llama-server` — running tier 1, `ollama ps` is empty while `ollama list` still
/// names the model, so the whole reserve was applied against a card that had nothing on it. The
/// embedder resolved to CPU and `--status` explained why with a sentence that was internally
/// coherent and wrong in every clause.
#[test]
fn a_runtime_that_is_not_ollama_reserves_nothing_and_says_which_runtime_answered() {
    // Zero is produced by FIVE branches of `read()`, so the number cannot say which one ran and
    // the assertion has to be on the reason. Ask what each of these would read if the dispatch
    // were deleted and every call fell through to the Ollama branch: all three reasons would come
    // from that branch, and the two below would fail by name.
    let off = Reserve::ForTier1 {
        model: "definitely-not-a-model:0b",
        runtime: Tier1Runtime::NotOnThisCard,
    }
    .read();
    assert_eq!(off.bytes, 0);
    assert!(
        off.reason.contains("not resident on this card"),
        "the branch must be named: {:?}",
        off.reason
    );

    let served = Reserve::ForTier1 {
        model: "definitely-not-a-model:0b",
        runtime: Tier1Runtime::LlamaServerLoaded,
    }
    .read();
    assert_eq!(served.bytes, 0);
    // `llama-server`, hyphenated — `ollama` contains the substring `llama`, so the unhyphenated
    // spelling would match the Ollama branch's own sentences and prove nothing.
    assert!(
        served.reason.contains("llama-server"),
        "the branch must be named: {:?}",
        served.reason
    );

    // **The control.** Without it, a `read()` that returned zero-and-a-fixed-string for every
    // input passes both assertions above. The Ollama arm must reach a DIFFERENT branch, and it is
    // the only one of the three that can ever consult the machine.
    let ollama = Reserve::ForTier1 {
        model: "definitely-not-a-model:0b",
        runtime: Tier1Runtime::Ollama,
    }
    .read();
    assert!(
        !ollama.reason.contains("not resident on this card")
            && !ollama.reason.contains("llama-server"),
        "the Ollama arm took a non-Ollama branch, so the dispatch is not discriminating: {:?}",
        ollama.reason
    );
}

/// The **same model**, read two ways. Where the precondition holds this is the strongest form of
/// the control above: one name, one machine, two runtimes, two different numbers.
#[test]
fn the_same_model_reserves_gigabytes_under_ollama_and_nothing_under_a_llama_server() {
    let _gpu = exclusive("gpu");
    // The same constant `a_reserve_for_an_installed_model_is_larger_than_the_rerank_graph` uses,
    // and for the same reason: only 9B models are loaded on this box, and picking one by
    // `ollama list` row order is what made an earlier test machine-dependent.
    const TIER1: &str = "marlowe-red:9b";
    let under_ollama =
        Reserve::ForTier1 { model: TIER1, runtime: Tier1Runtime::Ollama }.read();
    let under_llama_server =
        Reserve::ForTier1 { model: TIER1, runtime: Tier1Runtime::LlamaServerLoaded }.read();

    assert_eq!(
        under_llama_server.bytes, 0,
        "a loaded llama-server's weights are already out of memory.free: {:?}",
        under_llama_server.reason
    );

    if under_ollama.bytes > 0 {
        assert_ne!(
            under_ollama.bytes, under_llama_server.bytes,
            "the runtime made no difference to the number, which is the defect: {:?} vs {:?}",
            under_ollama.reason, under_llama_server.reason
        );
    } else {
        // Printed rather than silently passed: `TIER1` being absent or already resident is the
        // ordinary state of this machine, and a test that reported success without the
        // precondition would be asserting nothing.
        eprintln!(
            "precondition absent: the Ollama arm read 0 for {TIER1} ({}), so there is no pair to \
             compare. Install it and unload it to exercise the discriminating case.",
            under_ollama.reason
        );
    }
}

#[test]
fn the_measurement_arm_reserves_nothing_and_admits_it() {
    // `Reserve::None` is the pre-ADR-045 behaviour, kept so the defect can be reproduced
    // deliberately. It must never be mistaken for "there was nothing to reserve".
    let r = Reserve::None.read();
    assert_eq!(r.bytes, 0);
    assert!(r.reason.contains("measurement"), "{:?}", r.reason);
}

#[test]
fn a_reserve_for_an_installed_model_is_larger_than_the_rerank_graph() {
    // The card is one machine resource and `cargo test --workspace` runs test binaries
    // concurrently. Without this, this file wedged the suite twice and failed a reserve
    // assertion a third time -- all four passing alone. See `common/exclusive.rs`.
    let _gpu = exclusive("gpu");
    // **The test that fails when the reserve is reverted to zero.** It needs a real tier-1 model,
    // so it reports rather than skips silently when there is none — and it is deliberately not
    // asserted against a fixed byte count, because the reserve is DERIVED from whatever Ollama
    // has and a literal here would be the hardcoded constant ADR-045 forbids.
    // **Through `vram::ollama_model_names`, which is bounded.** Until 2026-08-27 this line spawned
    // `ollama list` directly and waited on it with a bare `.output()` -- no null stdin, no ceiling,
    // the exact shape `bounded_output` was written for two days earlier after it wedged the
    // workspace suite from `vram.rs`. It was reproduced here, one file outside the reach of a guard
    // that greped a single module, and on 2026-08-26 this binary hung ~25 minutes at 1 GB RSS with
    // every target queued behind it. `tests/no_unbounded_external_commands.rs` is the widened
    // guard; it covers `tests/` for that reason, and it is why the offending spelling cannot even
    // appear in this comment.
    let Some(installed_names) = marlowe_memory::cue::dense::vram::ollama_model_names() else {
        // Both branches named, because the reading cannot tell them apart and asserting one would
        // render a timeout as "not installed".
        eprintln!(
            "SKIP: `ollama list` did not answer -- either there is no `ollama` on this machine, or \
             it did not respond inside the probe ceiling. Either way there is no tier 1 to reserve \
             for."
        );
        return;
    };

    // **NAMED, NOT WHATEVER SORTED FIRST.** This used to take row 1 of `ollama list`, which makes
    // the test do different work on every machine — and on the development box row 1 is
    // `marlowe-dusk:27b`, 18 GB on a 16 GB card. That is the over-subscribed path, and on
    // 2026-08-25 it wedged the entire workspace suite: `Reserve::read()` shelled out through an
    // unbounded `Command::output()` and never came back, so every crate after this one went unrun.
    //
    // `vram.rs`'s own header already states the rule this test was breaking — *"ONLY 9B MODELS ARE
    // LOADED ON THIS MACHINE. 20B AND LARGER ARE OFF LIMITS"*, with `marlowe-red:9b` named as the
    // constant every coexistence figure here is measured against. A documented constraint that the
    // test ignored is a declared control with no reader.
    //
    // So: ask for the model the project actually measures against, and **skip by name** rather than
    // silently measuring a different one.
    const TIER1: &str = "marlowe-red:9b";
    let Some(first) = installed_names.iter().map(String::as_str).find(|n| *n == TIER1) else {
        eprintln!(
            "SKIP: {TIER1} is not installed, and this test will not substitute another model -- \
             picking one by list order is what made it machine-dependent. Installed: {installed_names:?}"
        );
        return;
    };

    let r = Reserve::ForTier1 { model: first, runtime: Tier1Runtime::Ollama }.read();
    // Resident models are already out of `memory.free`, so a zero there is CORRECT and is not a
    // failure of the reserve. Named rather than silently tolerated.
    if r.reason.contains("already resident") {
        eprintln!("{first} is resident; free memory is already net of it -- reserve 0 is correct");
        return;
    }
    assert!(
        r.bytes > 1024 * 1024 * 1024,
        "a language model claims gigabytes; {first} reserved {} bytes ({})",
        r.bytes,
        r.reason
    );
    assert!(r.reason.contains("tier 1"), "{:?}", r.reason);
}

#[test]
fn the_reserve_can_push_the_rerank_off_a_card_that_looks_free() {
    // The card is one machine resource and `cargo test --workspace` runs test binaries
    // concurrently. Without this, this file wedged the suite twice and failed a reserve
    // assertion a third time -- all four passing alone. See `common/exclusive.rs`.
    let _gpu = exclusive("gpu");
    // **This is the defect ADR-045 §4 exists to fix, driven rather than described.** The card
    // reports plenty free; tier 1's claim is what makes it not free. Under the pre-ADR-045 code
    // the same reading opened a CUDA session, because it read `memory.free` and stopped there.
    let Some(dir) = dir_or_skip() else { return };

    // 8 GB free by the driver's reckoning — comfortably above any rerank floor.
    let plenty = Probe::Fixed(8 * 1024 * 1024 * 1024);
    // ...and a tier-1 model that wants essentially all of it. `Fixed` is not available for the
    // reserve by design (it is derived, not configured), so this uses the real reader against a
    // model that is either installed or not; the assertion is on the RELATIONSHIP between the two
    // loads rather than on an absolute outcome.
    let with_reserve =
        CrossEncoder::load_auto(&dir, SHIPPED_THREADS, RerankChoice::Auto, plenty, Reserve::ForTier1 { model: "marlowe-dusk:27b", runtime: Tier1Runtime::Ollama })
            .expect("auto never fails a run over memory");
    let without =
        CrossEncoder::load_auto(&dir, SHIPPED_THREADS, RerankChoice::Auto, plenty, Reserve::None)
            .expect("auto never fails a run over memory");

    // The reserve must appear in the reason whichever way it went. A reserve that is applied and
    // not reported is a reserve nobody can check.
    assert!(
        with_reserve.plan().reason.contains("tier 1")
            || with_reserve.plan().reason.contains("not installed")
            || with_reserve.plan().reason.contains("no `ollama`"),
        "the reserve branch must be on the plan: {:?}",
        with_reserve.plan().reason
    );
    // The no-reserve arm says "measurement" only when a CUDA session actually constructed. With
    // MARLOWE_CUDA_LIB_DIR unset it never gets that far and reports why instead -- a different
    // sentence for a different reason, both honest. Accepting only the first made the assertion a
    // statement about the machine's environment rather than about the reserve.
    assert!(
        without.plan().reason.contains("measurement")
            || without.plan().reason.contains("CUDA session did not construct"),
        "the no-reserve arm must say so, or say why it could not: {:?}",
        without.plan().reason
    );

    // And where the model IS installed and NOT resident, an 18 GB reserve against 8 GB free must
    // land the reranker on CPU. That is the property; it is asserted only when the precondition
    // holds, and the precondition is printed either way.
    let r = Reserve::ForTier1 { model: "marlowe-dusk:27b", runtime: Tier1Runtime::Ollama }.read();
    if r.bytes > 8 * 1024 * 1024 * 1024 {
        assert_eq!(
            with_reserve.provider(),
            RerankProvider::Cpu,
            "8 GB free minus an {} MB tier-1 reserve is not room for a GPU session: {:?}",
            r.bytes / (1024 * 1024),
            with_reserve.plan().reason
        );
    } else {
        eprintln!(
            "precondition absent: reserve read {} MB, which does not exceed the 8 GB probe ({})",
            r.bytes / (1024 * 1024),
            r.reason
        );
    }
}

#[test]
fn a_full_card_falls_back_to_cpu_instead_of_failing_the_run() {
    // The card is one machine resource and `cargo test --workspace` runs test binaries
    // concurrently. Without this, this file wedged the suite twice and failed a reserve
    // assertion a third time -- all four passing alone. See `common/exclusive.rs`.
    let _gpu = exclusive("gpu");
    // `auto` NEVER fails a run over a busy card. Failing would be a correctness outcome imposed
    // for a throughput reason — the trade ADR-044 refused and ADR-045 refuses again.
    let Some(dir) = dir_or_skip() else { return };
    let e = CrossEncoder::load_auto(&dir, SHIPPED_THREADS, RerankChoice::Auto, Probe::Fixed(0), Reserve::None)
        .expect("a full card is a fallback, not an error");
    assert_eq!(e.provider(), RerankProvider::Cpu);
    assert!(
        e.plan().reason.contains("floor"),
        "the MEMORY branch must be the one reported, not the construction branch: {:?}",
        e.plan().reason
    );
}

#[test]
fn an_explicit_cpu_request_never_touches_the_card() {
    // The arm every number published before 2026-08-17 was taken on. It must not consult the
    // device at all: a `cpu` run whose behaviour depended on free VRAM would not be reproducible.
    let Some(dir) = dir_or_skip() else { return };
    let e = CrossEncoder::load_auto(&dir, SHIPPED_THREADS, RerankChoice::Cpu, Probe::Fixed(0), Reserve::None)
        .expect("cpu loads on a full card");
    assert_eq!(e.provider(), RerankProvider::Cpu);
    assert_eq!(e.plan().requested, RerankChoice::Cpu);
    assert!(e.plan().reason.contains("explicitly"), "{:?}", e.plan().reason);
}

// ─────────────────────────────────────────────────────────────────────────────────────────────
// The CUDA readings. ADR-015: per graph AND per configuration, never inherited.
// ─────────────────────────────────────────────────────────────────────────────────────────────

#[test]
fn scoring_is_deterministic_on_cuda_within_one_process() {
    // The card is one machine resource and `cargo test --workspace` runs test binaries
    // concurrently. Without this, this file wedged the suite twice and failed a reserve
    // assertion a third time -- all four passing alone. See `common/exclusive.rs`.
    let _gpu = exclusive("gpu");
    let Some(dir) = dir_or_skip() else { return };
    if !cuda_or_report(&dir) {
        return;
    }
    let mut e =
        CrossEncoder::load_auto(&dir, SHIPPED_THREADS, RerankChoice::Cuda, Probe::Device, Reserve::None)
            .expect("cuda loads");
    let (q, docs) = slate();
    let a = e.score_batch(q, &docs).expect("scores");
    let b = e.score_batch(q, &docs).expect("scores");
    for (i, (x, y)) in a.iter().zip(b.iter()).enumerate() {
        assert_eq!(x.to_bits(), y.to_bits(), "pair {i}: bit-identical, not merely close");
    }
}

/// **THE MEASUREMENT ADR-045 CANNOT INHERIT.**
///
/// `runs/session-l/batch-invariance-batch10.json` swept batch sizes 1..10 at max |delta|
/// `0.000000000` — **on CPU**. A different execution provider is a different scorer (ADR-015), and
/// a GPU had never been measured here at all. cuBLAS selects a GEMM kernel by problem shape, so a
/// batch of 1 and a batch of 10 can take different tilings and therefore different reduction
/// orders; there was no reason to expect bit-identity and it was predicted not to hold.
///
/// **The property that decides the flip is ORDER, not bytes.** A numerical delta that never
/// reorders a slate is the same amendment ADR-029 made on this exact graph. An order change is
/// disqualifying, and the assertion is written that way round: it fails on a REORDER, and it
/// PRINTS the delta rather than asserting a threshold nobody derived.
#[test]
fn batch_invariance_on_cuda_is_measured_here_and_never_inherited_from_cpu() {
    // The card is one machine resource and `cargo test --workspace` runs test binaries
    // concurrently. Without this, this file wedged the suite twice and failed a reserve
    // assertion a third time -- all four passing alone. See `common/exclusive.rs`.
    let _gpu = exclusive("gpu");
    let Some(dir) = dir_or_skip() else { return };
    if !cuda_or_report(&dir) {
        return;
    }
    let mut e =
        CrossEncoder::load_auto(&dir, SHIPPED_THREADS, RerankChoice::Cuda, Probe::Device, Reserve::None)
            .expect("cuda loads");
    let (q, docs) = slate();
    assert_eq!(docs.len(), MAX_BATCH, "the sweep must reach MAX_BATCH to cover the shipped shape");

    // Reference: every pair scored ALONE, which is batch 1.
    let alone: Vec<f32> = docs
        .iter()
        .map(|d| e.score(q, d).expect("scores"))
        .collect();
    let order_of = |v: &[f32]| {
        let mut idx: Vec<usize> = (0..v.len()).collect();
        idx.sort_by(|&a, &b| v[b].partial_cmp(&v[a]).expect("no NaN logits").then(a.cmp(&b)));
        idx
    };
    let reference_order = order_of(&alone);

    let mut worst = 0f32;
    let mut worst_at = 0usize;
    for n in 1..=MAX_BATCH {
        let batched = e.score_batch(q, &docs[..n]).expect("scores");
        for (i, (b, a)) in batched.iter().zip(alone.iter()).enumerate() {
            let d = (b - a).abs();
            if d > worst {
                worst = d;
                worst_at = n;
            }
            let _ = i;
        }
        // Within this prefix, the order the ranker would see must match the order the same
        // prefix produces one-at-a-time. This is the property; the delta above is diagnostics.
        assert_eq!(
            order_of(&batched),
            order_of(&alone[..n]),
            "batch {n} reordered the slate against one-at-a-time scoring on CUDA -- ADR-045's \
             registered decision rule says DO NOT DEFAULT TO CUDA if this fires"
        );
    }
    eprintln!(
        "CUDA batch invariance, sizes 1..{MAX_BATCH}, shipped graph: max |batched - single| = \
         {worst:.9} (worst at batch {worst_at}), zero order changes"
    );
}

/// **The control for the test above.** A sweep that could not detect a reorder proves nothing.
#[test]
fn the_batch_invariance_check_can_actually_see_a_reordering() {
    let Some(dir) = dir_or_skip() else { return };
    if !dir.join(MODEL_FILE).exists() {
        return;
    }
    let mut e = CrossEncoder::load(&dir).expect("cpu loads");
    let (q, docs) = slate();
    let scores = e.score_batch(q, &docs).expect("scores");
    let order = |v: &[f32]| {
        let mut idx: Vec<usize> = (0..v.len()).collect();
        idx.sort_by(|&a, &b| v[b].partial_cmp(&v[a]).expect("no NaN").then(a.cmp(&b)));
        idx
    };
    let a = order(&scores);
    // Perturb one logit by more than the observed cross-provider spread and require the comparator
    // to notice. Without this, `assert_eq!(order, order)` in the test above would be green on a
    // comparator that returned a constant.
    let mut moved = scores.clone();
    let top = a[0];
    moved[top] = f32::MIN;
    assert_ne!(order(&moved), a, "the order comparator cannot see a reordering; it is not a check");
    assert!(
        scores.iter().any(|s| (s - scores[0]).abs() > 1e-3),
        "the slate must have a real score spread or a reorder check has nothing to detect"
    );
}

/// A CUDA session holds **device** memory. The cheapest possible check that it is on the card.
#[test]
fn a_loaded_cuda_rerank_session_holds_device_memory() {
    // The card is one machine resource and `cargo test --workspace` runs test binaries
    // concurrently. Without this, this file wedged the suite twice and failed a reserve
    // assertion a third time -- all four passing alone. See `common/exclusive.rs`.
    let _gpu = exclusive("gpu");
    let Some(dir) = dir_or_skip() else { return };
    if !cuda_or_report(&dir) {
        return;
    }
    let Some(before) = Probe::Device.free_bytes() else {
        eprintln!("SKIP: no readable device");
        return;
    };
    let mut e =
        CrossEncoder::load_auto(&dir, SHIPPED_THREADS, RerankChoice::Cuda, Probe::Device, Reserve::None)
            .expect("cuda loads");
    let (q, docs) = slate();
    let _ = e.score_batch(q, &docs);
    let after = Probe::Device.free_bytes().expect("still readable");
    let held = before.saturating_sub(after);
    eprintln!("rerank CUDA session held {} MB device (warmed at MAX_BATCH)", held / (1024 * 1024));
    assert!(
        held > 0,
        "a warmed CUDA session that holds no device memory did not run on the device"
    );
}
