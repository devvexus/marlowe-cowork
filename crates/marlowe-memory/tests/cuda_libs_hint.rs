//! **When CUDA cannot load and nothing is configured, does the error say what to do?**
//!
//! This is the case that produced the whole `cuda_libs` module. On 2026-08-17 a session ran a CUDA
//! request on this machine, received
//!
//! > `Error loading "onnxruntime_providers_cuda.dll" which depends on "cublasLt64_12.dll" which is
//! > missing. (Error 126)`
//!
//! and concluded that the machine had no CUDA — while a complete CUDA 12 runtime sat in
//! `site-packages/torch/lib` and an ADR said so. The message was accurate and it was not
//! *actionable*: it names a library, and a reader has no way to know from it that ONNX Runtime
//! resolves that library lazily against the process search path.
//!
//! So the error now carries the remedy. This file is the reader-check for that: delete the hint
//! from either loader and the assertion below fails.
//!
//! # This test binary must NOT set `MARLOWE_CUDA_LIB_DIR`
//!
//! The hint appears **only** when the variable is unset, which is the only state where it is news.
//! `tests/cuda_libs_wiring.rs` sets it and therefore cannot cover this; the two files are separate
//! processes on purpose, because the resolution is cached per process by `OnceLock`.
//!
//! # It skips where CUDA loads, and that skip is honest rather than convenient
//!
//! On a machine whose search path already holds the CUDA runtime — a real toolkit install, or a
//! shell that exported it — the session **constructs**, there is no error, and there is nothing to
//! assert. That is reported as a skip naming the reason, not as a pass.

use std::path::PathBuf;

use marlowe_memory::cue::dense::cuda_libs;
use marlowe_memory::cue::dense::embedder::{Embedder, ProviderChoice};
use marlowe_memory::cue::dense::vram::Probe;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

#[test]
fn a_cuda_load_failure_with_nothing_configured_names_the_variable_that_would_fix_it() {
    // **A SKIP, not a failure, and the distinction cost a suite run.** The first draft asserted
    // the variable was unset. That is the right vacuity guard and the wrong mechanism: once CUDA
    // is configured -- which is now the normal way to run this suite -- the assertion fired and a
    // correct build went red. A test whose subject is "the unconfigured case" has nothing to say
    // about a configured process and must say nothing, loudly.
    if std::env::var_os(cuda_libs::ENV_VAR).is_some() {
        eprintln!(
            "SKIP: {} is set, so this process is in the CONFIGURED state and the \
             unconfigured-failure branch cannot be reached. The hint is UNMEASURED in this run -- \
             it is covered by a suite run that does not set the variable.",
            cuda_libs::ENV_VAR
        );
        return;
    }

    let dir = repo_root().join("models").join("jina-embeddings-v2-small-en");
    if !dir.join("model.onnx").exists() {
        eprintln!("SKIP: run `python tools/fetch_model.py`");
        return;
    }

    match Embedder::load_with_provider(&dir, 1, None, ProviderChoice::Cuda, Probe::Device, marlowe_memory::cue::dense::vram::Reserve::None) {
        Ok(_) => {
            // Not a failure of the product -- CUDA working is the good outcome. It is a failure of
            // THIS measurement, and it says so instead of reporting a pass on an untested branch.
            eprintln!(
                "SKIP: CUDA constructed with {} unset, so the runtime is already on this \
                 process's search path and the unconfigured-failure branch was never taken.",
                cuda_libs::ENV_VAR
            );
        }
        Err(err) => {
            let text = err.to_string();
            // Only library-load failures carry the hint; an out-of-memory legitimately does not.
            if !text.contains("Error 126") && !text.contains("which is missing") {
                eprintln!("SKIP: CUDA failed for a reason that is not a missing library: {text}");
                return;
            }
            assert!(
                text.contains(cuda_libs::ENV_VAR),
                "a missing-library CUDA failure must name {} so the reader knows the remedy. \
                 Without it this is the message that got read as 'this machine has no CUDA'. \
                 Got: {text}",
                cuda_libs::ENV_VAR
            );
            assert!(
                text.contains("NOT FOUND") || text.contains("not found"),
                "the hint must distinguish NOT FOUND from NOT INSTALLED -- that conflation is the \
                 error this whole module exists to prevent. Got: {text}"
            );
        }
    }
}
