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

use ort::session::builder::GraphOptimizationLevel;
use ort::session::Session;
use ort::value::Value;
use sha2::{Digest, Sha256};

use crate::cue::dense::tokenizer::{encode_pair, EncodedPair, Vocab};
use crate::ep::Provider;

/// The provider a cross-encoder runs on. One enum for every ONNX graph; see [`crate::ep`].
pub use crate::ep::Provider as RerankProvider;

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

pub const MODEL_FILE: &str = "model.onnx";
pub const TOKENIZER_FILE: &str = "tokenizer.json";

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

/// The execution provider this stage is measured and adopted on.
///
/// CPU, deliberately: every shipped number was read on it. The GPU path (`cuda` feature) is the
/// cascade in [`crate::cascade`], taken by the product only while the GPU is idle.
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

    #[error(
        "{name} is not a member of the cascade's fusion (crate::cascade::FUSION_GRAPHS). The membership is a rule fixed before any number existed, not a selection"
    )]
    NotAFusionMember { name: String },
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

/// The loaded cross-encoder. One graph, one vocabulary, one provider.
pub struct CrossEncoder {
    session: Session,
    vocab: Vocab,
    provider: Provider,
    graph: &'static str,
}

impl CrossEncoder {
    /// Load the shipped graph with the measured thread count of 1, **on CPU.**
    ///
    /// Both digests are checked before the graph is constructed, and the provider is required to
    /// register rather than allowed to fall back. Every published number was taken at one intra-op
    /// thread, per ADR-003's 1-vCPU target. `cross_encoder_reference.rs` loads through this and
    /// asserts `provider() == Cpu`.
    pub fn load(dir: &Path) -> Result<Self, RerankError> {
        Self::load_on(dir, SHIPPED_THREADS, Provider::Cpu)
    }

    /// Load at an explicit intra-op thread count, on CPU. **Measurement only.**
    ///
    /// **Multi-threaded ORT can change reduction order inside a matmul**, which changes logits,
    /// which changes the ranking. So a thread count is adopted only if `scored-candidates.ndjson`
    /// stays byte-identical — determinism is the gate regardless of speed.
    pub fn load_with_threads(dir: &Path, threads: usize) -> Result<Self, RerankError> {
        Self::load_on(dir, threads, Provider::Cpu)
    }

    /// The shipped graph on CPU; kept as the product's stable entry point.
    pub fn load_auto(dir: &Path, threads: usize) -> Result<Self, RerankError> {
        Self::load_on(dir, threads, Provider::Cpu)
    }

    /// Load the shipped graph on an explicit provider. A provider that does not construct is a
    /// [`RerankError::Session`] error, never a silent CPU session.
    pub fn load_on(dir: &Path, threads: usize, provider: Provider) -> Result<Self, RerankError> {
        // Named before the generic `Missing`, so pointing at the pre-Session-K int8 directory
        // says what actually happened rather than sending the reader to the fetch script.
        if !dir.join(MODEL_FILE).exists() && dir.join(SUPERSEDED_INT8_FILE).exists() {
            return Err(RerankError::SupersededGraph {
                dir: dir.to_path_buf(),
                superseded: SUPERSEDED_INT8_FILE,
                expected: MODEL_FILE,
            });
        }
        Self::load_pinned(dir, MODEL_SHA256, "ms-marco-MiniLM-L-2-v2-ft-session-j", threads, provider)
    }

    /// Load one member of the GPU cascade's fusion by name. Its digest comes from
    /// [`crate::cascade::FUSION_GRAPHS`] and nowhere else; every member shares the shipped
    /// tokenizer, pinned by [`TOKENIZER_SHA256`]. An unlisted name is refused.
    pub fn load_fusion_member(
        dir: &Path,
        name: &str,
        threads: usize,
        provider: Provider,
    ) -> Result<Self, RerankError> {
        let (graph, digest) = crate::cascade::FUSION_GRAPHS
            .iter()
            .find(|(n, _)| *n == name)
            .copied()
            .ok_or_else(|| RerankError::NotAFusionMember { name: name.to_string() })?;
        Self::load_pinned(dir, digest, graph, threads, provider)
    }

    /// Which provider serves this cross-encoder.
    pub fn provider(&self) -> RerankProvider {
        self.provider
    }

    /// Which pinned graph this is.
    pub fn graph(&self) -> &'static str {
        self.graph
    }

    fn load_pinned(
        dir: &Path,
        digest: &'static str,
        graph: &'static str,
        threads: usize,
        provider: Provider,
    ) -> Result<Self, RerankError> {
        let model_path = dir.join(MODEL_FILE);
        let tokenizer_path = dir.join(TOKENIZER_FILE);
        pinned(&model_path, digest, dir)?;
        pinned(&tokenizer_path, TOKENIZER_SHA256, dir)?;

        let text = std::fs::read_to_string(&tokenizer_path).map_err(|source| RerankError::Io {
            path: tokenizer_path.clone(),
            source,
        })?;
        let vocab = Vocab::from_tokenizer_json(&text, &tokenizer_path.display().to_string())
            .map_err(|source| RerankError::Vocab { path: tokenizer_path, source })?;

        // **ADR-013's provider finding, discharged at construction.** The old Python re-costing
        // requested CUDA, ORT failed on missing cuBLAS/cuDNN and fell back to CPU *without
        // raising*. `ep::register` uses `error_on_failure()`, so the provider cannot silently
        // differ from the one asked for.
        let build = || -> ort::Result<Session> {
            let b = crate::ep::register(crate::ep::session_builder()?, provider)?;
            let b = b.with_optimization_level(GraphOptimizationLevel::Level1)?;
            // Threads INSIDE the graph; `load` passes `SHIPPED_THREADS` (ADR-003). Inter-op stays
            // 1: this graph is a single sequential chain, so a second pool has nothing to run.
            let mut b = b.with_intra_threads(threads)?.with_inter_threads(1)?;
            b.commit_from_file(&model_path)
        };
        let session =
            build().map_err(|e| RerankError::Session { path: model_path.clone(), source: Box::new(e) })?;
        Ok(Self { session, vocab, provider, graph })
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
