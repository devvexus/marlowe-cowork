//! **What ONE embedder session costs, in bytes, measured — host and device.**
//!
//! # Why this file exists
//!
//! `MAX_SEQ_LEN` moved 8192 → 1024 because this export builds ALiBi's relative-distance matrix
//! explicitly, `[8, N, N]` at int64, **per session**: `8·N²·8` bytes, quadratic. At 8192 that is
//! 4,294,967,296 — and it was not failing, it was *succeeding*, silently, whenever the machine
//! happened to have 8.6 GB free. Nothing observed it, because no unit test embedded at the
//! declared maximum: the only two that did were the only two failing.
//!
//! So the guard has to be a **byte read from outside the process at a per-session granularity**.
//! An aggregate hides it — 4.9 GB across eight sessions is 612 MB each and unremarkable, and the
//! same 4.9 GB in ONE session is the defect. And a `sizeof` or a declared constant cannot see it
//! at all, because the allocation happens inside ORT's arena.
//!
//! # Two questions, two mechanisms, and they fail for different reasons
//!
//! `hostmem::tests::the_shipped_cap_predicts_a_footprint_in_megabytes_not_gigabytes` is pure
//! arithmetic over the **declared** cap: it fires the moment `MAX_SEQ_LEN` is raised, needs no
//! model and cannot be flaky.
//!
//! This file asks the other question — *does one real session, embedding at the cap, actually cost
//! what the cap predicts?* — which arithmetic cannot answer. A graph that allocated at a length
//! nobody asked for would satisfy every constant in the build and blow the measurement here. That
//! is the case the 8192 episode actually was.
//!
//! # ONE test function, and the reason is the instrument
//!
//! Peak working set is a **process-wide high-water mark**. Two `#[test]`s in one binary share it
//! and cargo runs them on parallel threads, so a CUDA session's ~1 GB of host-side context would
//! land in the CPU test's reading and fail it — alphabetically the CUDA one would even go first.
//! The host half therefore runs to completion before anything touches the card. This is the
//! instrument dictating the shape of the test, which is the right way round.
//!
//! # The ceilings are FIXED LITERALS, and that is the whole design
//!
//! A bound computed from `MAX_SEQ_LEN` rises with `MAX_SEQ_LEN`, so raising the cap would raise its
//! own ceiling and the guard would report nothing at exactly the moment its subject changed. That
//! is the "declared control that nothing reads" family with the reader present and useless. These
//! numbers are absolute, they sit far below the 4.29 GB this exists to catch, and they are far
//! enough above today's reading that ordinary machine noise cannot trip them.

use std::path::{Path, PathBuf};

use marlowe_memory::cue::dense::embedder::{Embedder, ProviderChoice, MODEL_FILE};
use marlowe_memory::cue::dense::hostmem::{alibi_matrix_bytes, peak_working_set_bytes};
use marlowe_memory::cue::dense::vram::Probe;
use marlowe_memory::cue::dense::MAX_SEQ_LEN;

/// One session's host footprint, embedding at `MAX_SEQ_LEN`.
///
/// **Measured 312.4 MB at `MAX_SEQ_LEN = 1024`, one CPU session** — 213.0 MB after load, +77.3 MB
/// for one forward pass at the cap, against a 64.0 MB prediction for the matrix alone. 768 MB is
/// roughly 2.5x that and an order of magnitude under the 4,294,967,296-byte matrix this guards
/// against, so the defect cannot hide beneath it and a busy machine cannot fake it.
///
/// **Deliberately loose rather than tight**, and the division of labour is the reason: raising
/// `MAX_SEQ_LEN` is already caught deterministically by
/// `hostmem::tests::the_shipped_cap_predicts_a_footprint_in_megabytes_not_gigabytes`, which needs
/// no model and cannot be flaky. What only a measurement can catch is a graph allocating at a
/// length **nobody declared**, and that failure is orders of magnitude, not tens of percent. A
/// ceiling tuned to fire at the next cap up would buy a duplicate of the arithmetic test at the
/// price of false failures on a loaded machine.
const HOST_CEILING_BYTES: u64 = 768 * 1024 * 1024;

/// One CUDA session's **device** footprint, embedding at `MAX_SEQ_LEN`.
///
/// **Measured 802 MB at `MAX_SEQ_LEN = 1024`, one CUDA session** — 383 MB at load (the 124 MB graph
/// plus a CUDA context), 418 MB after a short text, 802 MB after one forward pass at the cap. The
/// forward pass costs 384 MB where the ALiBi matrix alone predicts 64 MB; the rest is attention
/// activations, `[batch, 8, N, N]` f32, quadratic in the same way. That gap is recorded in
/// `STATE.md` rather than swept under the prediction.
///
/// Higher than the host ceiling on purpose and not because the graph is bigger: a CUDA context and
/// ORT's device arena are per-process overheads that land inside this delta, and `nvidia-smi`
/// reports the whole card, so anything else allocating during the read lands in it too. It still
/// refuses a **gigabytes-per-session** reading, which is the only thing being asked — and because
/// the whole forward-pass term is quadratic, `MAX_SEQ_LEN = 2048` would predict ~1,954 MB here and
/// trip it.
const DEVICE_CEILING_BYTES: u64 = 1536 * 1024 * 1024;

fn model_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/marlowe-memory -> repo root")
        .join("models")
        .join("jina-embeddings-v2-small-en")
}

