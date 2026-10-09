//! The cross-encoder rerank stage — Session H, re-pinned to the fine-tuned graph in Session K.
//!
//! A **rerank stage, not a cue.** The distinction is ADR-010's and it decides what this is capable
//! of. A cue produces a score that the gate arbitrates between; a fusion that only arbitrates
//! cannot beat the either-cue oracle, because it can only ever pick a candidate one of the cues
//! already ranked first — which is the cap Sessions D and E both shipped into. This stage computes
//! a **new** score over `(query, candidate text)` and can promote a candidate neither cue ranked
//! anywhere near the top, so its ceiling is *gold is in the slate handed to it*. That was
//! confirmed by measurement before this file was written, not assumed:
//! `runs/session-h/rerank-fit.json → adr_010_reach_check`.
//!
//! ## The shipped graph is the Session J fine-tune, and it is f32 — ADR-018, ADR-020
//!
//! Sessions B–I shipped `Xenova/ms-marco-MiniLM-L-2-v2` `onnx/model_int8.onnx`. What ships now is
//! that same architecture **domain-adapted on same-session hard negatives mined from the fit
//! split**, exported to f32: held-out R@1 `0.6026 → 0.6725`, `+0.0699`, discordant 38 (27 gained,
//! 11 lost), exact McNemar `p = 0.0139` with α attainable. It is the first change this project has
//! made to the scored path that is significant on held-out with the power to have detected an
//! effect.
//!
//! **The precision change from int8 to f32 removes a live hazard rather than adding one.** ADR-015
//! measured the shipped int8 graph as **shape-bound in every dimension**: bit-identical token ids
//! re-padded to a longer tensor moved the logit by a median 0.0109 and **padding alone flipped
//! top-1 in 15% of cases**, while all eight f32 graphs were invariant to `0.000000`. The shipped
//! f32 graph is re-verified — not inherited — at batch invariance `0.000000` and padding invariance
//! `0.000000`; see `runs/session-k/export-verification-*.json`.
//!
//! ## The batch is now the slate, and both of Session K's reasons were retired by measurement
//!
//! Session G's original reason for batch 1 was that **int8 batch invariance FAILED** at 0.037
//! logits for L-2. That reason died with the int8 graph. Session K replaced it with two others,
//! and **M0c Session L retired both — by measuring, not by arguing**:
//!
//! 1. *"Invariance is a per-graph measurement and is never inherited."* Still true, and still the
//!    rule. It is discharged by taking the measurement on **this** graph rather than by refusing to
//!    batch: `runs/session-l/batch-invariance-batch10.json`. Max `|batched − single|`
//!    **0.000000000** over **2,290 pairs** across all 229 held-out slates, **zero** order changes,
//!    **zero** top-1 changes — plus a sweep of **every batch size 1..10**, all `0.000000000`,
//!    because a pool with fewer survivors produces a shorter batch and measuring only at 10 would
//!    have left the production-reachable sizes resting on an argument.
//!
//!    Session K's read was taken at n = 8 on synthetic documents differing only by a trailing
//!    integer — near-identical lengths, so a length-heterogeneity effect inside the batch could not
//!    have appeared. Session L's slates span **2 to 5,241 word-pieces**.
//!
//! 2. *"Batching buys nothing here."* **This was wrong, and the profile is why.** The rerank is
//!    **90.49%** of the retrieval P95 (`runs/session-l/RESULT.md`) — ten sequential forwards at
//!    18.76 ms/pair. It is not a stage among stages; it is the budget. Everything else in the
//!    pipeline combined is under 10%.
//!
//! **The per-graph rule is now enforced in Rust, not just in a Python artifact.**
//! `tests/cross_encoder_reference.rs::batched_and_single_scoring_are_bit_identical` scores a slate
//! both ways through the shipped path and asserts bit-identical logits. A re-pin that breaks
//! invariance fails the build rather than silently scoring candidates against whoever shares their
//! batch — which is the hazard the batch-1 constant was standing in for.
//!
//! [`CrossEncoder::score_batch`] is the entry point and [`CrossEncoder::score`] delegates to it
//! with a one-element slice, so there is exactly **one** inference path. `MAX_BATCH` is the
//! measured envelope and a larger batch is **refused**, not silently run: the invariance number
//! covers 1..10 and nothing above it has been measured.
//!
//! ## `MAX_SEQ_LEN` is 256 and the 7.86% gold truncation is a PRICED DEFECT
//!
//! Not an unexamined constant. ADR-015 measured raising it to 512 as a **−0.0917 R@1 regression**
//! (`p = 0.0002`, α attainable): the cap is doing two opposing jobs, costing the 7.86% of gold
//! turns that do not fit and earning more back by capping how much score a long distractor can
//! accumulate. ADR-017 tested the only named route to raising it — explicit length normalization —
//! and that route is a **held-out null with power** on every cell. **Do not raise this without a
//! normalization term fitted against relevance rather than against the score.**
//!
//! ## Everything is pinned and nothing is defaulted
//!
//! Same rule as [`crate::cue::dense::embedder`], for the same reason: a wrong model, a wrong
//! vocabulary or a wrong sequence length all produce a plausible number rather than a crash. There
//! is deliberately **no table of accepted graphs** — a loader that accepts two graphs is a way for
//! a target string to name one scorer and measure another, which is the failure this project has
//! now recorded nine instances of.

use std::path::{Path, PathBuf};

use ort::execution_providers::{CPUExecutionProvider, CUDAExecutionProvider};
use ort::session::builder::GraphOptimizationLevel;
use ort::session::Session;
use ort::value::Value;
use sha2::{Digest, Sha256};

