//! The embedder: pinned model, pinned vocabulary, pinned runtime, no defaults.
//!
//! Every load-time refusal here follows CLAUDE.md's rule — *prefer a load-time error to a
//! sensible default* — and each one guards a failure that is **silent** rather than loud. A
//! wrong model, a wrong vocabulary, or a wrong sequence length all produce a 512-dimension unit
//! vector that scores, ranks, and yields a number. None of them crashes.
//!
//! **Threads are pinned to 1 inside the graph**, and parallelism is at the *text* level: each
//! forward pass runs single-threaded on one worker and results are reassembled by input index.
//! `docs/design/spike-2026-08-04-embedder.md` measured that this makes worker count irrelevant
//! to output — identical digests at 1, 2 and 8 workers, on both engines and both models. That is
//! what lets throughput be tuned without touching a published number.

use std::path::{Path, PathBuf};

use ort::execution_providers::{CPUExecutionProvider, CUDAExecutionProvider};
use ort::session::builder::GraphOptimizationLevel;
use ort::session::Session;
use ort::value::Value;
use sha2::{Digest, Sha256};

use crate::cue::dense::cache::{CacheIdentity, EmbeddingCache, EMBEDDER_VERSION};
use crate::cue::dense::tokenizer::{encode, Vocab};
use crate::cue::dense::vram::Probe;
use crate::cue::dense::{mean_pool_and_normalize, DIMENSIONS, MAX_SEQ_LEN};

/// `jinaai/jina-embeddings-v2-small-en` at revision `44e7d1d6…`, `model.onnx`.
///
/// Pinned here as well as in `tools/fetch_model.py` deliberately: the fetcher guards the
/// download, this guards the *load*. A file swapped after download passes the first and is
/// caught by the second.
pub const MODEL_SHA256: &str = "974fdefe71fc9889258f569132b35acae6278874c8d09dbdf7806d23ad0b4497";

/// The same revision's `vocab.txt`.
pub const VOCAB_SHA256: &str = "109753d618dbb576a35112f9c20ef35cf3517d46106175bcf010c986a4bef1df";

pub const MODEL_FILE: &str = "model.onnx";
pub const VOCAB_FILE: &str = "vocab.txt";

/// Which execution provider computed a vector.
///
/// **A different execution provider is a DIFFERENT SCORER — ADR-015, and it is why this is an enum
/// on the loaded object rather than a build flag.** The value reached
/// [`crate::cue::dense::cache::CacheIdentity`], so a CPU-computed vector and a CUDA-computed vector
/// can never share a cache key, and it is reported by [`Embedder::provider`] so a run cannot be
/// labelled with a provider it did not use.
///
/// **What a `Cuda` value licenses is narrow and must not be overstated.** It means *a CUDA session
/// constructed and registration was not allowed to fall back silently*. It does **not** mean every
/// node ran on the GPU: M0c Session L measured **13.6% of nodes still on CPU** — all shape and
/// index ops — under a successfully registered CUDA session, and `ort` 2.0.0-rc.10 exposes no node
/// placement at all. There is no way to assert the stronger claim from here, so do not write a
/// comment that makes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbedProvider {
    Cpu,
    Cuda,
}

impl EmbedProvider {
    /// The ONNX Runtime provider name. This string is in the cache namespace, so it is an
    /// identity, not a label — do not "tidy" it.
    pub fn name(self) -> &'static str {
        match self {
            EmbedProvider::Cpu => "CPUExecutionProvider",
            EmbedProvider::Cuda => "CUDAExecutionProvider",
        }
    }

    pub fn parse(value: &str) -> Option<ProviderChoice> {
        match value {
            "cpu" => Some(ProviderChoice::Cpu),
            "cuda" => Some(ProviderChoice::Cuda),
            "auto" => Some(ProviderChoice::Auto),
            _ => None,
        }
    }
}

/// What the caller asked for, as distinct from what was obtained.
///
/// The two are deliberately different types. `Auto` may resolve to either provider depending on
/// what the machine has free at that instant, and the whole hazard this file guards is a run that
/// *asked* for one thing, *got* another, and reported the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderChoice {
    /// Use the GPU if a CUDA session constructs and there is device memory for it; otherwise CPU.
    /// **Never fails over to a whole-run error** — a shared card that filled up is a throughput
    /// problem, not a correctness one.
    Auto,
    /// CPU, whatever the machine has. Every number published by this project before 2026-08-17 was
    /// taken here.
    Cpu,
    /// CUDA, and **fail loudly if it cannot be had**. For measurement: a CUDA cell that silently
    /// ran on CPU is the Session G failure verbatim.
    Cuda,
}

