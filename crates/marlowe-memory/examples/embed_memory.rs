//! **Peak memory while embedding at `MAX_SEQ_LEN` — host and device, per provider, per width.**
//!
//! The number under test is the ALiBi relative-distance matrix, `8 x N x N` int64, allocated per
//! session. It is not visible in any unit test because a test that merely embeds a short string
//! never reaches the maximum — the two reference tests that DO reach it fail with `bad allocation`
//! rather than reporting a size.
//!
//! Peak working set is read from the OS rather than instrumented in-process, because the
//! allocation happens inside ONNX Runtime's arena and a Rust allocator hook would not see it. See
//! `cue::dense::hostmem`, which `tests/session_footprint.rs` asserts a bound against — one
//! definition, so the reported figure and the asserted figure are the same quantity.
//!
//! **Device memory is read device-level, not per-process**, and the distinction is load-bearing on
//! this machine: `nvidia-smi --query-compute-apps=...,used_memory` returns `[N/A]` under WDDM for
//! every process, so a per-process column would print a sentinel and a careless reader would take
//! it for zero. `memory.free` for the whole card is real, and the delta across a load is what a
//! session cost — inflated by anything else that allocated during the window, which is why the
//! baseline is printed too.
//!
//! ```text
//! cargo run -p marlowe-memory --release --example embed_memory -- <model-dir> <workers> <cpu|cuda|auto>
//! ```

use marlowe_memory::cue::dense::embedder::{Embedder, ProviderChoice};
use marlowe_memory::cue::dense::hostmem::{alibi_matrix_bytes, peak_working_set_bytes};
use marlowe_memory::cue::dense::vram::{free_bytes, Probe};
use marlowe_memory::cue::dense::MAX_SEQ_LEN;

fn mb(b: u64) -> f64 {
    b as f64 / 1024.0 / 1024.0
}

fn host_peak() -> u64 {
    peak_working_set_bytes().unwrap_or(0)
}

/// Device memory held since `baseline`, or `None` where the card is unreadable.
///
/// Signed by intent: another process freeing memory mid-run makes this negative, and a saturating
/// subtraction would render that as a confident `0.0 MB` — a wrong number that looks like a
/// measurement. `None`/negative are visible; a floored zero is not.
fn device_held(baseline: Option<u64>) -> Option<i64> {
    let (b, now) = (baseline?, free_bytes()?);
    Some(b as i64 - now as i64)
}

fn main() {
    let dir = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "models/jina-embeddings-v2-small-en".to_string());
    let workers: usize = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(1);
    let provider_arg = std::env::args().nth(3).unwrap_or_else(|| "cpu".to_string());
    let choice = match provider_arg.as_str() {
        "cpu" => ProviderChoice::Cpu,
        "cuda" => ProviderChoice::Cuda,
        "auto" => ProviderChoice::Auto,
        other => {
            println!("unknown provider {other:?}; expected cpu, cuda or auto");
            return;
        }
    };

    println!("MAX_SEQ_LEN = {MAX_SEQ_LEN}, workers = {workers}, provider requested = {provider_arg}");
    println!(
        "predicted ALiBi matrix (8 x N x N int64, PER SESSION): {:.1} MB; x{workers} = {:.1} MB",
        mb(alibi_matrix_bytes(MAX_SEQ_LEN)),
        mb(alibi_matrix_bytes(MAX_SEQ_LEN) * workers as u64)
    );
    let device_baseline = free_bytes();
    match device_baseline {
        Some(free) => println!("device free at baseline: {:.1} MB", mb(free)),
        None => println!("device: no readable NVIDIA card"),
    }
    println!("host peak before load: {:.1} MB\n", mb(host_peak()));

    let mut embedder = match Embedder::load_with_provider(
        std::path::Path::new(&dir),
        workers,
        None,
        choice,
        Probe::Device, marlowe_memory::cue::dense::vram::Reserve::None,) {
        Ok(e) => e,
        Err(e) => {
            println!("embedder did not open: {e}");
            return;
        }
    };
    let plan = embedder.plan().clone();
    println!(
        "PLAN: {} with {} of {} session(s) -- {}",
        plan.provider.name(),
        plan.workers,
        plan.requested,
        plan.reason
    );
    println!(
        "after load: host peak {:.1} MB, device held {}",
        mb(host_peak()),
        device_held(device_baseline).map_or("[unreadable]".to_string(), |d| format!("{:.1} MB", d as f64 / 1048576.0))
    );

    // A short text first — this is the common case and it must stay cheap.
    let short = "a niche equation appears in the middle of a long document";
    match embedder.embed(short) {
        Ok(v) => println!(
            "short text ({} dims): host peak {:.1} MB, device held {}",
            v.len(),
            mb(host_peak()),
            device_held(device_baseline)
                .map_or("[unreadable]".to_string(), |d| format!("{:.1} MB", d as f64 / 1048576.0))
        ),
        Err(e) => println!("short text FAILED: {e}"),
    }

    // **Then a text that reaches MAX_SEQ_LEN**, which is the case that allocates the matrix and
    // the case the failing reference tests exercise. One token per word keeps it simple; the
    // tokenizer truncates at the cap regardless.
    let long = "equation ".repeat(MAX_SEQ_LEN * 2);
    match embedder.embed(&long) {
        Ok(v) => println!(
            "at MAX_SEQ_LEN ({} dims): host peak {:.1} MB, device held {}",
            v.len(),
            mb(host_peak()),
            device_held(device_baseline)
                .map_or("[unreadable]".to_string(), |d| format!("{:.1} MB", d as f64 / 1048576.0))
        ),
        Err(e) => println!("at MAX_SEQ_LEN FAILED: {e}"),
    }

    // **A full-width batch, because per-session is the question and a batch of one answers a
    // different one.** `embed_batch` splits contiguously across sessions, so a batch of `workers`
    // long texts is the only shape that has every session holding its matrix at once.
    let batch: Vec<String> = (0..plan.workers.max(1)).map(|_| long.clone()).collect();
    match embedder.embed_batch(&batch) {
        Ok(v) => println!(
            "batch of {} at MAX_SEQ_LEN: host peak {:.1} MB, device held {}",
            v.len(),
            mb(host_peak()),
            device_held(device_baseline)
                .map_or("[unreadable]".to_string(), |d| format!("{:.1} MB", d as f64 / 1048576.0))
        ),
        Err(e) => println!("batch FAILED: {e}"),
    }

    let peak = host_peak();
    println!("\nFINAL host peak: {:.1} MB over {} session(s)", mb(peak), plan.workers);
    if plan.workers > 0 {
        println!("  -> {:.1} MB per session (host, aggregate / width)", mb(peak) / plan.workers as f64);
    }
    match device_held(device_baseline) {
        Some(held) if plan.workers > 0 => println!(
            "FINAL device held: {:.1} MB -> {:.1} MB per session",
            held as f64 / 1048576.0,
            held as f64 / 1048576.0 / plan.workers as f64
        ),
        Some(held) => println!("FINAL device held: {:.1} MB", held as f64 / 1048576.0),
        None => println!("FINAL device held: [unreadable]"),
    }
}
