//! `marlowe --eval-adapter`: the §4.0 NDJSON target that `eval/` and
//! `tools/score_longmemeval.py` spawn. CPU only.
//!
//! Every flag that picks *what is measured* is required and has no default (profile root,
//! embedder, reranker), so a forgotten flag fails loudly instead of measuring another
//! configuration under this one's label. stderr is diagnostic only; the harness never parses it.

mod adapter;
mod dump;

use std::io::{self, BufReader};
use std::path::PathBuf;

use marlowe_memory::cue::dense::embedder::Embedder;
use marlowe_memory::rerank::{CrossEncoder, RerankProvider, SHIPPED_THREADS};
use marlowe_memory::retrieve::RERANK_BUDGET;
use marlowe_memory::Coverage;

pub use adapter::{Adapter, RerankSettings};

pub const USAGE: &str = "\
marlowe --eval-adapter --profile-root <DIR> --embedder-model <DIR> --reranking <off|DIR>
        [--embedding-cache <DIR>] [--embedder-workers <N>] [--dump-gate-features <FILE>]
        [--rerank-threads <N>] [--rerank-batch <on|off>] [--injection-coverage <declared|full>]

  --profile-root        fresh empty directory for this spawn's journal (required)
  --embedder-model      models/jina-embeddings-v2-small-en (required; sha256-pinned)
  --reranking           models/ms-marco-MiniLM-L-2-v2-ft-session-j, or `off` (required)
  --embedding-cache     content-addressed vector cache, outside the profile root
  --embedder-workers    ONNX sessions for batch embedding; throughput only (default min(cpus, 8))
  --dump-gate-features  one NDJSON row per scored candidate (truncated on open)
  --rerank-threads      cross-encoder intra-op threads (default 1, the measured value)
  --rerank-batch        one forward pass per slate (default off on CPU, the measured value)
  --injection-coverage  `declared` ships; `full` is a measurement arm stamped on the wire
";

/// Parsed `--eval-adapter` flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvalArgs {
    pub profile_root: PathBuf,
    pub embedder_model: PathBuf,
    /// `None` = explicit `--reranking off`.
    pub reranking: Option<PathBuf>,
    pub embedding_cache: Option<PathBuf>,
    pub embedder_workers: Option<usize>,
    pub dump_gate_features: Option<PathBuf>,
    pub rerank_threads: usize,
    pub rerank_batch: Option<bool>,
    pub coverage: Coverage,
}

/// Parse the flags after `--eval-adapter`. Unknown flags are refused: the old harness's
/// `--fit-mode`, `--rerank-plan` and provider flags no longer exist, and silently ignoring one
/// would label a run with a configuration it did not use.
pub fn parse(args: &[String]) -> Result<EvalArgs, String> {
    let mut it = args.iter();
    let (mut profile_root, mut embedder_model, mut reranking) = (None, None, None);
    let (mut embedding_cache, mut embedder_workers, mut dump) = (None, None, None);
    let (mut rerank_threads, mut rerank_batch, mut coverage) = (SHIPPED_THREADS, None, Coverage::Declared);
    while let Some(flag) = it.next() {
        let mut value = || -> Result<&String, String> {
            it.next().filter(|v| !v.starts_with("--")).ok_or(format!("{flag} requires a value"))
        };
        match flag.as_str() {
            "--profile-root" => profile_root = Some(PathBuf::from(value()?)),
            "--embedder-model" => embedder_model = Some(PathBuf::from(value()?)),
            "--reranking" => {
                let v = value()?;
                reranking = Some((v != "off").then(|| PathBuf::from(v)));
            }
            "--embedding-cache" => embedding_cache = Some(PathBuf::from(value()?)),
            "--embedder-workers" => embedder_workers = Some(positive(flag, value()?)?),
            "--dump-gate-features" => dump = Some(PathBuf::from(value()?)),
            "--rerank-threads" => rerank_threads = positive(flag, value()?)?,
            "--rerank-batch" => {
                rerank_batch = Some(match value()?.as_str() {
                    "on" => true,
                    "off" => false,
                    other => return Err(format!("--rerank-batch takes `on` or `off`, got {other:?}")),
                })
            }
            "--injection-coverage" => {
                let v = value()?;
                coverage = Coverage::parse(v)
                    .ok_or(format!("--injection-coverage takes `declared` or `full`, got {v:?}"))?;
            }
            other => return Err(format!("unknown --eval-adapter flag {other:?}")),
        }
    }
    Ok(EvalArgs {
        profile_root: profile_root.ok_or("--profile-root is required and has no default")?,
        embedder_model: embedder_model.ok_or("--embedder-model is required and has no default")?,
        reranking: reranking.ok_or(
            "--reranking is required and has no default; pass `off` or the cross-encoder directory",
        )?,
        embedding_cache,
        embedder_workers,
        dump_gate_features: dump,
        rerank_threads,
        rerank_batch,
        coverage,
    })
}

fn positive(flag: &str, v: &str) -> Result<usize, String> {
    v.parse::<usize>()
        .ok()
        .filter(|n| *n >= 1)
        .ok_or(format!("{flag} must be a positive integer, got {v:?}"))
}

/// Run the adapter until the harness closes stdin. Exit 2 on bad flags, 1 on a failed start.
pub fn run(args: &[String]) -> Result<i32, String> {
    let args = match parse(args) {
        Ok(a) => a,
        Err(e) => {
            eprint!("{USAGE}");
            eprintln!("error: {e}");
            return Ok(2);
        }
    };
    let workers = args
        .embedder_workers
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get().min(8)));
    let embedder = match Embedder::load(&args.embedder_model, workers, args.embedding_cache.as_deref()) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("marlowe: {e}");
            return Ok(1);
        }
    };
    eprintln!("marlowe: embedder {} ({workers} worker(s))", embedder.provider().name());
    let encoder = match &args.reranking {
        Some(dir) => match CrossEncoder::load_with_threads(dir, args.rerank_threads) {
            Ok(e) => Some(e),
            Err(e) => {
                eprintln!("marlowe: {e}");
                return Ok(1);
            }
        },
        None => None,
    };
    let provider = encoder.as_ref().map_or(RerankProvider::Cpu, |e| e.provider());
    let batched = args.rerank_batch.unwrap_or_else(|| provider.default_batching());
    match &encoder {
        Some(_) => eprintln!(
            "marlowe: rerank {} depth {RERANK_BUDGET}, {} thread(s), batched {batched}",
            provider.name(),
            args.rerank_threads
        ),
        None => eprintln!("marlowe: rerank off (explicit)"),
    }
    let mut adapter = match Adapter::start(
        &args.profile_root,
        embedder,
        RerankSettings { encoder, batched },
        args.coverage,
        args.dump_gate_features.as_deref(),
    ) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("marlowe: could not start against {}: {e}", args.profile_root.display());
            return Ok(1);
        }
    };
    let stdout = io::stdout();
    if let Err(e) = adapter.run(BufReader::new(io::stdin()), stdout.lock()) {
        eprintln!("marlowe: transport failure: {e}");
        return Ok(1);
    }
    // §4.0.6: stdin closed, everything flushed, exit 0.
    Ok(0)
}

#[cfg(test)]
mod tests;