/// How many GPU sessions were opened, and why not more.
///
/// Returned rather than logged so a caller can assert on it. `Embedder::plan` is read by
/// `examples/embed_provider_bench.rs` and by `tests/embedder_provider.rs`; it is not a declared
/// control with no reader.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderPlan {
    pub provider: EmbedProvider,
    pub workers: usize,
    /// Workers asked for. Lower than `workers` means device memory, not a request, set the width.
    pub requested: usize,
    /// Free device bytes at the first read, or `None` when there is no readable device.
    pub free_at_load: Option<u64>,
    /// Measured bytes one warmed GPU session cost, when that could be measured.
    pub session_cost: Option<u64>,
    /// One line, in English, saying which branch was taken. Reported, never parsed.
    pub reason: String,
}

#[derive(Debug, thiserror::Error)]
pub enum EmbedError {
    #[error(
        "{path} does not exist. The embedding model is never vendored ({dir} is gitignored); \
         fetch it with `python tools/fetch_model.py`. There is no fallback model and no default \
         path"
    )]
    Missing { path: PathBuf, dir: String },

    #[error("reading {path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },

    #[error(
        "{path} hashes to {found}, the pinned digest is {expected}. This is a different file \
         than the one every number in runs/ was produced with. A different model still embeds, \
         still scores and still ranks -- it only moves the number, which is why this is a \
         refusal rather than a warning. See DECISIONS.md ADR-004"
    )]
    DigestMismatch { path: PathBuf, found: String, expected: &'static str },

    #[error("{path}: {source}")]
    Vocab { path: PathBuf, source: crate::cue::dense::tokenizer::VocabError },

    #[error("loading the ONNX session from {path}: {source}")]
    Session { path: PathBuf, source: Box<ort::Error> },

    #[error(
        "the CUDA runtime libraries could not be located: {reason}. \
         See crates/marlowe-memory/src/cue/dense/cuda_libs.rs"
    )]
    CudaLibs { reason: String },

    #[error("running the ONNX graph: {source}")]
    Run { source: Box<ort::Error> },

    #[error(
        "the graph returned a hidden state of {found} values for {tokens} tokens, which is not \
         a multiple of {DIMENSIONS}. The model's output dimension disagrees with this build"
    )]
    ShapeDisagrees { found: usize, tokens: usize },

    #[error(transparent)]
    Cache(#[from] crate::cue::dense::cache::CacheError),
}

/// Did this ORT error come from failing to **load a library**, as opposed to anything else a
/// session can fail at?
///
/// Matched on text because `ort` 2.0.0-rc.10 surfaces the runtime's message as a string and exposes
/// no code for it. That is brittle and it is bounded: the only consequence of a miss is that an
/// advisory sentence is not appended, and the only consequence of a false positive is that it is
/// appended where it does not apply. **It never changes whether the load succeeds.**
pub(crate) fn is_library_load_failure(err: &ort::Error) -> bool {
    let text = err.to_string();
    text.contains("Error 126") || text.contains("which is missing")
}

