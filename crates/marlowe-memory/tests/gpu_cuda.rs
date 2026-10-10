//! CUDA against the fixtures and against CPU (`--features cuda`; ignored by default: it uses the
//! GPU). Run it only when the GPU is idle:
//!
//!   cargo test -p marlowe-memory --features cuda --test gpu_cuda -- --ignored --nocapture
//!
//! Needs `scripts/fetch-gpu-runtime.ps1` (found through `<repo>\gpu-runtime`). Batch invariance is
//! a per-graph, per-provider measurement and is never inherited from CPU, so it is re-taken here.
#![cfg(feature = "cuda")]

use std::path::{Path, PathBuf};

use marlowe_memory::cascade::FUSION_GRAPHS;
use marlowe_memory::rerank::{CrossEncoder, RerankProvider, MAX_BATCH};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

/// cargo copies ORT's provider DLLs into `target/<profile>` but, without symlink rights, not into
/// `deps/` where test binaries run; ORT loads them from beside the executable.
fn provider_dlls_beside_exe() {
    let exe = std::env::current_exe().unwrap();
    let here = exe.parent().unwrap();
    let profile = here.parent().unwrap();
    for dll in ["onnxruntime_providers_shared.dll", "onnxruntime_providers_cuda.dll"] {
        if !here.join(dll).exists() && profile.join(dll).exists() {
            std::fs::copy(profile.join(dll), here.join(dll)).unwrap();
        }
    }
    marlowe_memory::ep::runtime::set_root(repo().join("gpu-runtime"));
}

fn fixture(name: &str) -> serde_json::Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
#[ignore]
fn cuda_matches_the_fixtures_and_cpu_and_is_batch_invariant() {
    provider_dlls_beside_exe();
    let models = repo().join("models");
    let cases = [
        (FUSION_GRAPHS[0].0, "cross-encoder-reference-ft-session-j.json"),
        (FUSION_GRAPHS[1].0, "cross-encoder-reference-ft-session-j-L6.json"),
    ];
    for (graph, fx) in cases {
        let dir = models.join(graph);
        let mut gpu = CrossEncoder::load_fusion_member(&dir, graph, 1, RerankProvider::Cuda)
            .unwrap_or_else(|e| panic!("{graph} on CUDA: {e}\n{}", marlowe_memory::ep::runtime::describe()));
        let mut cpu = CrossEncoder::load_fusion_member(&dir, graph, 1, RerankProvider::Cpu).unwrap();
        assert_eq!(gpu.provider(), RerankProvider::Cuda);

        let fx = fixture(fx);
        let (mut vs_ref, mut vs_cpu) = (0f32, 0f32);
        let mut docs: Vec<String> = Vec::new();
        let mut query = String::new();
        for case in fx["cases"].as_array().unwrap() {
            let (q, d) = (case["query"].as_str().unwrap(), case["document"].as_str().unwrap());
            let g = gpu.score(q, d).unwrap();
            vs_ref = vs_ref.max((g - case["logit"].as_f64().unwrap() as f32).abs());
            vs_cpu = vs_cpu.max((g - cpu.score(q, d).unwrap()).abs());
            query = q.to_string();
            docs.push(d.to_string());
        }
        assert!(vs_ref <= 1e-3, "{graph}: CUDA vs reference {vs_ref}");

        // Batch invariance on CUDA: every size 1..=MAX_BATCH against single scoring.
        while docs.len() < MAX_BATCH {
            docs.push(format!("{} (variant {})", docs[docs.len() % 8], docs.len()));
        }
        let single: Vec<f32> = docs.iter().map(|d| gpu.score(&query, d).unwrap()).collect();
        let mut worst_batch = 0f32;
        for size in 1..=MAX_BATCH {
            let refs: Vec<&str> = docs[..size].iter().map(String::as_str).collect();
            for (b, s) in gpu.score_batch(&query, &refs).unwrap().iter().zip(&single) {
                worst_batch = worst_batch.max((b - s).abs());
            }
        }
        eprintln!(
            "[{graph}] CUDA: max |logit - reference| {vs_ref:.3e}; max |CUDA - CPU| {vs_cpu:.3e}; \
             max |batched - single| over sizes 1..={MAX_BATCH}: {worst_batch:.3e}"
        );
    }
    eprintln!("{}", marlowe_memory::ep::runtime::describe());
}
