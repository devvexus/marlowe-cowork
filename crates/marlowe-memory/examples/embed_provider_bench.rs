//! **CPU against CUDA, on the same texts, with the cache OFF.**
//!
//! ADR-015's rule is that a different execution provider is a different scorer, so a CUDA number
//! and a CPU number are two baselines rather than one comparison. This produces both, in one
//! process, on one fixed text set, at three sequence lengths.
//!
//! # The cache is disabled and that is the whole validity of the second column
//!
//! `Embedder::load_with_provider(..., cache_dir = None, marlowe_memory::cue::dense::vram::Reserve::None)`. With a cache, whichever provider runs
//! second reads back the first one's vectors and reports a "speedup" that is a disk read — a
//! fabricated number of exactly the kind this project keeps a ledger of. The line
//! `embedding cache: OFF` is printed with every table so a pasted result carries its own control.
//!
//! # What is measured, and what each number is NOT
//!
//! - **Per-embedding overhead** is wall clock over a fixed text set divided by the count. It
//!   includes tokenisation, which is CPU on both arms, so the CUDA column is not a kernel time and
//!   must not be read as one.
//! - **Peak host RAM** is the process's peak working set, read from the OS. On the CUDA arm it
//!   includes the driver's host-side allocations, which are large and are not the embedder's.
//! - **VRAM is read DEVICE-LEVEL, and which instrument produced it is printed with the table.**
//!   The per-process query -- `nvidia-smi --query-compute-apps=pid,used_memory` -- returns `[N/A]`
//!   for every process under WDDM on this machine, and the column printed `-1.0` for a whole
//!   session's worth of tables. `-1.0 MB` is not `0 MB`, but a sentinel in a numeric column is one
//!   careless reading away from being taken for one. So the shipped column is
//!   `memory.free` for the whole card, differenced against a baseline captured before the first
//!   session opens, and the per-process figure is printed beside it as `[N/A]` when the driver
//!   declines. The device-level reading includes anything ELSE that allocated during the window --
//!   which is the honest cost of the only instrument that works here, and is why the baseline is
//!   printed too.
//! - A CUDA session having constructed says nothing about node placement: M0c Session L measured
//!   13.6% of nodes still on CPU under a registered CUDA session.
//!
//! # Two components, TWO PROCESSES, and that is not an ergonomic choice
//!
//! `--component reranker` measures the cross-encoder on the same instruments. It is a separate
//! *invocation* rather than a second loop in the same run, because **peak working set is a
//! high-water mark for the whole process**: measuring the embedder first and the reranker second
//! in one process reports the embedder's peak under the reranker's name, and every row after the
//! first would be an aggregate wearing a component label. That is the failure the 4 GB ALiBi
//! allocation hid behind for months — 4.9 GB across eight sessions reads as 612 MB each and
//! unremarkable. One component per process; the peak then belongs to it.
//!
//! ```text
//! cargo run -p marlowe-memory --release --example embed_provider_bench
//! cargo run -p marlowe-memory --release --example embed_provider_bench -- --component reranker
//! ```

use std::time::Instant;

use marlowe_memory::cue::dense::embedder::{Embedder, EmbedProvider, ProviderChoice};
use marlowe_memory::cue::dense::tokenizer::{encode, Vocab};
use marlowe_memory::cue::dense::vram::{self, Probe};
use marlowe_memory::cue::dense::MAX_SEQ_LEN;

fn peak_host_bytes() -> u64 {
    #[cfg(windows)]
    {
        // Through `vram::bounded_output`: the crate's only sanctioned way to run an external
        // command, enforced by `tests/no_unbounded_external_commands.rs`. A bare `.output()` on a
        // `powershell` spawn is what wedged the workspace suite twice.
        let script = format!("(Get-Process -Id {}).PeakWorkingSet64", std::process::id());
        if let Some(o) = vram::bounded_output("powershell", &["-NoProfile", "-Command", &script]) {
            if let Ok(s) = String::from_utf8(o.stdout) {
                if let Ok(v) = s.trim().parse::<u64>() {
                    return v;
                }
            }
        }
        0
    }
    #[cfg(not(windows))]
    {
        std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|s| {
                s.lines().find(|l| l.starts_with("VmHWM:")).and_then(|l| {
                    l.split_whitespace().nth(1).and_then(|v| v.parse::<u64>().ok())
                })
            })
            .map(|kb| kb * 1024)
            .unwrap_or(0)
    }
}