use crate::cue::dense::tokenizer::{encode_pair, EncodedPair, Vocab};

/// `ms-marco-MiniLM-L-2-v2-ft-session-j`, `model.onnx` — the Session J fine-tune, f32.
///
/// Pinned here so a file swapped after export is caught at *load*, not merely at training time.
/// The same digest appears in `runs/session-j/finetuned-manifest.json` and in
/// `runs/session-k/export-verification-ms-marco-MiniLM-L-2-v2-ft-session-j.json`, which is where
/// the five determinism checks were re-taken on the shipped graph — so this constant is what ties
/// the running binary to the published `0.6725`.
///
/// **The export gap is bounded, not closed.** There is no external authority for a model this
/// project trained, because this project is the publisher. What stands behind the graph is digest
/// pinning, torch-vs-ORT fixture agreement at 1e-6, and per-graph determinism / batch / padding
/// invariance. See ADR-018 and STATE.md's open-gaps section.
pub const MODEL_SHA256: &str = "9c222dac4315cfd2f33f2e865bb651a7a16bf532c11e55b7fb1a43bb041880c0";

/// The fine-tune's `tokenizer.json`, carried out of `cross-encoder/ms-marco-MiniLM-L-2-v2`.
///
/// **A different file than the Xenova export's, and the difference was measured rather than
/// waved through.** `vocab` (30522 entries), `normalizer`, `pre_tokenizer`, `post_processor`,
/// `decoder` and `added_tokens` are byte-identical between the two; they differ only in the
/// embedded `padding` and `truncation` blocks, which this crate implements itself in
/// [`encode_pair`] and never reads from the file. Confirmed empirically: token ids, attention
/// mask, token type ids and the truncation flag agree on all 8 reference cases across both
/// tokenizers. See `tests/fixtures/cross-encoder-reference-ft-session-j.json`.
pub const TOKENIZER_SHA256: &str =
    "0d3aef594edd5f9b53e7f814277a9171dc70ff93eb66bda6e01f7aa53997d963";

/// The graphs the GPU cascade fuses, **digest-pinned exactly as the shipped graph is**.
///
/// M0c Session M2, held-out, n = 229: RRF (k = 60) over these six, applied to the ten candidates
/// the shipped graph narrows a 30-wide slate down to, reads **R@3 0.8908** against the shipped
/// path's **0.8515** — +0.0393, McNemar 11 gained / 2 lost, **p = 0.0225**. `cond@3` 0.9062 →
/// 0.9107, input recall 0.9039 → 0.9782.
///
/// **The membership is a RULE, not a selection.** It is *every* graph trained on Session J's recipe
/// — same pairs file, same MarginMSE, same delta rule, same seed, same conversation fold — with no
/// subset chosen. That rule was fixed in `tools/cascade_squeeze.py`'s docstring before any model
/// loaded, and it is why this configuration is trustworthy: on fit it ranked **second**, behind
/// `ms-marco-MiniLM-L-4-v2-ft-w1` alone at 0.9432. Held-out, L-4 alone read **0.8690 — last**, below
/// the untouched baseline on R@1. The arm picked by looking at fit lost; the arm picked by a rule
/// won. Do not "improve" this list by scoring candidates on the fit split.
///
/// The `ft-m-*` family is excluded for a stated reason: different negative mining
/// (`deployed_top_k`), and arm B is on the record as fit 0.8253 → held-out 0.6812.
///
/// **The first entry is the narrower** and must remain the shipped graph, because the 30 → 10
/// narrowing was measured with it (retention 0.9956).
pub const FUSION_GRAPHS: &[(&str, &str)] = &[
    // The shipped graph, and the one that narrows. Its digest is MODEL_SHA256.
    (
        "ms-marco-MiniLM-L-2-v2-ft-session-j",
        "9c222dac4315cfd2f33f2e865bb651a7a16bf532c11e55b7fb1a43bb041880c0",
    ),
    (
        "ms-marco-MiniLM-L-6-v2-ft-session-j",
        "78dcc7c1834b2e0cfc67d58a2735b9bc27900f46d5b5ccada99e8ee610c724f6",
    ),
    (
        "ms-marco-MiniLM-L-2-v2-ft-w1",
        "cc4df68c30d319cab3d74c166f4de18c4ade7e8c1fccd1fbc1512bdffee58b96",
    ),
    (
        "ms-marco-MiniLM-L-4-v2-ft-w1",
        "f6324276380fbc6a6a34864c077a94d5227d9719d9d5dfbc80bcb8695078fddb",
    ),
    (
        "ms-marco-MiniLM-L-6-v2-ft-w1",
        "bbfb8a0831db882397c2b83f1c108e233320e1c257c923216357d2c48e25e050",
    ),
    (
        "ms-marco-MiniLM-L-12-v2-ft-w1",
        "b1a374f3cd2752af2143d4f01853282a9a1d4e7ee8137e82c5827291b26ccdd6",
    ),
];

/// Reciprocal-rank-fusion constant. Cormack et al.'s published default.
///
/// **Fixed before any number existed and never swept.** A `k` chosen after seeing a result is the
/// tunable knob this project has four fit→held-out collapses from.
pub const RRF_K: f32 = 60.0;

/// All fusion members share one tokenizer, so [`TOKENIZER_SHA256`] pins every one of them.
/// Verified against `runs/session-m0c-m/capacity-manifest.json`: every member records
/// `tokenizer.json` = `0d3aef59…`.
pub const MODEL_FILE: &str = "model.onnx";
pub const TOKENIZER_FILE: &str = "tokenizer.json";

