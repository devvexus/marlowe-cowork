//! **Is there a line of code that READS `MARLOWE_CUDA_LIB_DIR`?**
//!
//! `cue::dense::cuda_libs`'s own unit tests assert what `resolve` decides. Every one of them stays
//! green if the call into it is deleted from `Embedder::session` and `CrossEncoder::load_pinned` —
//! they assert the property **where it is defined** rather than **where it is enforced**, which is
//! this project's sixteenth-instance family and the reason `web`'s `inline_threshold_bytes: 0`
//! guarded nothing while a test asserted it was zero.
//!
//! This file is the other half. It sets the variable to a directory that does not exist and
//! requires the **loaders** to refuse in the words of `cuda_libs`. Delete either call site and this
//! fails: without it the request reaches ORT, which either fails with `Error 126` naming a library
//! (never the path the human typed) or, on a machine that happens to have the runtime on `PATH`
//! already, **succeeds** — and a silent success under a deliberately broken configuration is the
//! failure this whole module exists to prevent.
//!
//! # Why one test function and not four
//!
//! [`cuda_libs::ensure_search_path`] resolves once per process, by `OnceLock`, and cargo runs the
//! tests in one file inside one process. Splitting these assertions across functions would make
//! them order-dependent — the first to run would fix the answer for the rest — so they share a
//! function and the variable is set exactly once, before anything reads it.

use std::path::PathBuf;

use marlowe_memory::cue::dense::cuda_libs;
use marlowe_memory::cue::dense::embedder::{Embedder, ProviderChoice};
use marlowe_memory::cue::dense::vram::Probe;
use marlowe_memory::rerank::{CrossEncoder, RerankProvider};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

/// A path chosen to be absent and to be *recognisable in an error message*. If the refusal quotes
/// it, the refusal came from `cuda_libs`; ORT's own failure never mentions it.
const BOGUS: &str = "marlowe-cuda-libs-this-directory-does-not-exist";

#[test]
fn both_loaders_read_the_cuda_lib_variable_and_refuse_in_its_words() {
    let bogus = std::env::temp_dir().join(BOGUS);
    let _ = std::fs::remove_dir_all(&bogus);
    assert!(!bogus.exists(), "the fixture requires this path to be absent");

    // Set BEFORE anything constructs a CUDA session, because the resolution is cached for the
    // process. Nothing else in this file may load CUDA first.
    std::env::set_var(cuda_libs::ENV_VAR, &bogus);

    // The decision itself is a refusal. If this is not, the rest of the test proves nothing.
    let resolution = cuda_libs::ensure_search_path();
    let reason = resolution
        .rejection()
        .unwrap_or_else(|| panic!("a missing directory must be refused, got: {resolution:?}"));
    assert!(reason.contains(BOGUS), "the refusal must quote the path: {reason}");

    let embed_dir = repo_root().join("models").join("jina-embeddings-v2-small-en");
    let rerank_dir = repo_root().join("models").join("ms-marco-MiniLM-L-2-v2-ft-session-j");

    // ---- the embedder ----------------------------------------------------------------
    if embed_dir.join("model.onnx").exists() {
        let err = Embedder::load_with_provider(
            &embed_dir,
            1,
            None,
            ProviderChoice::Cuda,
            Probe::Device, marlowe_memory::cue::dense::vram::Reserve::None,)
        .err()
        .expect("a broken CUDA lib directory must refuse, never load");
        let text = err.to_string();
        assert!(
            text.contains(BOGUS),
            "Embedder::session does not read {}: the refusal did not quote the configured path, \
             which means the request reached ORT instead. Got: {text}",
            cuda_libs::ENV_VAR
        );
    } else {
        eprintln!("SKIP embedder half: run `python tools/fetch_model.py`");
    }

    // ---- the reranker, ADR-029's path ------------------------------------------------
    if rerank_dir.join("model.onnx").exists() {
        let err = CrossEncoder::load_with(&rerank_dir, 1, RerankProvider::Cuda)
            .err()
            .expect("a broken CUDA lib directory must refuse, never load");
        let text = err.to_string();
        assert!(
            text.contains(BOGUS),
            "CrossEncoder::load_pinned does not read {}: the refusal did not quote the configured \
             path, so ADR-029's CUDA path is unguarded. Got: {text}",
            cuda_libs::ENV_VAR
        );
    } else {
        eprintln!("SKIP reranker half: run `python tools/fetch_model.py --cross-encoder`");
    }

    // A guard on the guard: if BOTH models are absent this test asserted almost nothing, and it
    // should say so rather than report a pass.
    assert!(
        embed_dir.join("model.onnx").exists() || rerank_dir.join("model.onnx").exists(),
        "neither model is present, so neither call site was exercised. This test cannot \
         distinguish a wired loader from an unwired one here"
    );
}