fn sha256_file(path: &Path) -> Result<String, EmbedError> {
    let bytes = std::fs::read(path).map_err(|source| EmbedError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(format!("{:x}", hasher.finalize()))
}

fn verify(path: &Path, expected: &'static str, dir: &Path) -> Result<String, EmbedError> {
    if !path.exists() {
        return Err(EmbedError::Missing {
            path: path.to_path_buf(),
            dir: dir.display().to_string(),
        });
    }
    let found = sha256_file(path)?;
    if found != expected {
        return Err(EmbedError::DigestMismatch {
            path: path.to_path_buf(),
            found,
            expected,
        });
    }
    Ok(found)
}

/// The dense cue's embedder.
///
/// No `Debug`: an ONNX `Session` has none, and deriving one by hand would print a model digest
/// and a path into whatever log caught it. `identity()` is the deliberate, minimal alternative.
pub struct Embedder {
    /// One session per worker. `Session::run` takes `&mut self`, and sharing one session across
    /// threads would serialize the very thing the workers exist to parallelize.
    ///
    /// **Every session here uses the SAME execution provider.** See [`Embedder::load_with_provider`]
    /// for why a mixed set is refused rather than built.
    sessions: Vec<Session>,
    vocab: Vocab,
    cache: Option<EmbeddingCache>,
    truncated: u64,
    plan: ProviderPlan,
}

impl Embedder {
    /// Load and verify **on CPU, always**. The fixed-provider constructor.
    ///
    /// `workers` is a throughput knob and provably not a quality knob — see the module docs.
    /// `cache_dir` is `Option` because the cache is a *tool-side* accelerator; the shipping
    /// retrieval path does not need one and must not depend on one existing.
    ///
    /// # This did NOT move to `Auto` when the product default did, and that is deliberate
    ///
    /// **ADR-044 flipped the PRODUCT's default to `auto`.** It did not flip this one, because this
    /// is what the reference measurements load through, and a reference reading whose provider is
    /// decided by how much VRAM happened to be free is not a reference reading.
    /// `tests/embedding_reference.rs` labels its result `cpu`, asserts `MAX_ABS_DIFF = 1e-4`
    /// against HuggingFace's own output, and separately measures CUDA at a *provisional tripwire*
    /// because CUDA misses that tolerance by ~500x across the distribution. Were this `Auto`, the
    /// row labelled `cpu` would silently become whichever scorer the card allowed — the
    /// mislabelling family, applied to the one instrument that can detect it.
    ///
    /// So the label is enforced, not assumed: that test asserts `provider() == EmbedProvider::Cpu`
    /// and fails if this line changes. The product goes through [`Embedder::load_with_provider`],
    /// which is what `--embedder-provider` drives.
    pub fn load(
        model_dir: &Path,
        workers: usize,
        cache_dir: Option<&Path>,
    ) -> Result<Self, EmbedError> {
        // Fixed CPU, so the reserve is irrelevant and `None` states that rather than pretending
        // to yield to something this path can never contend with.
        Self::load_with_provider(
            model_dir,
            workers,
            cache_dir,
            ProviderChoice::Cpu,
            Probe::Device,
            crate::cue::dense::vram::Reserve::None,
        )
    }

    /// Load at an explicit provider choice and an explicit device-memory probe.
    ///
    /// # The provider is uniform across every session, and that is a correction to the brief
    ///
    /// The obvious reading of "put the remainder on CPU" is a *mixed* set — k CUDA sessions and
    /// `workers - k` CPU ones. **That is refused here, and the reason is a property this file
    /// already claims.** [`Embedder::embed_batch`] splits a batch contiguously across sessions, so
    /// with a mixed set the vector a text receives depends on *which worker it landed on*, which
    /// depends on the batch length and on how much VRAM happened to be free at load. The module
    /// header states that worker count is "a throughput knob and provably not a quality knob", and
    /// `tests/embedding_reference.rs::embedding_is_bit_identical_across_calls_and_worker_counts`
    /// enforces it. A mixed set breaks both, and it breaks them *as a function of machine state* —
    /// `marlowe-eval repro` would compare two spawns that split differently. The cache cannot save
    /// it either: one `Embedder` has one [`CacheIdentity`], so two providers behind one identity is
    /// the stale-vector failure with the provider as the stale field.
    ///
    /// So device memory sets the **width** of a GPU embedder, never the composition of a mixed one,
    /// and the fallback is still per-session in the way that matters: a card too full for eight
    /// sessions yields fewer, and a card too full for one yields CPU. **Nothing here fails the run.**
    ///
    /// # How the width is derived, with no hardcoded VRAM number and no hardcoded worker count
    ///
    /// 1. `probe` reads free device memory. `None` — no readable device — is CPU, full stop.
    /// 2. A session's cost has a **floor computed from things this build already knows**: the size
    ///    of `model.onnx` on disk plus the ALiBi relative-distance matrix, `8 x N x N` int64 at
    ///    [`MAX_SEQ_LEN`]. That is the quadratic term `STATE.md` measured at 23x between 8192 and
    ///    1024; it is transient per inference and it is per session.
    /// 3. The **headroom rule is one whole spare session**, not a constant: the loader opens
    ///    another only while what is free would still hold one more after it. The card is shared —
    ///    `llama-server` holds ~11.5 GB of 16.4 GB here — and the reading is an instant, not a
    ///    reservation, so the margin has to be big enough for someone else's allocation.
    /// 4. After the first session is opened **and warmed at `MAX_SEQ_LEN`**, the floor is replaced
    ///    by the *measured* delta when that is larger. Warming matters: ORT's CUDA arena allocates
    ///    on first run, so a cost read at construction reads a fraction of the real one.
    /// 5. Every subsequent decision **re-reads the device** rather than spending down a budget
    ///    computed once. A budget computed at startup and trusted afterwards is a stale artifact.
    ///
    /// A construction failure under [`ProviderChoice::Auto`] falls back — to CPU if it was the
    /// first session, to the sessions already open if it was not. Under [`ProviderChoice::Cuda`] it
    /// is an error, because a measurement cell that quietly ran on CPU is Session G verbatim.
    pub fn load_with_provider(
        model_dir: &Path,
        workers: usize,
        cache_dir: Option<&Path>,
        choice: ProviderChoice,
        probe: Probe,
        reserve: crate::cue::dense::vram::Reserve<'_>,
    ) -> Result<Self, EmbedError> {
        let model_path = model_dir.join(MODEL_FILE);
        let vocab_path = model_dir.join(VOCAB_FILE);

        let model_digest = verify(&model_path, MODEL_SHA256, model_dir)?;
        let vocab_digest = verify(&vocab_path, VOCAB_SHA256, model_dir)?;

        let vocab_text = std::fs::read_to_string(&vocab_path).map_err(|source| EmbedError::Io {
            path: vocab_path.clone(),
            source,
        })?;
        let vocab = Vocab::parse(&vocab_text, &vocab_path.display().to_string())
            .map_err(|source| EmbedError::Vocab { path: vocab_path, source })?;

        let requested = workers.max(1);
        let model_bytes = std::fs::metadata(&model_path).map(|m| m.len()).unwrap_or(0);

        let (sessions, plan) = match choice {
            ProviderChoice::Cpu => (
                Self::cpu_sessions(&model_path, requested)?,
                ProviderPlan {
                    provider: EmbedProvider::Cpu,
                    workers: requested,
                    requested,
                    free_at_load: None,
                    session_cost: None,
                    reason: "CPU was asked for explicitly".to_string(),
                },
            ),
            ProviderChoice::Cuda => {
                // No probe gate and no fallback: the caller said CUDA, so a failure to construct
                // must surface as an error rather than as a quietly slower run.
                let mut sessions = Vec::with_capacity(requested);
                for _ in 0..requested {
                    sessions.push(Self::session(&model_path, EmbedProvider::Cuda)?);
                }
                let plan = ProviderPlan {
                    provider: EmbedProvider::Cuda,
                    workers: requested,
                    requested,
                    free_at_load: probe.free_bytes(),
                    session_cost: None,
                    reason: "CUDA was asked for explicitly; no VRAM budget was applied".to_string(),
                };
                (sessions, plan)
            }
            ProviderChoice::Auto => {
                Self::auto_sessions(&model_path, requested, model_bytes, probe, reserve)?
            }
        };

        let cache = match cache_dir {
            Some(dir) => Some(EmbeddingCache::open(
                dir,
                CacheIdentity::new(
                    model_digest.clone(),
                    vocab_digest.clone(),
                    plan.provider.name(),
                ),
            )?),
            None => None,
        };

        Ok(Self { sessions, vocab, cache, truncated: 0, plan })
    }

    fn cpu_sessions(model_path: &Path, workers: usize) -> Result<Vec<Session>, EmbedError> {
        let mut sessions = Vec::with_capacity(workers);
        for _ in 0..workers {
            sessions.push(Self::session(model_path, EmbedProvider::Cpu)?);
        }
        Ok(sessions)
    }

    /// The floor on what one warmed GPU session costs, in bytes.
    ///
    /// Both terms are read from this build rather than chosen: the graph's own size on disk, and
    /// the ALiBi relative-distance matrix this export materialises — `Abs(Range(0,N) - Range(0,N)T)`
    /// expanded to `[8, N, N]` at int64, so `8 * N * N * 8` bytes, quadratic in [`MAX_SEQ_LEN`].
    /// `STATE.md` measured that term at 4.29 GB when `MAX_SEQ_LEN` was 8192 and 64 MB at 1024.
    ///
    /// It is a **floor**, not an estimate: the measured delta replaces it whenever the measurement
    /// is larger. It exists so that a device reading which does not move — the `Fixed` probe, or a
    /// concurrent free by another process — cannot be read as "a session costs nothing", which
    /// would let the loop open sessions without bound.
    fn session_cost_floor(model_bytes: u64) -> u64 {
        let alibi = 8u64 * MAX_SEQ_LEN as u64 * MAX_SEQ_LEN as u64 * 8;
        model_bytes + alibi
    }

    fn auto_sessions(
        model_path: &Path,
        requested: usize,
        model_bytes: u64,
        probe: Probe,
        reserve: crate::cue::dense::vram::Reserve<'_>,
    ) -> Result<(Vec<Session>, ProviderPlan), EmbedError> {
        let floor = Self::session_cost_floor(model_bytes);
        let cpu = |reason: String, free: Option<u64>| -> Result<_, EmbedError> {
            Ok((
                Self::cpu_sessions(model_path, requested)?,
                ProviderPlan {
                    provider: EmbedProvider::Cpu,
                    workers: requested,
                    requested,
                    free_at_load: free,
                    session_cost: None,
                    reason,
                },
            ))
        };

        let Some(free) = probe.free_bytes() else {
            return cpu("no readable NVIDIA device (nvidia-smi absent or silent)".to_string(), None);
        };
        // **TIER 3 YIELDS TO TIER 1 -- ADR-045 SS4, applied to the component that made the defect
        // visible.** ADR-044 shipped this loader reading `free` and taking what was there; on an
        // idle card that was 8 sessions and 4,647 MB, and the language model has no CPU fallback
        // to take when it arrives to find the card full. `usable` is what is spare AFTER tier 1's
        // claim. Every decision below re-reads the device and subtracts the same reserve, so the
        // width narrows as the card fills rather than being computed once and trusted.
        let reserved = reserve.read();
        let usable = free.saturating_sub(reserved.bytes);
        // The same one-spare-session rule that governs every later decision, applied to the first:
        // opening a session that leaves no room for a second is how a shared card gets filled.
        if usable < floor.saturating_mul(2) {
            return cpu(
                format!(
                    "{} MB usable ({} MB free, {}) is under the {} MB one-spare-session floor for \
                     this graph",
                    usable / (1024 * 1024),
                    free / (1024 * 1024),
                    reserved.reason,
                    floor.saturating_mul(2) / (1024 * 1024)
                ),
                Some(free),
            );
        }

        let mut first = match Self::session(model_path, EmbedProvider::Cuda) {
            Ok(s) => s,
            Err(e) => {
                // The Session G case, and the reason `error_on_failure` is on the builder: CUDA
                // reported available, failed to CREATE (a missing `cublasLt64_12.dll` there), and
                // ORT would otherwise have registered CPU and scored happily. Here it is a visible
                // fallback with the driver's own message attached.
                return cpu(format!("a CUDA session did not construct: {e}"), Some(free));
            }
        };
        // Warm at MAX_SEQ_LEN before measuring. The arena allocates on first run, so an unwarmed
        // delta reads a fraction of the real cost -- and MAX_SEQ_LEN is where the quadratic term
        // is largest, which is the number the budget has to survive.
        let _ = Self::forward(&mut first, &vec![0u32; MAX_SEQ_LEN]);
        let after = probe.free_bytes().unwrap_or(free);
        let cost = free.saturating_sub(after).max(floor);

        let mut sessions = vec![first];
        // A ledger beside the device reading, and the decision takes the MINIMUM of the two. The
        // device reading is the truth on a real card; the ledger is what makes the same code path
        // testable with a `Fixed` probe, whose reading by construction does not move.
        let mut ledger = usable.saturating_sub(cost);
        let mut stopped = String::new();

        while sessions.len() < requested {
            // The reserve is subtracted on EVERY re-read, not only the first. A reserve applied
            // once and then forgotten is a stale budget, which is the family this loop's re-read
            // was written to avoid in the first place.
            let device = probe.free_bytes().unwrap_or(0).saturating_sub(reserved.bytes);
            let available = device.min(ledger);
            if available < cost.saturating_mul(2) {
                stopped = format!(
                    "device memory: {} MB usable would not hold another session plus a spare \
                     ({} MB each)",
                    available / (1024 * 1024),
                    cost / (1024 * 1024)
                );
                break;
            }
            match Self::session(model_path, EmbedProvider::Cuda) {
                Ok(mut s) => {
                    let _ = Self::forward(&mut s, &vec![0u32; MAX_SEQ_LEN]);
                    sessions.push(s);
                    ledger = ledger.saturating_sub(cost);
                }
                Err(e) => {
                    stopped = format!("session {} did not construct: {e}", sessions.len() + 1);
                    break;
                }
            }
        }

        let opened = sessions.len();
        let reason = if opened == requested {
            format!("CUDA, all {requested} requested sessions opened; {}", reserved.reason)
        } else {
            format!("CUDA, {opened} of {requested} sessions; {stopped}; {}", reserved.reason)
        };
        Ok((
            sessions,
            ProviderPlan {
                provider: EmbedProvider::Cuda,
                workers: opened,
                requested,
                free_at_load: Some(free),
                session_cost: Some(cost),
                reason,
            },
        ))
    }

    /// Which provider actually served this embedder, and how the width was decided.
    pub fn plan(&self) -> &ProviderPlan {
        &self.plan
    }

    pub fn provider(&self) -> EmbedProvider {
        self.plan.provider
    }

    /// Build one session on one provider. **The provider is REQUESTED, never left to ORT.**
    ///
    /// Until 2026-08-17 this function called no `with_execution_providers` at all, so the shipped
    /// embedder was CPU *by omission* — nothing asked for anything, and ORT quietly served CPU. The
    /// omission was invisible in exactly the way this project keeps paying for: there was no wrong
    /// value to find, only an absent call.
    ///
    /// # `error_on_failure()` is the load-bearing part, on BOTH arms
    ///
    /// Session G's Python spike found CUDA listed as *available* while failing to **create** on a
    /// missing `cublasLt64_12.dll`, after which ORT registered CPU and scored happily — producing a
    /// "GPU" figure within 1% of the 1-thread CPU one. `error_on_failure` turns that into a hard
    /// error, which is what makes [`EmbedProvider::Cuda`] on a loaded [`Embedder`] mean something.
    /// [`ProviderChoice::Auto`] then converts that error into a *visible* fallback with the
    /// driver's message attached; what it never becomes is a silent one.
    ///
    /// The CPU arm registers explicitly too. It cannot fail, and that is not the point: the point
    /// is that CPU is now a decision with a line of code behind it rather than the absence of one.
    ///
    /// # What a constructed CUDA session does NOT establish
    ///
    /// `ort` 2.0.0-rc.10 exposes no way to enumerate a constructed session's active providers and
    /// no node placement, and M0c Session L measured **13.6% of nodes still running on CPU** under
    /// a registered CUDA session — all shape and index ops, no matmuls. So this answers *"did a
    /// CUDA session construct"* and never *"did every node run on the GPU"*. There is no
    /// post-construction assertion here because there is nothing to assert against; do not add a
    /// comment claiming otherwise.
    fn session(model_path: &Path, provider: EmbedProvider) -> Result<Session, EmbedError> {
        // **Before ORT loads its provider DLL, not after.** The CUDA provider is loaded lazily by
        // the runtime and resolves `cublasLt64_12.dll` and friends through the process search path
        // at that moment, so this has to happen first or it has not happened at all. See
        // `cue::dense::cuda_libs` -- and note that a REFUSAL here is reported as such rather than
        // left to surface as Error 126, which names a library and not the misconfiguration.
        if provider == EmbedProvider::Cuda {
            if let Some(reason) = crate::cue::dense::cuda_libs::ensure_search_path().rejection() {
                return Err(EmbedError::CudaLibs { reason: reason.to_string() });
            }
        }
        Session::builder()
            .and_then(|b| match provider {
                EmbedProvider::Cpu => b.with_execution_providers([CPUExecutionProvider::default()
                    .build()
                    .error_on_failure()]),
                EmbedProvider::Cuda => b.with_execution_providers([
                    CUDAExecutionProvider::default().build().error_on_failure()
                ]),
            })
            .and_then(|b| b.with_intra_threads(1))
            .and_then(|b| b.with_inter_threads(1))
            // Pinned explicitly rather than left at the runtime's default: the default is a
            // property of the ONNX Runtime version, and a bump could re-fuse the graph and move
            // a published number with nothing in this repo changing.
            .and_then(|b| b.with_optimization_level(GraphOptimizationLevel::Level1))
            .and_then(|b| b.commit_from_file(model_path))
            .map_err(|source| {
                // **The sentence whose absence cost a session.** A provider-load failure with
                // nothing configured is the exact state that got read as "this machine has no
                // CUDA"; attaching the remedy to the error is what stops the next reader repeating
                // it. Narrow on purpose: only for CUDA, only when the runtime failed to LOAD a
                // library, and only when nothing was configured. A `bad allocation` is a real
                // out-of-memory and must not be relabelled as a missing-library problem.
                if provider == EmbedProvider::Cuda && is_library_load_failure(&source) {
                    if let Some(hint) = crate::cue::dense::cuda_libs::hint_when_unconfigured() {
                        return EmbedError::CudaLibs { reason: format!("{source} -- {hint}") };
                    }
                }
                EmbedError::Session {
                    path: model_path.to_path_buf(),
                    source: Box::new(source),
                }
            })
    }

    /// Can a CUDA session be **constructed** on this machine, on the **shipped graph**?
    ///
    /// A construction, never an availability list — the same probe `CrossEncoder::cuda_available`
    /// makes, for the same reason, and on the graph the product actually loads. Probing with a
    /// different graph measures the availability of something else.
    ///
    /// Callers get a boolean and nothing more. It does not say how many sessions fit, and it does
    /// not say that any node ran on the GPU.
    pub fn cuda_available(model_dir: &Path) -> bool {
        let model_path = model_dir.join(MODEL_FILE);
        model_path.exists() && Self::session(&model_path, EmbedProvider::Cuda).is_ok()
    }

    pub fn workers(&self) -> usize {
        self.sessions.len()
    }

    /// How many texts were truncated at [`MAX_SEQ_LEN`]. Reported, never assumed to be zero.
    pub fn truncated(&self) -> u64 {
        self.truncated
    }

    pub fn cache_stats(&self) -> Option<(u64, u64)> {
        self.cache.as_ref().map(|c| (c.hits(), c.misses()))
    }

    /// Embed one text. Cache-aware.
    pub fn embed(&mut self, text: &str) -> Result<Vec<f32>, EmbedError> {
        if let Some(cache) = self.cache.as_mut() {
            if let Some(hit) = cache.get(text) {
                return Ok(hit.to_vec());
            }
        }
        let encoded = encode(&self.vocab, text, MAX_SEQ_LEN);
        if encoded.truncated {
            self.truncated += 1;
        }
        let vector = Self::forward(&mut self.sessions[0], &encoded.input_ids)?;
        if let Some(cache) = self.cache.as_mut() {
            cache.put(text, &vector)?;
        }
        Ok(vector)
    }

    /// Embed many texts, in input order, using every worker.
    ///
    /// Cache lookups happen first and single-threaded, so a fully warm batch does no inference
    /// at all — which is the property ADR-004's model choice depends on
    /// (`runs/session-c/PREREGISTRATION-model.json`).
    pub fn embed_batch(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        let mut out: Vec<Option<Vec<f32>>> = vec![None; texts.len()];
        let mut pending: Vec<usize> = Vec::new();

        for (index, text) in texts.iter().enumerate() {
            match self.cache.as_mut().and_then(|c| c.get(text)) {
                Some(hit) => out[index] = Some(hit.to_vec()),
                None => pending.push(index),
            }
        }

        if !pending.is_empty() {
            let encoded: Vec<_> = pending
                .iter()
                .map(|index| encode(&self.vocab, &texts[*index], MAX_SEQ_LEN))
                .collect();
            self.truncated += encoded.iter().filter(|e| e.truncated).count() as u64;

            // Contiguous split across workers, each returning its results in order. Because the
            // splits are contiguous and taken in worker order, concatenating them reconstructs
            // the input order exactly -- no scatter, no shared mutable state, no unsafe.
            //
            // Worker count therefore cannot reorder or alter anything, which is the property
            // docs/design/spike-2026-08-04-embedder.md measured at 1, 2 and 8 workers.
            let workers = self.sessions.len();
            let per_worker = encoded.len().div_ceil(workers);
            let mut computed: Vec<Result<Vec<f32>, EmbedError>> = Vec::with_capacity(encoded.len());

            std::thread::scope(|scope| {
                let mut handles = Vec::new();
                let mut remaining: &[crate::cue::dense::tokenizer::Encoded] = &encoded;
                for session in self.sessions.iter_mut() {
                    let take = per_worker.min(remaining.len());
                    let (mine, rest) = remaining.split_at(take);
                    remaining = rest;
                    handles.push(scope.spawn(move || {
                        mine.iter()
                            .map(|e| Self::forward(session, &e.input_ids))
                            .collect::<Vec<_>>()
                    }));
                }
                for handle in handles {
                    computed.extend(handle.join().expect("embedding worker panicked"));
                }
            });

            for (position, index) in pending.iter().enumerate() {
                let vector = std::mem::replace(
                    &mut computed[position],
                    Err(EmbedError::ShapeDisagrees { found: 0, tokens: 0 }),
                )?;
                if let Some(cache) = self.cache.as_mut() {
                    cache.put(&texts[*index], &vector)?;
                }
                out[*index] = Some(vector);
            }
        }

        Ok(out.into_iter().map(|v| v.expect("every slot filled")).collect())
    }

    fn forward(session: &mut Session, input_ids: &[u32]) -> Result<Vec<f32>, EmbedError> {
        let tokens = input_ids.len();
        let ids: Vec<i64> = input_ids.iter().map(|v| *v as i64).collect();
        let mask: Vec<i64> = vec![1; tokens];
        let zeros: Vec<i64> = vec![0; tokens];

        let names: Vec<String> = session.inputs.iter().map(|i| i.name.clone()).collect();
        let mut inputs: Vec<(String, Value)> = Vec::with_capacity(names.len());
        for name in names {
            let data = if name.contains("attention") {
                &mask
            } else if name.contains("token_type") {
                &zeros
            } else {
                &ids
            };
            let tensor = Value::from_array(([1usize, tokens], data.clone()))
                .map_err(|source| EmbedError::Run { source: Box::new(source) })?;
            inputs.push((name, tensor.into_dyn()));
        }

        let outputs = session
            .run(inputs)
            .map_err(|source| EmbedError::Run { source: Box::new(source) })?;
        let (_, hidden) = outputs[0]
            .try_extract_tensor::<f32>()
            .map_err(|source| EmbedError::Run { source: Box::new(source) })?;

        if hidden.len() != tokens * DIMENSIONS {
            return Err(EmbedError::ShapeDisagrees { found: hidden.len(), tokens });
        }
        Ok(mean_pool_and_normalize(hidden, tokens))
    }

    /// What the cache keys on, for reporting. Never used to decide anything.
    ///
    /// **Deliberately still an associated function without `self`, so it does NOT carry the
    /// provider.** The provider is per-instance and lives on [`Embedder::plan`]; putting it here
    /// would mean a caller could print a provider this embedder is not using. The one place the
    /// provider must appear is the cache namespace, and it gets there from `plan.provider` at
    /// construction, not from this tuple.
    pub fn identity() -> (&'static str, &'static str, &'static str, usize, usize) {
        (MODEL_SHA256, VOCAB_SHA256, EMBEDDER_VERSION, MAX_SEQ_LEN, DIMENSIONS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .unwrap()
            .join("models")
            .join("jina-embeddings-v2-small-en")
    }

    #[test]
    fn a_missing_model_names_the_fetch_command_rather_than_defaulting() {
        let err = match Embedder::load(Path::new("no-such-directory"), 1, None) {
            Err(e) => e,
            Ok(_) => panic!("a missing model must not load"),
        };
        assert!(matches!(err, EmbedError::Missing { .. }), "{err}");
        assert!(err.to_string().contains("tools/fetch_model.py"), "{err}");
    }

    #[test]
    fn a_swapped_model_file_is_refused_at_load() {
        // The hazard the load-time digest exists for, and the reason it is checked here as well
        // as at download: a file replaced after fetching passes the fetcher and reaches this.
        let dir = std::env::temp_dir().join("marlowe-embedder-swapped");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(MODEL_FILE), b"not an onnx graph").unwrap();
        std::fs::write(dir.join(VOCAB_FILE), b"[PAD]\n").unwrap();

        let err = match Embedder::load(&dir, 1, None) {
            Err(e) => e,
            Ok(_) => panic!("a swapped model file must not load"),
        };
        assert!(matches!(err, EmbedError::DigestMismatch { .. }), "{err}");
        assert!(err.to_string().contains("ADR-004"), "{err}");
    }

    #[test]
    fn the_pinned_digests_match_the_fetched_model() {
        // Skips on a fresh clone, where models/ legitimately does not exist.
        let dir = model_dir();
        if !dir.join(MODEL_FILE).exists() {
            eprintln!("SKIP: run `python tools/fetch_model.py`");
            return;
        }
        assert_eq!(sha256_file(&dir.join(MODEL_FILE)).unwrap(), MODEL_SHA256);
        assert_eq!(sha256_file(&dir.join(VOCAB_FILE)).unwrap(), VOCAB_SHA256);
    }
}