/// The cascade's second opinion — the fuse stage of the pre-registered configuration.
///
/// **The narrow stage is always [`MODEL_SHA256`] (the shipped graph)**, because the 30 → 10
/// narrowing was measured with it (retention 0.9956). This constant names only the partner, and
/// its digest lives in [`FUSION_GRAPHS`], which is what [`CrossEncoder::load_fusion_member`]
/// enforces — there is deliberately no second way to spell either pin.
pub const CASCADE_FUSE_GRAPH: &str = "ms-marco-MiniLM-L-6-v2-ft-session-j";

/// The graph file the pre-Session-K builds loaded, named here **only** so a stale `--reranking`
/// path gets an error that says what happened instead of a bare "file not found".
const SUPERSEDED_INT8_FILE: &str = "model_int8.onnx";

/// Sequence length, fixed. See the module docs — this is a priced defect, not a free constant.
pub const MAX_SEQ_LEN: usize = 256;

/// The largest batch whose invariance has been **measured** on the shipped graph.
///
/// Not a tuning parameter and not a capacity limit — it is the edge of the evidence. The sweep in
/// `runs/session-l/batch-invariance-batch10.json` covers 1..10 at `0.000000000`; 11 has never been
/// run. [`CrossEncoder::score_batch`] refuses above this rather than extrapolating, because "it was
/// invariant at 10 so it is invariant at 32" is exactly the inherited-measurement reasoning the
/// per-graph rule exists to stop.
///
/// It equals `retrieve::RERANK_BUDGET` by construction, and a test asserts the two agree — if the
/// budget ever rises, the invariance sweep has to be re-run to that number first.
pub const MAX_BATCH: usize = 10;

/// The intra-op thread count every published number in this project was measured at.
///
/// ADR-003 sizes against a 1-vCPU VPS. That target is what forced this to 1, and M0c Session L
/// records that the target itself is under question — so this constant names the *measured*
/// configuration rather than an assumed one, and `load_with_threads` exists so the alternative can
/// be measured instead of argued.
pub const SHIPPED_THREADS: usize = 1;

/// Which execution provider scores the slate.
///
/// **Not a fork: an accelerator.** Both providers return the same RANKING -- M0c Session L measured
/// 0 of 229 held-out slates reordered, 0 top-1 changes, and R@1 identical at 0.6725. They differ in
/// latency and in what that latency makes possible: brief section 9's 800 ms voice-to-voice budget
/// does not fit a 200 ms retrieval stage, and does fit a 3 ms one.
///
/// **Cross-provider logits are NOT bit-identical and that is accepted deliberately**, on the
/// human's authority, with an ADR. The acceptance is *ranking equivalence*, not byte equality --
/// see `runs/session-l/PREREGISTRATION-gpu-amendment.json`. Within a provider, byte-identity IS the
/// bar and it holds: GPU-to-GPU is bit-identical at provider defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RerankProvider {
    Cpu,
    Cuda,
}

impl RerankProvider {
    /// Whether to score the slate in ONE forward pass, **for this provider**.
    ///
    /// **Batching is not a global preference; it is a property of the hardware**, and the two
    /// providers measured opposite. M0c Session L, warm-249 held-out, ranking identical in every
    /// cell:
    ///
    /// | provider | sequential | batched | |
    /// |---|---|---|---|
    /// | CPU, 1 thread | rerank p50 **185.8 ms** | 195.6 ms | batching **LOSES** |
    /// | CUDA | rerank p50 15.2 ms | **3.4 ms** | batching **WINS**, −77% |
    ///
    /// The mechanism is the same in both directions. Batching pays for itself through parallelism
    /// across the batch dimension and amortized per-call overhead. At one CPU thread there is no
    /// such parallelism, so all that remains is the cost: attention is O(seq²) per row either way,
    /// and ten rows in one forward multiply the intermediate tensors and lose cache locality. On a
    /// GPU that parallelism is the whole machine, and ten sequential forwards pay ten kernel
    /// launches instead of one.
    ///
    /// **A single global default would therefore be wrong for one provider whichever value it
    /// took.** Both values are measured; neither is inherited from the other.
    ///
    /// Overridable by `--rerank-batch` for measurement, and the value actually used is stamped on
    /// every retrieval-profile row — so a cell cannot claim one configuration and run another.
    pub fn default_batching(self) -> bool {
        match self {
            RerankProvider::Cpu => false,
            RerankProvider::Cuda => true,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            RerankProvider::Cpu => "CPUExecutionProvider",
            RerankProvider::Cuda => "CUDAExecutionProvider",
        }
    }
}

/// What the caller **asked for**, as distinct from what was **obtained**.
///
/// Two types on purpose, exactly as `cue::dense::embedder::ProviderChoice` is separate from
/// [`RerankProvider`]. `Auto` may resolve either way depending on what the card has free at that
/// instant, and the hazard this whole file guards is a run that asked for one thing, got another,
/// and reported the request. See ADR-045.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RerankChoice {
    /// GPU if a CUDA session constructs and there is device memory for it plus a spare; CPU
    /// otherwise. **Never fails a run over a busy card** — a shared card that filled up is a
    /// throughput problem, not a correctness one.
    Auto,
    /// CPU, whatever the machine has. Every rerank number this project published before
    /// 2026-08-17 was taken here.
    Cpu,
    /// CUDA, and **a hard error if it cannot be had**. For measurement only: a CUDA cell that
    /// silently ran on CPU is Session G verbatim, and ADR-029's CUDA column would be a lie.
    Cuda,
}

impl RerankChoice {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "cpu" => Some(RerankChoice::Cpu),
            "cuda" => Some(RerankChoice::Cuda),
            "auto" => Some(RerankChoice::Auto),
            _ => None,
        }
    }

    /// The word the caller typed, for the announcement. Never parsed.
    pub fn asked(self) -> &'static str {
        match self {
            RerankChoice::Auto => "auto",
            RerankChoice::Cpu => "cpu",
            RerankChoice::Cuda => "cuda",
        }
    }
}