/// This process's own device memory, in bytes, or `None` where the driver will not say.
///
/// **`None` is the normal answer on this machine and it must never be rendered as a number.**
/// WDDM does not report per-process device memory, so `used_memory` comes back as the literal
/// `[N/A]` for every row -- including this process's. The parse therefore fails, which is correct:
/// a reading that does not exist is `None`, and the caller prints `[N/A]`.
///
/// Kept rather than deleted because it is the *right* instrument where it works: the card is
/// shared, and on a driver that answers, this attributes bytes to a process where a card-wide
/// delta cannot.
fn own_vram_bytes() -> Option<u64> {
    let out = vram::bounded_output(
        "nvidia-smi",
        &["--query-compute-apps=pid,used_memory", "--format=csv,noheader,nounits"],
    )?;
    let text = String::from_utf8(out.stdout).ok()?;
    let me = std::process::id().to_string();
    for line in text.lines() {
        let mut parts = line.split(',').map(str::trim);
        if parts.next() == Some(me.as_str()) {
            // `[N/A]` fails to parse and yields None -- deliberately, see above.
            return parts.next().and_then(|m| m.parse::<u64>().ok()).map(|m| m * 1024 * 1024);
        }
    }
    None
}

/// Device memory held since `baseline`, card-wide, or `None` where the card is unreadable.
///
/// Signed: another process freeing memory mid-run makes this negative, and a saturating
/// subtraction would render that as a confident `0.0 MB` -- a wrong number that looks like a
/// measurement.
fn device_held_since(baseline: Option<u64>) -> Option<i64> {
    let (b, now) = (baseline?, vram::free_bytes()?);
    Some(b as i64 - now as i64)
}

fn mb(b: u64) -> f64 {
    b as f64 / 1024.0 / 1024.0
}

/// A text of roughly `words` word-pieces. The actual token count is measured, never assumed.
fn text_of(words: usize, salt: usize) -> String {
    // A rotating handful of ordinary single-piece words, so the text is not one token repeated --
    // a degenerate input could be optimised differently by either provider.
    const BANK: [&str; 8] =
        ["equation", "morning", "report", "engine", "signal", "harbour", "letter", "number"];
    let mut s = String::with_capacity(words * 8);
    for i in 0..words {
        s.push_str(BANK[(i + salt) % BANK.len()]);
        s.push(' ');
    }
    s
}

struct Row {
    provider: &'static str,
    workers: usize,
    tokens: usize,
    texts: usize,
    ms_total: f64,
    peak_host: u64,
    /// Card-wide, differenced against the baseline taken before any session opened.
    device_held: Option<i64>,
    /// Per-process, `None` where the driver returns `[N/A]`.
    own_vram: Option<u64>,
}

fn run_case(
    dir: &std::path::Path,
    choice: ProviderChoice,
    workers: usize,
    texts: &[String],
    tokens: usize,
    device_baseline: Option<u64>,
) -> Option<Row> {
    // cache_dir = None. See the module header: with a cache this whole table is a disk benchmark.
    let mut embedder =
        match Embedder::load_with_provider(dir, workers, None, choice, Probe::Device, marlowe_memory::cue::dense::vram::Reserve::None) {
            Ok(e) => e,
            Err(e) => {
                println!("  {choice:?} x{workers}: did not load: {e}");
                return None;
            }
        };
    let plan = embedder.plan().clone();

    // One untimed pass so the arena and any lazy kernel selection are paid for outside the clock.
    let _ = embedder.embed_batch(&texts[..texts.len().min(2)]);

    let start = Instant::now();
    let out = embedder.embed_batch(texts);
    let elapsed = start.elapsed();
    if let Err(e) = out {
        println!("  {} x{}: FAILED: {e}", plan.provider.name(), plan.workers);
        return None;
    }

    let row = Row {
        provider: plan.provider.name(),
        workers: plan.workers,
        tokens,
        texts: texts.len(),
        ms_total: elapsed.as_secs_f64() * 1000.0,
        peak_host: peak_host_bytes(),
        // Both sampled while the sessions are still alive -- after the drop the driver has
        // released the memory and every reading would be zero.
        device_held: device_held_since(device_baseline),
        own_vram: own_vram_bytes(),
    };
    Some(row)
}