fn dir_or_skip() -> Option<PathBuf> {
    let dir = model_dir();
    if dir.join(MODEL_FILE).exists() {
        Some(dir)
    } else {
        eprintln!("SKIP: run `python tools/fetch_model.py`");
        None
    }
}

fn mb(bytes: u64) -> f64 {
    bytes as f64 / 1024.0 / 1024.0
}

/// A text guaranteed to tokenize past the cap, so the matrix is actually built.
///
/// **A short string would leave this green on a build where the matrix is 4 GB**, because the
/// allocation is sized by the batch's longest sequence and never happens. The 8192 episode is
/// precisely that: every existing test embedded short strings and none of them saw it.
fn a_text_that_reaches_the_cap() -> String {
    "equation ".repeat(MAX_SEQ_LEN * 2)
}

#[test]
fn one_session_at_the_cap_costs_megabytes_not_gigabytes_on_host_and_on_device() {
    let Some(dir) = dir_or_skip() else { return };
    let model_bytes = std::fs::metadata(dir.join(MODEL_FILE)).map(|m| m.len()).unwrap_or(0);
    let predicted = alibi_matrix_bytes(MAX_SEQ_LEN);
    let long = a_text_that_reaches_the_cap();

    // ---------------------------------------------------------------- host, and it goes FIRST
    match peak_working_set_bytes() {
        None => eprintln!("SKIP (host half): the OS did not report a peak working set here"),
        Some(before) => {
            let mut embedder =
                Embedder::load_with_provider(&dir, 1, None, ProviderChoice::Cpu, Probe::Device, marlowe_memory::cue::dense::vram::Reserve::None)
                    .expect("the CPU embedder loads");
            embedder.embed(&long).expect("embeds at the cap");
            let after = peak_working_set_bytes().expect("the OS reported a peak once already");
            eprintln!(
                "host peak: {:.1} MB before, {:.1} MB after ONE CPU session at \
                 MAX_SEQ_LEN = {MAX_SEQ_LEN}; the [8, N, N] int64 matrix predicts {:.1} MB of that",
                mb(before),
                mb(after),
                mb(predicted)
            );
            assert!(
                after <= HOST_CEILING_BYTES,
                "ONE session embedding at MAX_SEQ_LEN = {MAX_SEQ_LEN} peaked at {:.1} MB, over \
                 the {:.1} MB ceiling. The [8, N, N] int64 ALiBi matrix predicts {:.1} MB at this \
                 cap, so a reading far above it means the graph is allocating at a length nobody \
                 asked for -- the 8192 defect with the constant left alone. This is PER SESSION: \
                 at the derived worker count it is multiplied.",
                mb(after),
                mb(HOST_CEILING_BYTES),
                mb(predicted)
            );
        }
    }

    // -------------------------------------------------------------------------------- device
    //
    // The same question on the card, and it is the one that matters now that the embedder can run
    // there: 4 GB on host RAM was survivable on a 64 GB box and is fatal on a 16 GB card shared
    // with a resident model server.
    //
    // **Skipped loudly where CUDA does not construct**, because a CUDA assertion that silently
    // no-ops on a CPU-only machine is green and vacuous.
    let Some(before) = marlowe_memory::cue::dense::vram::free_bytes() else {
        eprintln!("SKIP (device half): no readable NVIDIA device, so there is no byte to observe");
        return;
    };
    let mut embedder =
        match Embedder::load_with_provider(&dir, 1, None, ProviderChoice::Cuda, Probe::Device, marlowe_memory::cue::dense::vram::Reserve::None) {
            Ok(e) => e,
            Err(err) => {
                eprintln!("SKIP (device half): CUDA does not construct here, so this property is \
                           UNMEASURED: {err}");
                return;
            }
        };
    embedder.embed(&long).expect("embeds at the cap on the device");

    let after = marlowe_memory::cue::dense::vram::free_bytes().expect("the device was readable");
    let held = before.saturating_sub(after);
    eprintln!(
        "device: {:.1} MB free before, {:.1} MB after -> {:.1} MB held by ONE session at \
         MAX_SEQ_LEN = {MAX_SEQ_LEN} (graph {:.1} MB + ALiBi {:.1} MB + context/arena/activations)",
        mb(before),
        mb(after),
        mb(held),
        mb(model_bytes),
        mb(predicted)
    );

    // The floor is the same assertion `embedder_provider.rs` makes, and it is here because a
    // ceiling alone passes on exactly the failure that matters most: a session that quietly became
    // CPU holds no device memory at all, and 0 is under every ceiling.
    assert!(
        held >= model_bytes,
        "a CUDA session that ran at the cap holds at least the graph's {:.1} MB on the device; \
         free memory moved by only {:.1} MB, so either it fell back to CPU or another process \
         freed memory during the read",
        mb(model_bytes),
        mb(held)
    );
    assert!(
        held <= DEVICE_CEILING_BYTES,
        "ONE CUDA session at MAX_SEQ_LEN = {MAX_SEQ_LEN} held {:.1} MB of device memory, over the \
         {:.1} MB ceiling. The [8, N, N] int64 matrix predicts {:.1} MB at this cap. Gigabytes per \
         session is the 8192 defect moved from host RAM onto a 16 GB card that is shared with a \
         model server.",
        mb(held),
        mb(DEVICE_CEILING_BYTES),
        mb(predicted)
    );
}