/// Which provider this cross-encoder ended up on, and why.
///
/// Returned rather than logged so a caller can assert on it — the embedder's `ProviderPlan` is
/// read by its bench and its tests for the same reason, and a plan nobody reads is the "declared
/// control with no reader" family.
///
/// **There is no worker-count field and that is a real difference from the embedder.** The
/// embedder opens one session per worker and its width is derived from free VRAM; the rerank stage
/// holds exactly ONE `CrossEncoder`, scores one slate at a time, and has no width to narrow. So
/// `auto` here is a two-valued decision, not a budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RerankProviderPlan {
    pub provider: RerankProvider,
    /// What was asked for. On the line beside the resolution, because before ADR-045 an explicit
    /// `cpu` run and a silent fallback to CPU printed the same words.
    pub requested: RerankChoice,
    /// Free device bytes at the read, or `None` when there is no readable device.
    pub free_at_load: Option<u64>,
    /// Measured bytes one warmed CUDA session cost, when that could be measured.
    pub session_cost: Option<u64>,
    /// One line, in English, saying which branch was taken. Reported, never parsed.
    pub reason: String,
}

/// The execution provider this stage is measured and adopted on.
///
/// CPU, deliberately. ADR-013 records that GPU is not adopted for the retrieval path: determinism
/// across execution providers and ADR-003's 1-vCPU VPS target each need their own ADR, and CPU
/// already clears the budget.
pub const PROVIDER: &str = "CPUExecutionProvider";

#[derive(Debug, thiserror::Error)]
pub enum RerankError {
    #[error(
        "{path} does not exist. The cross-encoder is never vendored ({dir} is gitignored); fetch \
         it with `python tools/fetch_model.py --cross-encoder`. There is no fallback model and no \
         default path"
    )]
    Missing { path: PathBuf, dir: String },

    #[error(
        "the CUDA runtime libraries could not be located: {reason}. \
         See crates/marlowe-memory/src/cue/dense/cuda_libs.rs"
    )]
    CudaLibs { reason: String },

    #[error(
        "{dir} holds `{superseded}` but no `{expected}`. This is the pre-Session-K int8 graph, and \
         the shipped reranker moved to the Session J fine-tune (f32) -- held-out R@1 0.6026 -> \
         0.6725, ADR-018. Point --reranking at models/ms-marco-MiniLM-L-2-v2-ft-session-j. The \
         int8 graph is NOT loadable here on purpose: a loader that accepts two graphs lets a \
         target string name one scorer and measure another. It stays reachable through the offline \
         tools (tools/session_i_rerankers.py), which is where ablations belong"
    )]
    SupersededGraph { dir: PathBuf, superseded: &'static str, expected: &'static str },

    #[error("reading {path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },

    #[error(
        "{path} hashes to {found}, the pinned digest is {expected}. This is a different file than \
         the one runs/session-k/ took the five determinism checks on and measured held-out R@1 \
         0.6725 with. A different graph still scores and still ranks -- it only moves the number, \
         which is why this is a refusal"
    )]
    DigestMismatch { path: PathBuf, found: String, expected: &'static str },

    #[error("{path}: {source}")]
    Vocab { path: PathBuf, source: crate::cue::dense::tokenizer::VocabError },

    #[error("loading the ONNX session from {path}: {source}")]
    Session { path: PathBuf, source: Box<ort::Error> },

    #[error("running the cross-encoder graph: {source}")]
    Run { source: Box<ort::Error> },

    // There is deliberately NO `ProviderFellBack` variant. ADR-013's finding is discharged at
    // construction by `error_on_failure()` (see `load`), which makes a registration failure a
    // `Session` error rather than something this stage has to detect afterwards. An unreachable
    // variant would suggest a check that does not exist.
    #[error(
        "the graph returned {found} values for {pairs} pair(s); a sequence-classification head \
         returns exactly one logit per pair. This is not the model this stage was built against"
    )]
    ShapeDisagrees { found: usize, pairs: usize },

    #[error(
        "a batch of {found} pairs was requested; invariance on this graph is measured only over \
         1..{MAX_BATCH} (runs/session-l/batch-invariance-batch10.json, max |delta| 0.000000000). \
         A larger batch is REFUSED rather than run: extrapolating an invariance result past the \
         sizes it was taken at is the inherited-measurement error this project has now paid for \
         four times. Re-run the sweep to the new size first"
    )]
    BatchTooLarge { found: usize },

    #[error("an empty batch was passed to the cross-encoder; there is nothing to score")]
    EmptyBatch,
}