/// The value of `--component`, or `embedder` when it is absent.
fn component_arg() -> String {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == "--component")
        .and_then(|i| args.get(i + 1).cloned())
        .unwrap_or_else(|| "embedder".to_string())
}

/// The first **positional** argument — the model directory.
///
/// Every flag here takes a value, so a flag consumes two slots. This skips both rather than
/// skipping one: the first version skipped only the flag, so `--component reranker --provider cpu`
/// returned `--provider` as the model directory and the load failed with
/// *"--provider\model.onnx does not exist"*. The error was clear, which is the only reason it
/// cost seconds rather than a mislabelled table.
fn dir_arg(default: &str) -> String {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        if args[i].starts_with("--") {
            i += 2;
            continue;
        }
        return args[i].clone();
    }
    default.to_string()
}

fn main() {
    match component_arg().as_str() {
        "embedder" => embedder_main(),
        "reranker" => reranker_main(),
        other => {
            println!("unknown --component {other:?}; expected `embedder` or `reranker`");
            std::process::exit(2);
        }
    }
}

fn embedder_main() {
    let dir = dir_arg("models/jina-embeddings-v2-small-en");
    let dir = std::path::Path::new(&dir);

    println!("== embedder provider benchmark ==");
    println!("embedding cache: OFF (cache_dir = None on every case)");
    println!("MAX_SEQ_LEN = {MAX_SEQ_LEN}");
    // Captured ONCE, before any session opens, and every `device held` column below is a
    // difference against it. A per-case baseline would silently absorb whatever the previous case
    // failed to release.
    let device_baseline = vram::free_bytes();
    println!(
        "free device memory at start: {}",
        device_baseline.map_or("no readable device".to_string(), |b| format!("{:.0} MB", mb(b)))
    );
    println!(
        "VRAM instrument: device-level `nvidia-smi --query-gpu=memory.free`, differenced against          that baseline. Per-process `--query-compute-apps` reports {} here.",
        match own_vram_bytes() {
            Some(b) => format!("{:.1} MB", mb(b)),
            None => "[N/A] -- WDDM does not attribute device memory per process".to_string(),
        }
    );
    println!(
        "co-resident processes on the card at start: {}",
        vram::bounded_output(
            "nvidia-smi",
            &["--query-compute-apps=process_name", "--format=csv,noheader"],
        )
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|t| {
                let names: Vec<String> = t
                    .lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .filter(|l| l.contains("llama") || l.contains("ollama"))
                    .map(str::to_string)
                    .collect();
                if names.is_empty() { "no llama/ollama process".to_string() } else { names.join(", ") }
            })
            .unwrap_or_else(|| "unreadable".to_string())
    );
    println!("CUDA session constructs on the shipped graph: {}", Embedder::cuda_available(dir));

    let vocab = match std::fs::read_to_string(dir.join("vocab.txt"))
        .ok()
        .and_then(|t| Vocab::parse(&t, "vocab.txt").ok())
    {
        Some(v) => v,
        None => {
            println!("no vocab.txt under {}; run `python tools/fetch_model.py`", dir.display());
            return;
        }
    };

    // The derived width -- the same expression the product uses, never a literal.
    let derived = std::thread::available_parallelism().map_or(1, |n| n.get().min(8));
    println!("derived worker count (available_parallelism().min(8)): {derived}\n");

    // **`--provider` narrows this to ONE arm, and the host column is why.**
    //
    // `peak_host_bytes` is a process high-water mark, so running CPU then CUDA in one process
    // reports the CPU arm's peak inside the CUDA arm's row. The default still runs both, because
    // the LATENCY columns are unaffected and every figure this project has published from this
    // file was taken that way — changing the default would silently make old and new runs
    // incomparable. Pass `--provider cpu` or `--provider auto` when the host column is the
    // quantity being read.
    //
    // Within one arm the loops ascend (100 -> 500 -> 1024 tokens, 1 -> 8 workers), so a smaller
    // cell's peak cannot have been inflated by a larger cell that had not run yet. Across
    // non-adjacent cells it still can: 1024x1 follows 500x8. `examples/embed_memory.rs` takes one
    // configuration per process and is the authority where that matters.
    let arms: &[ProviderChoice] = match std::env::args()
        .collect::<Vec<_>>()
        .iter()
        .position(|a| a == "--provider")
        .and_then(|i| std::env::args().nth(i + 1))
        .as_deref()
    {
        Some("cpu") => &[ProviderChoice::Cpu],
        Some("auto") => &[ProviderChoice::Auto],
        Some("cuda") => &[ProviderChoice::Cuda],
        Some(other) => {
            println!("unknown --provider {other:?}; expected cpu, cuda or auto");
            std::process::exit(2);
        }
        None => &[ProviderChoice::Cpu, ProviderChoice::Auto],
    };
    println!("provider arms in THIS process: {arms:?}
");

    const TEXTS_PER_CASE: usize = 32;
    let mut rows: Vec<Row> = Vec::new();

    for target in [100usize, 500, MAX_SEQ_LEN] {
        let texts: Vec<String> =
            (0..TEXTS_PER_CASE).map(|i| text_of(target, i)).collect();
        let measured = encode(&vocab, &texts[0], MAX_SEQ_LEN).input_ids.len();
        println!("-- target {target} word-pieces, measured {measured} tokens/text, {TEXTS_PER_CASE} texts");

        // **The GPU arm goes through `Auto`, not `Cuda`, and that is a safety decision.**
        // `ProviderChoice::Cuda` deliberately applies NO device-memory budget -- it is the refusal
        // arm for measurement cells. Opening eight unbudgeted sessions on a card that is already
        // holding an 11.5 GB model server is how a benchmark takes the user's model down. `Auto`
        // runs the same CUDA builder behind the budget, and reports what it actually got, so a row
        // that reads `workers 3` where 8 were asked for is the budget speaking.
        for choice in arms {
            for workers in [1usize, derived] {
                if let Some(row) = run_case(dir, *choice, workers, &texts, measured, device_baseline) {
                    println!(
                        "   {:<22} workers {:<2}  {:>8.2} ms total  {:>7.2} ms/embedding  host peak {:>7.1} MB  device held {:>9}  per-process {:>9}",
                        row.provider,
                        row.workers,
                        row.ms_total,
                        row.ms_total / row.texts as f64,
                        mb(row.peak_host),
                        row.device_held
                            .map_or("[unreadable]".to_string(), |d| format!("{:.1} MB", d as f64 / 1048576.0)),
                        row.own_vram.map_or("[N/A]".to_string(), |v| format!("{:.1} MB", mb(v))),
                    );
                    rows.push(row);
                }
                if derived == 1 {
                    break;
                }
            }
        }
        println!();
    }

    // The comparison, stated as two baselines rather than one ratio -- and the ratio printed only
    // where both arms exist, because a missing CUDA arm must read as absent, not as 1.0x.
    println!("== summary: ms per embedding ==");
    println!("{:<8} {:<10} {:>8} {:>12}", "tokens", "workers", "cpu", "cuda");
    let mut widths: Vec<usize> = rows.iter().map(|r| r.workers).collect();
    widths.sort_unstable();
    widths.dedup();
    let mut lens: Vec<usize> = rows.iter().map(|r| r.tokens).collect();
    lens.sort_unstable();
    lens.dedup();
    for tokens in &lens {
        for workers in &widths {
            let find = |name: &str| {
                rows.iter()
                    .find(|r| r.tokens == *tokens && r.workers == *workers && r.provider == name)
                    .map(|r| r.ms_total / r.texts as f64)
            };
            let cpu = find(EmbedProvider::Cpu.name());
            let cuda = find(EmbedProvider::Cuda.name());
            if cpu.is_none() && cuda.is_none() {
                continue;
            }
            println!(
                "{:<8} {:<10} {:>8} {:>12}{}",
                tokens,
                workers,
                cpu.map_or("-".to_string(), |v| format!("{v:.2}")),
                cuda.map_or("-".to_string(), |v| format!("{v:.2}")),
                match (cpu, cuda) {
                    (Some(c), Some(g)) if g > 0.0 => format!("   {:.2}x", c / g),
                    _ => String::new(),
                }
            );
        }
    }
    println!("\nembedding cache was OFF for every row above.");
}

