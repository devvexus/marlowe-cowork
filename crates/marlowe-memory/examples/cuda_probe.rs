//! Why a CUDA session does or does not construct, on BOTH ONNX graphs this project loads.
//!
//! The reranker is the control. `rerank.rs` has requested CUDA since ADR-029, so if the embedder
//! fails and the reranker succeeds the cause is in the embedder; if both fail it is the machine,
//! and the embedder change is not what to look at.
use std::path::Path;
use marlowe_memory::cue::dense::embedder::{Embedder, ProviderChoice};
use marlowe_memory::cue::dense::vram::Probe;
use marlowe_memory::rerank::{CrossEncoder, RerankProvider};

fn main() {
    let embed_dir = Path::new("models/jina-embeddings-v2-small-en");
    let rerank_dir = Path::new("models/ms-marco-MiniLM-L-2-v2-ft-session-j");

    println!("-- embedder, ProviderChoice::Cuda (no fallback)");
    match Embedder::load_with_provider(embed_dir, 1, None, ProviderChoice::Cuda, Probe::Device, marlowe_memory::cue::dense::vram::Reserve::None) {
        Ok(_) => println!("   OK: a CUDA session constructed"),
        Err(e) => println!("   ERR: {e}"),
    }

    println!("-- reranker, RerankProvider::Cuda (the CONTROL: unchanged since ADR-029)");
    match CrossEncoder::load_with(rerank_dir, 1, RerankProvider::Cuda) {
        Ok(_) => println!("   OK: a CUDA session constructed"),
        Err(e) => println!("   ERR: {e}"),
    }

    println!("-- embedder, ProviderChoice::Cpu");
    match Embedder::load_with_provider(embed_dir, 1, None, ProviderChoice::Cpu, Probe::Device, marlowe_memory::cue::dense::vram::Reserve::None) {
        Ok(e) => println!("   OK: {:?}", e.plan()),
        Err(e) => println!("   ERR: {e}"),
    }
}