fn sha256_file(path: &Path) -> Result<String, RerankError> {
    let bytes = std::fs::read(path).map_err(|source| RerankError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(format!("{:x}", hasher.finalize()))
}

fn pinned(path: &Path, expected: &'static str, dir: &Path) -> Result<(), RerankError> {
    if !path.exists() {
        return Err(RerankError::Missing {
            path: path.to_path_buf(),
            dir: dir.display().to_string(),
        });
    }
    let found = sha256_file(path)?;
    if found != expected {
        return Err(RerankError::DigestMismatch {
            path: path.to_path_buf(),
            found,
            expected,
        });
    }
    Ok(())
}

/// The loaded cross-encoder. One graph, one vocabulary, one pair at a time.
pub struct CrossEncoder {
    session: Session,
    vocab: Vocab,
    plan: RerankProviderPlan,
}

impl CrossEncoder {
    /// Load from a directory holding the pinned `model.onnx` and `tokenizer.json`.
    ///
    /// Both digests are checked before the graph is constructed, and the execution provider is
    /// required to register rather than being allowed to fall back. See the builder below.
    /// Load with the shipped, measured thread count of 1, **on CPU, fixed.**
    ///
    /// Every published number in this project was taken at one intra-op thread, per ADR-003's
    /// 1-vCPU target.
    ///
    /// **This constructor stays CPU after ADR-045 flipped the CLI default to `auto`, and the
    /// fixing is deliberate.** It is what `cross_encoder_reference.rs` loads, and that file's job
    /// is to check this build against HuggingFace's output — a reference row labelled `cpu` that
    /// resolved to CUDA on whichever machine ran it would be measuring a different scorer under
    /// the reference's label. `cross_encoder_reference.rs` asserts `provider() == Cpu` so the
    /// label cannot drift silently. The product path is [`CrossEncoder::load_auto`].
    pub fn load(dir: &Path) -> Result<Self, RerankError> {
        Self::load_with(dir, SHIPPED_THREADS, RerankProvider::Cpu)
    }

    /// Load at an explicit intra-op thread count. **Measurement only.**
    ///
    /// M0c Session L exists because ADR-003's 1-vCPU assumption is what forced single-threading,
    /// and that assumption is itself under question — so the thread count had to become measurable
    /// rather than remain a constant defended by a target nobody had re-examined.
    ///
    /// **Multi-threaded ORT can change reduction order inside a matmul**, which changes logits,
    /// which changes the ranking. So a thread count is adopted only if `scored-candidates.ndjson`
    /// stays byte-identical — determinism is the gate regardless of speed, and a faster
    /// configuration that moves one bit is rejected, not priced.
    pub fn load_with_threads(dir: &Path, threads: usize) -> Result<Self, RerankError> {
        Self::load_with(dir, threads, RerankProvider::Cpu)
    }

    /// Load a **fusion member** by name, digest-pinned from [`FUSION_GRAPHS`].
    ///
    /// The shipped graph has exactly one pinned digest and that is deliberate. This does not relax
    /// it: the name must appear in `FUSION_GRAPHS`, whose digests are compiled in, so an unknown
    /// directory is refused before any file is read. There is no caller-supplied digest and no way
    /// to pass one — that would turn the pin into a parameter, which is the same as not having it.
    ///
    /// `models_root` is the parent directory; the member's own name is joined to it.
    pub fn load_fusion_member(
        models_root: &Path,
        name: &str,
        provider: RerankProvider,
    ) -> Result<Self, RerankError> {
        let expected = FUSION_GRAPHS
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, sha)| *sha)
            .ok_or_else(|| RerankError::Missing {
                path: models_root.join(name),
                dir: format!(
                    "{name} is not in FUSION_GRAPHS; the {} pinned members are {:?}",
                    FUSION_GRAPHS.len(),
                    FUSION_GRAPHS.iter().map(|(n, _)| *n).collect::<Vec<_>>()
                ),
            })?;
        let requested = match provider {
            RerankProvider::Cpu => RerankChoice::Cpu,
            RerankProvider::Cuda => RerankChoice::Cuda,
        };
        Self::load_pinned(
            &models_root.join(name),
            expected,
            SHIPPED_THREADS,
            provider,
            requested,
            None,
        )
    }

    /// Can a CUDA session actually be **constructed** on this machine?
    ///
    /// **This is a construction, never an availability list, and the difference is the whole
    /// point.** Session G's spike found CUDA *listed* as available while failing to create on a
    /// missing `cublasLt64_12.dll`, after which ORT registered CPU and scored happily — producing a
    /// "GPU" figure within 1% of the 1-thread CPU one. `error_on_failure()` makes that a hard error
    /// instead of a silent fallback, so "it constructed" means something.
    ///
    /// **What this still does NOT establish.** `ort` 2.0.0-rc.10 exposes no node placement, and
    /// Session L measured **13.6% of nodes running on CPU** under a successfully registered CUDA
    /// session — all shape/index ops, no matmuls. So this answers *"did a CUDA session construct"*,
    /// not *"did every node run on the GPU"*. Do not write a comment here claiming otherwise.
    ///
    /// It loads the shipped graph, because probing with a graph the product does not use would
    /// measure the availability of something else.
    pub fn cuda_available(shipped_dir: &Path) -> bool {
        Self::load_with(shipped_dir, SHIPPED_THREADS, RerankProvider::Cuda).is_ok()
    }

    /// Load at an explicit thread count AND provider.
    pub fn load_with(
        dir: &Path,
        threads: usize,
        provider: RerankProvider,
    ) -> Result<Self, RerankError> {
        let requested = match provider {
            RerankProvider::Cpu => RerankChoice::Cpu,
            RerankProvider::Cuda => RerankChoice::Cuda,
        };
        Self::load_pinned(dir, MODEL_SHA256, threads, provider, requested, None)
    }

    /// Load under a **choice**, resolving [`RerankChoice::Auto`] against the card. **ADR-045.**
    ///
    /// This is the constructor the product calls. The two explicit arms delegate to
    /// [`CrossEncoder::load_with`] unchanged; `Auto` is the new behaviour and it is deliberately
    /// simple, because the rerank stage has no width to narrow:
    ///
    /// 1. **No readable device** → CPU, saying so.
    /// 2. **Free device memory under the one-spare-session floor** → CPU, quoting both numbers.
    ///    The floor is [`Self::session_cost_floor`], read from this build rather than chosen.
    /// 3. **A CUDA session fails to construct** → CPU, **with the driver's own message attached**.
    ///    This is the Session G branch: CUDA reported available, failed to create on a missing
    ///    `cublasLt64_12.dll`, and ORT would have registered CPU and scored happily. Here it is a
    ///    *visible* fallback.
    /// 4. Otherwise CUDA, with the warmed session's measured device cost recorded on the plan.
    ///
    /// **It never returns an error for want of a GPU.** A full card is a throughput outcome; a
    /// failed run would be a correctness outcome imposed for a throughput reason, which is the
    /// trade ADR-044 refused and this ADR refuses again. `RerankChoice::Cuda` is the arm that
    /// errors, and it exists for measurement.
    pub fn load_auto(
        dir: &Path,
        threads: usize,
        choice: RerankChoice,
        probe: crate::cue::dense::vram::Probe,
        reserve: crate::cue::dense::vram::Reserve<'_>,
    ) -> Result<Self, RerankError> {
        match choice {
            RerankChoice::Cpu => {
                Self::load_pinned(dir, MODEL_SHA256, threads, RerankProvider::Cpu, choice, None)
            }
            RerankChoice::Cuda => {
                // No probe gate and no fallback: the caller said CUDA, so a failure to construct
                // must surface as an error rather than as a quietly slower run.
                Self::load_pinned(
                    dir,
                    MODEL_SHA256,
                    threads,
                    RerankProvider::Cuda,
                    choice,
                    probe.free_bytes(),
                )
            }
            RerankChoice::Auto => Self::auto(dir, threads, probe, reserve),
        }
    }

    /// The floor on what one warmed CUDA cross-encoder session costs, in bytes.
    ///
    /// Both terms are read from this build rather than chosen, exactly as the embedder's is:
    ///
    /// - the **graph's own size on disk** — 62.5 MB for the shipped f32 fine-tune;
    /// - the **attention score matrix**, `[batch, heads, seq, seq]` at f32, which is the only term
    ///   that is quadratic in [`MAX_SEQ_LEN`]: `MAX_BATCH * HEADS * MAX_SEQ_LEN² * 4` bytes.
    ///   At 10 × 12 × 256² × 4 that is **31.5 MB** — three orders of magnitude below the 4.29 GB
    ///   the embedder's ALiBi term reached at `MAX_SEQ_LEN` 8192, because this stage's cap is 256
    ///   and quadratic growth cuts both ways.
    ///
    /// It is a **floor**, not an estimate: the measured delta replaces it when the measurement is
    /// larger. It exists so a device reading that does not move — a `Fixed` probe, or a concurrent
    /// free by another process — cannot be read as "a session costs nothing".
    ///
    /// **`HEADS` is 12 and is not read from the graph**, which is a stated limitation rather than
    /// an oversight: `ort` exposes no way to enumerate a graph's attention configuration, and a
    /// floor that under-counts is still a floor. If the graph is re-pinned to a wider model this
    /// term is wrong in the safe direction and the *measured* cost governs.
    pub fn session_cost_floor(model_bytes: u64) -> u64 {
        const HEADS: u64 = 12;
        let attention = MAX_BATCH as u64 * HEADS * MAX_SEQ_LEN as u64 * MAX_SEQ_LEN as u64 * 4;
        model_bytes + attention
    }

    fn auto(
        dir: &Path,
        threads: usize,
        probe: crate::cue::dense::vram::Probe,
        reserve: crate::cue::dense::vram::Reserve<'_>,
    ) -> Result<Self, RerankError> {
        let model_bytes = std::fs::metadata(dir.join(MODEL_FILE)).map(|m| m.len()).unwrap_or(0);
        let floor = Self::session_cost_floor(model_bytes);
        let cpu = |reason: String, free: Option<u64>| {
            Self::load_pinned(dir, MODEL_SHA256, threads, RerankProvider::Cpu, RerankChoice::Auto, free)
                .map(|mut e| {
                    e.plan.reason = reason;
                    e
                })
        };

        let Some(free) = probe.free_bytes() else {
            return cpu("no readable NVIDIA device (nvidia-smi absent or silent)".to_string(), None);
        };
        // **TIER 3 YIELDS TO TIER 1 -- ADR-045 SS4.** `free` is what the driver reports; `usable` is
        // what is genuinely spare once the language model's claim is honoured. Reading `free`
        // directly is the defect this replaces: on an idle card it is the WHOLE card, and taking
        // it means squatting on memory the LLM claims the moment it loads -- which Ollama answers
        // by evicting its own model, in a log that cannot see us.
        let reserved = reserve.read();
        let usable = free.saturating_sub(reserved.bytes);
        // The same one-spare-session rule the embedder applies to its first session. The card is
        // shared -- `llama-server` holds ~11.5 GB of 16.4 here -- so opening a session that leaves
        // no room for a second is how a shared card gets filled by the component that was supposed
        // to be the cheap one.
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

        let mut encoder = match Self::load_pinned(
            dir,
            MODEL_SHA256,
            threads,
            RerankProvider::Cuda,
            RerankChoice::Auto,
            Some(free),
        ) {
            Ok(e) => e,
            Err(e) => return cpu(format!("a CUDA session did not construct: {e}"), Some(free)),
        };
        // Warm at MAX_BATCH before measuring. ORT's CUDA arena allocates on first run, so a cost
        // read at construction reads a fraction of the real one -- the embedder learned this the
        // expensive way and the lesson is per-provider, not per-graph.
        let warm: Vec<&str> = vec!["warm"; MAX_BATCH];
        let _ = encoder.score_batch("warm", &warm);
        let after = probe.free_bytes().unwrap_or(free);
        encoder.plan.session_cost = Some(free.saturating_sub(after).max(floor));
        // The reserve is on the line whichever way the decision went. A GPU run that took the card
        // is exactly as much a decision about tier 1 as a CPU fallback is, and a reader who cannot
        // see the reserve cannot tell whether it was applied or forgotten.
        encoder.plan.reason = format!(
            "CUDA, one session opened and warmed at MAX_BATCH; {} MB usable of {} MB free -- {}",
            usable / (1024 * 1024),
            free / (1024 * 1024),
            reserved.reason
        );
        Ok(encoder)
    }

    /// Which provider actually served this cross-encoder, and how it was decided.
    pub fn plan(&self) -> &RerankProviderPlan {
        &self.plan
    }

    pub fn provider(&self) -> RerankProvider {
        self.plan.provider
    }

    fn load_pinned(
        dir: &Path,
        model_sha: &'static str,
        threads: usize,
        provider: RerankProvider,
        requested: RerankChoice,
        free_at_load: Option<u64>,
    ) -> Result<Self, RerankError> {
        let model_path = dir.join(MODEL_FILE);
        let tokenizer_path = dir.join(TOKENIZER_FILE);

        // Named before the generic `Missing`, so pointing at the pre-Session-K int8 directory
        // says what actually happened. A stale path in a target string is the most likely way
        // this stage gets loaded wrong, and "model.onnx does not exist" would send the reader to
        // the fetch script rather than to ADR-018.
        if !model_path.exists() && dir.join(SUPERSEDED_INT8_FILE).exists() {
            return Err(RerankError::SupersededGraph {
                dir: dir.to_path_buf(),
                superseded: SUPERSEDED_INT8_FILE,
                expected: MODEL_FILE,
            });
        }

        pinned(&model_path, model_sha, dir)?;
        pinned(&tokenizer_path, TOKENIZER_SHA256, dir)?;

        // **ADR-029's path is the one this actually restores.** `runs/session-l/RESULT.md` §5b took
        // the shipped GPU numbers through this function -- CUDA batched 3.4 ms against CPU's
        // 195.6 -- and recorded its precondition as prose: *"PATH carries torch's bundled CUDA
        // libraries for every cell."* Nothing read that, so it did not survive the session, and a
        // later one measured the failure and concluded the machine had no GPU. Now it is an input
        // with a reader. See `cue::dense::cuda_libs`.
        if provider == RerankProvider::Cuda {
            if let Some(reason) = crate::cue::dense::cuda_libs::ensure_search_path().rejection() {
                return Err(RerankError::CudaLibs { reason: reason.to_string() });
            }
        }

        let text = std::fs::read_to_string(&tokenizer_path).map_err(|source| RerankError::Io {
            path: tokenizer_path.clone(),
            source,
        })?;
        let vocab = Vocab::from_tokenizer_json(&text, &tokenizer_path.display().to_string())
            .map_err(|source| RerankError::Vocab { path: tokenizer_path, source })?;

        let session = Session::builder()
            // **ADR-013's provider finding, discharged at construction.** The Python re-costing
            // requested CUDA, ORT failed to load it on missing cuBLAS/cuDNN and fell back to CPU
            // *without raising*, producing a "GPU" figure within 1% of the 1-thread CPU one. That
            // was caught after the fact by comparing `get_providers()` against the request.
            //
            // `ort` lets the same requirement be stated up front: `error_on_failure()` turns a
            // registration failure into a hard error instead of a silent fallback to the next
            // provider. So the provider cannot silently differ from the one this stage was
            // measured on -- there is no after-the-fact check to forget to write.
            //
            // **And there could not be one here, which is worth stating rather than implying.**
            // Session K checked: `ort` 2.0.0-rc.10 exposes no way to enumerate a CONSTRUCTED
            // session's active providers -- no `get_providers`, no `available_providers`, nothing
            // on `Session`. The Python side asserts `sess.get_providers()` explicitly because it
            // can (`tools/session_j_models.py`, `session_j_verify_export.py`); Rust states the
            // requirement at build time instead, which refuses earlier rather than reporting
            // later. Do not add a comment claiming a post-construction assertion exists here.
            .and_then(|b| match provider {
                RerankProvider::Cpu => b.with_execution_providers([CPUExecutionProvider::default()
                    .build()
                    .error_on_failure()]),
                // `error_on_failure` again, and it is doing MORE work here than on CPU: the Python
                // spike found CUDA listed as "available" while failing to CREATE (missing
                // cublasLt64_12.dll), after which ORT registers CPU and scores happily. That is
                // Session G verbatim. Here it is a refusal instead.
                //
                // **It still does not cover per-node fallback.** Session L measured 13.6% of nodes
                // running on CPU under a registered CUDA session -- all shape/index ops (Gather,
                // Unsqueeze, Concat, Reshape, Equal, Where), no matmuls. `ort` exposes no node
                // placement, so that half of the check lives in
                // `tools/session_l_gpu_recovery.py` and is NOT assertable from here.
                RerankProvider::Cuda => b.with_execution_providers([CUDAExecutionProvider::default()
                    .build()
                    .error_on_failure()]),
            })
            .and_then(|b| b.with_optimization_level(GraphOptimizationLevel::Level1))
            // Threads INSIDE the graph. ADR-003 sizes against a 1-vCPU VPS and every published
            // number was read at 1 for that reason; `load` still passes `SHIPPED_THREADS`. It is a
            // parameter rather than a constant because M0c Session L is measuring whether the
            // 1-vCPU target — not the code — is the thing that should change.
            .and_then(|b| b.with_intra_threads(threads))
            // Inter-op stays 1. It parallelizes across independent graph NODES, and this graph is
            // a single sequential chain, so raising it adds a thread pool that nothing can use.
            // Sweeping it would produce a column of noise and invite reading one of its cells.
            .and_then(|b| b.with_inter_threads(1))
            .and_then(|b| b.commit_from_file(&model_path))
            // Same advisory as the embedder's, for the same reason, and narrow in the same way:
            // CUDA only, library-load failures only, unconfigured only. See `cue::dense::cuda_libs`.
            .map_err(|e| {
                if provider == RerankProvider::Cuda
                    && crate::cue::dense::embedder::is_library_load_failure(&e)
                {
                    if let Some(hint) = crate::cue::dense::cuda_libs::hint_when_unconfigured() {
                        return RerankError::CudaLibs { reason: format!("{e} -- {hint}") };
                    }
                }
                RerankError::Session { path: model_path, source: Box::new(e) }
            })?;

        let reason = match (requested, provider) {
            (RerankChoice::Cpu, _) => "CPU was asked for explicitly".to_string(),
            (RerankChoice::Cuda, _) => {
                "CUDA was asked for explicitly; no VRAM budget was applied".to_string()
            }
            // `auto` overwrites this in `auto()`, which is the only caller that can say WHY.
            (RerankChoice::Auto, p) => format!("auto resolved to {}", p.name()),
        };
        Ok(Self {
            session,
            vocab,
            plan: RerankProviderPlan {
                provider,
                requested,
                free_at_load,
                session_cost: None,
                reason,
            },
        })
    }

    /// Score one `(query, document)` pair. Higher is more relevant.
    ///
    /// Delegates to [`CrossEncoder::score_batch`]. **There is one inference path, not two** — a
    /// second copy "for the single case" is how the batched and unbatched paths would drift into
    /// scoring differently while every test that exercises only one of them stayed green.
    pub fn score(&mut self, query: &str, document: &str) -> Result<f32, RerankError> {
        Ok(self.score_batch(query, std::slice::from_ref(&document))?[0])
    }

    /// Score a whole slate against one query, in **one** forward pass. Results are in input order.
    ///
    /// This is the shipped path. See the module docs for why batching is now permitted and what
    /// was measured before it was: max `|batched − single|` **0.000000000** across every batch size
    /// 1..[`MAX_BATCH`], so the ordering the ranker sees is bit-identical to the sequential one.
    ///
    /// **Refuses above [`MAX_BATCH`]** rather than extrapolating past the measured envelope.
    pub fn score_batch(&mut self, query: &str, documents: &[&str]) -> Result<Vec<f32>, RerankError> {
        if documents.is_empty() {
            return Err(RerankError::EmptyBatch);
        }
        if documents.len() > MAX_BATCH {
            return Err(RerankError::BatchTooLarge { found: documents.len() });
        }
        let encoded: Vec<EncodedPair> = documents
            .iter()
            .map(|d| encode_pair(&self.vocab, query, d, MAX_SEQ_LEN))
            .collect();
        self.run(&encoded)
    }

    fn run(&mut self, encoded: &[EncodedPair]) -> Result<Vec<f32>, RerankError> {
        let pairs = encoded.len();

        // Row-major concatenation: row `i` is pair `i`, which is what makes the output's order the
        // input's order. Written as an explicit flatten rather than relying on an iterator chain
        // so the layout is legible beside the shape below.
        let mut ids: Vec<i64> = Vec::with_capacity(pairs * MAX_SEQ_LEN);
        let mut mask: Vec<i64> = Vec::with_capacity(pairs * MAX_SEQ_LEN);
        let mut types: Vec<i64> = Vec::with_capacity(pairs * MAX_SEQ_LEN);
        for pair in encoded {
            ids.extend(pair.input_ids.iter().map(|&v| v as i64));
            mask.extend(pair.attention_mask.iter().map(|&v| v as i64));
            types.extend(pair.token_type_ids.iter().map(|&v| v as i64));
        }

        let shape = [pairs as i64, MAX_SEQ_LEN as i64];
        // The sequence dimension is still structural: `MAX_SEQ_LEN` is a priced defect (ADR-015,
        // ADR-017) and every pair is padded to it. Only the BATCH dimension became variable, and
        // only up to the size its invariance was measured at.
        assert!(pairs <= MAX_BATCH, "batch {pairs} exceeds the measured envelope {MAX_BATCH}");
        assert_eq!(ids.len(), pairs * MAX_SEQ_LEN, "fixed [{pairs}, {MAX_SEQ_LEN}] shape");

        let make = |data: Vec<i64>| -> Result<Value, RerankError> {
            Value::from_array((shape, data))
                .map(|v| v.into_dyn())
                .map_err(|e| RerankError::Run { source: Box::new(e) })
        };
        // BY NAME, never by position. `token_type_ids` is what carries the query/candidate
        // boundary, and binding positionally would transpose it on any graph that orders its
        // inputs differently -- the model would still return a plausible score.
        let inputs: Vec<(String, Value)> = vec![
            ("input_ids".to_string(), make(ids)?),
            ("attention_mask".to_string(), make(mask)?),
            ("token_type_ids".to_string(), make(types)?),
        ];

        let outputs = self
            .session
            .run(inputs)
            .map_err(|e| RerankError::Run { source: Box::new(e) })?;
        let (_, logits) = outputs[0]
            .try_extract_tensor::<f32>()
            .map_err(|e| RerankError::Run { source: Box::new(e) })?;

        // One logit per pair, and the count is checked rather than assumed. A graph returning a
        // two-class head would silently halve the slate here and the ranker would score every
        // candidate against its neighbour's logit.
        if logits.len() != pairs {
            return Err(RerankError::ShapeDisagrees { found: logits.len(), pairs });
        }
        Ok(logits.to_vec())
    }
}