/// **The cross-encoder, on the same instruments — one provider per process.**
///
/// # Why `--provider` is a flag and not a loop
///
/// `peak_host_bytes` is a **high-water mark for the process**, so a `for choice in [Cpu, Auto]`
/// loop reports the CPU arm's peak inside the CUDA arm's row. The embedder half of this file has
/// that shape and its host column is read with that caveat; the authoritative per-provider host
/// figure comes from `examples/embed_memory.rs`, which takes the provider as an argument for
/// exactly this reason. The reranker half does not repeat the mistake.
///
/// # Why the batch sizes DO share a process
///
/// They are run **1 then `MAX_BATCH`**, in that order, and the peak is sampled after each. A
/// high-water mark is monotonic, so the batch-1 reading cannot have been inflated by a batch that
/// had not run yet, and the batch-10 reading is genuinely the larger of the two. Reversing the
/// order would make the batch-1 row an aggregate.
///
/// # There is no cache to disable here
///
/// `CrossEncoder` holds no cache of any kind — no `cache_dir` parameter exists on any of its
/// constructors. The embedder's `cache_dir = None` has no analogue because there is nothing to
/// switch off, which is stated rather than left implicit: "cache OFF" must not read as an
/// unverified claim on a component that has none.
fn reranker_main() {
    use marlowe_memory::rerank::{
        CrossEncoder, RerankChoice, MAX_BATCH, MAX_SEQ_LEN as RERANK_SEQ, SHIPPED_THREADS,
    };

    let dir = dir_arg("models/ms-marco-MiniLM-L-2-v2-ft-session-j");
    let dir = std::path::Path::new(&dir);
    let args: Vec<String> = std::env::args().collect();
    let provider_arg = args
        .iter()
        .position(|a| a == "--provider")
        .and_then(|i| args.get(i + 1).cloned())
        .unwrap_or_else(|| "cpu".to_string());
    let Some(choice) = RerankChoice::parse(&provider_arg) else {
        println!("unknown --provider {provider_arg:?}; expected cpu, cuda or auto");
        std::process::exit(2);
    };

    println!("== reranker provider benchmark ==");
    println!("cache: NONE EXISTS on CrossEncoder (no cache_dir parameter on any constructor)");
    println!("MAX_SEQ_LEN = {RERANK_SEQ}, MAX_BATCH = {MAX_BATCH}, threads = {SHIPPED_THREADS}");
    println!("provider REQUESTED = {}", choice.asked());

    let device_baseline = vram::free_bytes();
    println!(
        "free device memory at start: {}",
        device_baseline.map_or("no readable device".to_string(), |b| format!("{:.0} MB", mb(b)))
    );
    println!(
        "VRAM instrument: device-level `nvidia-smi --query-gpu=memory.free`, differenced against \
         that baseline. Per-process `--query-compute-apps` reports {} here.",
        match own_vram_bytes() {
            Some(b) => format!("{:.1} MB", mb(b)),
            None => "[N/A] -- WDDM does not attribute device memory per process".to_string(),
        }
    );
    println!("host peak before load: {:.1} MB", mb(peak_host_bytes()));

    // `Reserve::None` -- this is a measurement cell, and a tier-1 reserve would make the resolved
    // provider depend on whether a language model happened to be resident. The coexistence arm
    // measures that deliberately by starting the model, not by letting the reserve decide.
    let mut encoder = match CrossEncoder::load_auto(
        dir,
        SHIPPED_THREADS,
        choice,
        Probe::Device,
        marlowe_memory::cue::dense::vram::Reserve::None,
    ) {
        Ok(e) => e,
        Err(e) => {
            println!("did not load: {e}");
            std::process::exit(1);
        }
    };
    let plan = encoder.plan().clone();
    println!(
        "\nprovider RESOLVED = {}  ({})  -- {}",
        plan.provider.name(),
        if plan.provider.default_batching() { "batched" } else { "sequential" },
        plan.reason
    );
    println!("host peak after load: {:.1} MB", mb(peak_host_bytes()));
    println!(
        "device held after load: {}",
        device_held_since(device_baseline)
            .map_or("[unreadable]".to_string(), |d| format!("{:.1} MB", d as f64 / 1048576.0))
    );

    // Documents long enough to reach the 256-token cap, so a row is not measuring a short-circuit
    // on a graph whose whole cost is quadratic in the sequence it actually fills.
    let query = "what did the harbour report say about the morning signal";
    let docs: Vec<String> = (0..MAX_BATCH).map(|i| text_of(300, i)).collect();
    let refs: Vec<&str> = docs.iter().map(String::as_str).collect();

    // Warm at MAX_BATCH so the arena and kernel selection are paid for outside every clock below.
    let _ = encoder.score_batch(query, &refs);

    const SLATES: usize = 30;
    println!("\n-- {SLATES} slates per cell, warmed at MAX_BATCH first");
    for batch in [1usize, MAX_BATCH] {
        let slice = &refs[..batch];
        let start = Instant::now();
        let mut scored = 0usize;
        for _ in 0..SLATES {
            match encoder.score_batch(query, slice) {
                Ok(v) => scored += v.len(),
                Err(e) => {
                    println!("   batch {batch}: FAILED: {e}");
                    break;
                }
            }
        }
        let elapsed = start.elapsed().as_secs_f64() * 1000.0;
        // **The vacuity guard, and it is here because a clean PASS over zero comparisons is this
        // project's most recent instrument failure.** `rerank_gate1` asked for batches of 12,
        // every call was refused, and it printed a confident zero. A latency divided by a pair
        // count that is zero would print `inf` or `NaN`; a latency divided by a pair count that
        // is merely SMALLER than asked for prints a plausible number. Both are refused here.
        if scored != SLATES * batch {
            println!(
                "   batch {batch}: VACUOUS -- {scored} pairs scored, {} expected",
                SLATES * batch
            );
            continue;
        }
        println!(
            "   batch {:<2}  {:>8.2} ms total  {:>7.3} ms/slate  {:>7.3} ms/pair  \
             host peak {:>7.1} MB  device held {:>9}  per-process {:>9}",
            batch,
            elapsed,
            elapsed / SLATES as f64,
            elapsed / scored as f64,
            mb(peak_host_bytes()),
            device_held_since(device_baseline)
                .map_or("[unreadable]".to_string(), |d| format!("{:.1} MB", d as f64 / 1048576.0)),
            own_vram_bytes().map_or("[N/A]".to_string(), |v| format!("{:.1} MB", mb(v))),
        );
    }

    // **The refusal above MAX_BATCH is a measured behaviour, not a claim.** It is printed with the
    // table because a reader who sees no row for batch 11 cannot otherwise tell whether the cell
    // was refused or simply not attempted.
    let over: Vec<&str> = std::iter::repeat("over").take(MAX_BATCH + 1).collect();
    match encoder.score_batch(query, &over) {
        Ok(_) => {
            println!("\n   batch {} was ACCEPTED -- the envelope guard did not fire", MAX_BATCH + 1)
        }
        Err(e) => println!("\n   batch {} refused, correctly: {e}", MAX_BATCH + 1),
    }

    println!(
        "\nco-resident processes on the card: {}",
        vram::bounded_output(
            "nvidia-smi",
            &["--query-compute-apps=process_name", "--format=csv,noheader"],
        )
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|t| {
                let names: Vec<String> = t
                    .lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .filter(|l| l.contains("llama") || l.contains("ollama"))
                    .map(str::to_string)
                    .collect();
                if names.is_empty() {
                    "no llama/ollama process".to_string()
                } else {
                    names.join(", ")
                }
            })
            .unwrap_or_else(|| "unreadable".to_string())
    );
}
