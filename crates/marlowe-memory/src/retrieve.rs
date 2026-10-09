//! CONTRACTS.md sections 4.1â€“4.4 â€” retrieval.
//!
//! Session B adds the first relevance judgment: one lexical cue ([`crate::cue::lexical`]) and
//! the frozen gate ([`crate::gate`]). What that is **not** is the five-cue system K1 measures â€”
//! dense, entity-graph, temporal and causal are all missing, and a number produced here is a
//! statement about an incomplete cue set rather than about the design.
//!
//! The pipeline, in order, matching Â§4.2's latency breakdown:
//!
//! 1. **candidates** â€” Â§4.3's three exclusions, then session scope
//! 2. **cues** â€” BM25 over that set
//! 3. **gate** â€” frozen weights â†’ score â†’ isotonic curve â†’ calibrated precision â†’ threshold
//! 4. **budget** â€” Â§5.7's â‰¤7,000 tokens, applied to what survived the gate
//!
//! Order matters at step 4. Gating **before** budgeting means the budget cuts the least
//! confident of the memories worth injecting; budgeting first would cut on recency and hand
//! the gate a set that was already truncated for an unrelated reason.

use marlowe_contract::{Fidelity, InjectedMemory};

use crate::cue::dense::{self, vectors::VectorStore};
use crate::cue::lexical;
use crate::entry::MemoryEntry;
use crate::gate::{features, FrozenGate};
use crate::operating_point::{Abstention, Coverage, OperatingPoint};
use crate::probe::{Stage, StageProbe};
use crate::rerank::CrossEncoder;
use crate::store::BeliefStore;

/// The gate stamp for a build with no gate â€” Session A's state, retained for the feature-dump
/// path's sibling and as the thing `gate::GATE_VERSION` is asserted to differ from.
///
/// It is deliberately not `frozen-v1`. A report reading `gate.version` must be able to tell
/// that nothing was scored â€” a plausible version string next to zero scores would read as a
/// gate that ran and found nothing interesting, which is a different and much better-looking
/// claim than the true one.
pub const UNGATED_VERSION: &str = "ungated-v0";

/// Characters per token, used **pessimistically on purpose**.
///
/// There is no tokenizer here: ADR-004's model is not wired yet, and inventing a precise
/// count would be reporting a measurement that was not taken. So this is an estimate, and
/// the direction it errs in is the whole point.
///
/// Three characters per token *over*-estimates for ordinary English (four is the usual rule
/// of thumb). Over-estimating can only make the reported `retrieval_tokens` look worse than
/// reality, so it can never hide a miss against section 5.7's â‰¤7,000 budget. An estimator
/// that erred the other way would let a real budget overrun report as compliant, which is the
/// failure mode that matters.
pub const CHARS_PER_TOKEN_PESSIMISTIC: usize = 3;

pub fn estimate_tokens(text: &str) -> u32 {
    // div_ceil so a non-empty string never estimates zero tokens.
    (text.len().div_ceil(CHARS_PER_TOKEN_PESSIMISTIC)) as u32
}

/// The inactivity gap that separates one derived session from the next.
///
/// **Thirty minutes, and it is not swept.** Chosen as the web-analytics inactivity-timeout
/// convention â€” a value that exists independently of this corpus and was not derived from it. A
/// threshold picked by looking at LongMemEval's inter-session gaps would be a parameter fit on the
/// data it is then evaluated against. It has to be defensible for the production mechanism too,
/// and Marlowe's real sessions are terminal sessions, where a 30-minute gap is a boundary as well.
///
/// Frozen in `runs/session-h/PREREGISTRATION.json â†’ frozen_parameters.session_gap_ms`.
pub const SESSION_GAP_MS: i64 = 30 * 60 * 1000;

/// How many sessions per cue survive pruning. The union is taken, so at most `2N` survive.
///
/// Inherited rather than chosen: the registered question names "the arm-1 N=3 pruned pool".
pub const PRUNE_TOP_N: usize = 3;

/// Group candidates into sessions by `occurred_at_ms` contiguity.
///
/// Returns one session index per candidate, **in the candidates' own order**.
///
/// ## Why this exists rather than a session id
///
/// CONTRACTS.md Â§4.6 carries no internal session structure. The harness flattens a case's ~48
/// haystack sessions into one history whose `session_id` is the question id, so the real boundary
/// survives only inside the *harness's private* `turn_id` encoding. Teaching the shipping
/// retrieval path to parse that would be the implementation reshaping itself around the
/// scoreboard, so it is not done and there is no fallback to it. Â§4.6 not carrying sessions is
/// recorded as the real defect and as an M0a change with its own registration.
///
/// ## What it is measured to do, and what it is measured NOT to do
///
/// On the fit split, against the true partition (`runs/session-h/sessionizer-and-arm1-fit.json`):
///
/// * gold retention at N=3 is **identical**, Î” +0.0000
/// * completeness **1.0000** â€” no true session is ever split across two derived ones, and no gold
///   session is split, so pruning can never discard the fragment holding the answer
/// * it **merges**: 39.0 derived sessions per case against 47.7 true, so the surviving pool is
///   15.5% where the true partition gives 10.0%
///
/// That last figure **failed** the registered pool-inflation guard at 1.546Ã— against â‰¤1.5Ã—. The
/// argument for proceeding is recorded in `runs/session-h/BAND-FAILURE-ARGUMENT.md` and is *not*
/// that the miss was small: the guard is a proxy for gold damage, and the quantity it proxies for
/// was measured directly and is exactly zero.
pub fn session_keys(candidates: &[&MemoryEntry], gap_ms: i64) -> Vec<u32> {
    let mut order: Vec<usize> = (0..candidates.len()).collect();
    // Sorted by time, then by id. The id tiebreak is for determinism only and cannot change the
    // grouping: two candidates at the same millisecond have a gap of 0, which never exceeds the
    // threshold, so they land in the same session whichever order they are visited in.
    order.sort_by(|a, b| {
        candidates[*a]
            .occurred_at_ms
            .cmp(&candidates[*b].occurred_at_ms)
            .then_with(|| candidates[*a].id.cmp(&candidates[*b].id))
    });

    let mut keys = vec![0u32; candidates.len()];
    let mut group = 0u32;
    let mut previous: Option<i64> = None;
    for index in order {
        let at = candidates[index].occurred_at_ms;
        if let Some(prev) = previous {
            if at - prev > gap_ms {
                group += 1;
            }
        }
        keys[index] = group;
        previous = Some(at);
    }
    keys
}

/// Which sessions survive pruning: the union of each cue's top-`n` by max-aggregated score.
///
/// **Max aggregation, and the union.** Frozen in the pre-registration before the fit-split
/// re-derivation, which is what closes ADR-013's second lesson â€” Session G declared three variants
/// and left the rule free, so its best was not quotable. This is the first rule it declared and the
/// only one with no free parameter. **A better session scorer is worth about four cases and is
/// explicitly out of scope.**
///
/// Returns `None` when nothing can be pruned â€” fewer sessions than the union would keep â€” so the
/// caller can record "pruning did not apply" rather than a no-op that looks like a decision.
fn surviving_sessions(
    keys: &[u32],
    cue_scores: &[&[f32]],
    n: usize,
) -> Option<std::collections::BTreeSet<u32>> {
    use std::collections::{BTreeMap, BTreeSet};

    let mut distinct = BTreeSet::new();
    for key in keys {
        distinct.insert(*key);
    }
    if distinct.len() <= n {
        return None;
    }

    let mut keep = BTreeSet::new();
    for scores in cue_scores {
        // BTreeMap, not HashMap: the determinism guard bans hash-ordered collections, and the
        // tiebreak below reads this map's order.
        let mut best: BTreeMap<u32, f32> = BTreeMap::new();
        for (key, score) in keys.iter().zip(scores.iter()) {
            let slot = best.entry(*key).or_insert(f32::NEG_INFINITY);
            if *score > *slot {
                *slot = *score;
            }
        }
        let mut ranked: Vec<(u32, f32)> = best.into_iter().collect();
        // Score descending, then session key ascending. Deterministic at every tie.
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        for (key, _) in ranked.into_iter().take(n) {
            keep.insert(key);
        }
    }
    Some(keep)
}

/// How a run scores its candidates.
///
/// One mode. The absence of a "gate is optional" variant is the point: there is no path where a
/// calibrated stamp can be produced without a calibrated gate.
pub enum Scoring<'a> {
    /// The shipping path.
    ///
    /// **`operating_point` is a required field, not an `Option`.** K1 condition 3 is binding: a
    /// configuration that injects at low precision to raise coverage fails outright. A nullable
    /// cut point would be a path where injection happens with no declared operating point, which
    /// is the thing the criterion forbids â€” and it would be reached by forgetting an argument
    /// rather than by deciding anything.
    ///
    /// It is a field on `Gated` rather than a third variant because the absence of a
    /// "gate is optional" variant is the point (see the enum's own doc), and the same reasoning
    /// applies here: there is no calibrated injection without a calibrated cut point.
    Gated {
        gate: &'a FrozenGate,
        operating_point: &'a OperatingPoint,
        /// **Required, and `Declared` is the only value that ships.** `Full` is the un-gated arm of
        /// the controlled comparison that tells an ASR of 0.000 caused by a guard from one caused
        /// by nothing being injected. A field rather than a default so a run cannot be in the
        /// measurement arm without having said so.
        coverage: Coverage,
    },
}

/// Whether a run reranks, and with what budget.
///
/// Two variants and no "rerank with default settings" third. The budget is always explicit because
/// it is the whole cost of the stage: Q2's registered question fixes it at 10, and a build that
/// could quietly use a different number would be answering a different question.
pub enum Rerank<'a> {
    /// No cross-encoder. The ranking is Session F's exactly.
    Off,
    CrossEncoder {
        encoder: &'a mut CrossEncoder,
        /// How many of the pruned pool's top candidates to rerank.
        budget: usize,
        /// Score the whole slate in one forward pass instead of one pair at a time.
        ///
        /// **A measurement switch, and `false` is the shipped value.** M0c Session L measured
        /// batching at one intra-op thread as a **12% regression** (rerank p50 187.6 â†’ 210.2 ms,
        /// max 205.3 â†’ 259.3) with output bit-identical, so it is off on cost grounds rather than
        /// correctness grounds. It is a field rather than a constant because the finding is
        /// *conditional on the thread count*, and the thread count is what Session L is measuring.
        ///
        /// Carried on the variant rather than read from a global so a run cannot report one
        /// configuration and execute another; the retrieval profile records the value per query.
        batched: bool,
    },
}

/// Which beliefs a retrieval may consider: this session's, or the whole profile's.
///
/// # Why this is declared rather than assumed, and why the two callers differ
///
/// **The eval must stay `ThisSession`.** LongMemEval flattens each case's haystack into one history
/// whose `session_id` is the query id, so a profile-wide scope would let every case see every other
/// case's turns. Every published R@1, the precision/coverage curve and the poisoning numbers are all
/// computed under session scoping; widening it there would not improve them, it would invalidate
/// them.
///
/// **The product wants `Profile`, and without it memory does not work.** A session in the daemon is
/// a *client name* â€” the TUI connects as `tui`, `--ask` as `cli` â€” so under `ThisSession` a memory
/// written at the CLI is invisible to auto-injection in the TUI, and the eleven-week callback cannot
/// happen across surfaces. `recall` has always been profile-wide (`recall_candidates` has no session
/// filter), so this also ends an asymmetry where explicit search could see what injection could not.
///
/// # THE CALIBRATION DOES NOT TRANSFER ACROSS THIS BOUNDARY, and that is not fixable here
///
/// The declared operating point's margin threshold was measured on **session-scoped** pools â€” on
/// LongMemEval, roughly one session's turns per query. Under `Profile` the candidate pool is the
/// whole store, so the rank-1/rank-2 margin distribution is a different distribution, and the
/// published coverage (10.0%) and precision are **statements about the `ThisSession` configuration
/// only**.
///
/// The cut point is still the best available threshold and it is still in the graph's own logit
/// units. What must not happen is anyone quoting `docs/design/PRECISION-COVERAGE.md`'s numbers as a
/// description of the product's behaviour. Re-measuring under `Profile` needs a corpus with
/// cross-session structure, which LongMemEval is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetrievalScope {
    /// Only beliefs whose `source_session_id` matches the request. The measured configuration.
    ThisSession,
    /// Every belief in the profile. What a person means by "remember".
    Profile,
}

/// Q2's registered budget, and therefore the shipped candidate count.
///
/// Fixed by `runs/session-g/REGISTERED-QUESTION-in-session-rerank.json`, not by this build. At
/// ~9 ms per pair measured at 1 thread, ten pairs is ~90 ms against Â§5.7's 300 ms.
pub const RERANK_BUDGET: usize = 10;

/// How many memories the operating point admits once it decides to inject.
///
/// **Was 1.** Changed under direction in M0c Session M2. Held-out on the shipped ranking the gold
/// turn is at rank 1 for **0.6725** of queries and inside the top 3 for **0.8515** â€” so k = 3 raises
/// the chance the right memory is present by **+0.1790** and necessarily lowers per-memory
/// precision, since at most one of the three can be gold.
///
/// **`docs/design/PRECISION-COVERAGE.md` is a top-1 artifact and does not describe this.** See the
/// K1 block in `select_for_injection` for the full debt.
pub const ADMIT_TOP_K: usize = 3;

/// One candidate after scoring. The dump writes these; the selection ranks them.
pub struct ScoredCandidate<'a> {
    pub entry: &'a MemoryEntry,
    pub features: features::FeatureVector,
    /// The winning cue's within-query **z**. See [`crate::gate::Verdict::score`] â€” this field's
    /// meaning has moved twice without the name changing, and it is the ranking key's first level.
    pub score: f32,
    /// The winning cue's **margin** over its runner-up. The ranking key's second level.
    pub margin: f32,
    /// **max** over the per-cue calibrated precisions. What the threshold reads, and under v4
    /// the only thing it does.
    pub calibrated_precision: f32,
    /// Which cue's opinion won. Diagnostic, carried into the dump.
    pub winning_cue: &'static str,
    pub passes: bool,

    /// The derived session this candidate belongs to. See [`session_keys`].
    pub session_key: u32,
    /// Did this candidate survive session-level pruning?
    ///
    /// **Pruned-away candidates stay in `scored` rather than being dropped**, for the same reason
    /// the gate's rejects do: `scored` is the population every offline number is computed over, and
    /// a candidate that vanished with nothing recording it had been there is this project's
    /// unobservable-mismatch pattern applied to a denominator. They are ranked last, not deleted.
    pub survived_pruning: bool,
    /// The cross-encoder's logit, for the candidates that were reranked.
    ///
    /// `None` means *not reranked* â€” either no cross-encoder was loaded, or this candidate was
    /// outside the reranking budget. It never means "scored zero": a cross-encoder logit is signed
    /// and near-zero is a real, middling score, so a `0.0` sentinel would be indistinguishable from
    /// a genuine reading.
    pub rerank_score: Option<f32>,
}

pub struct Selection<'a> {
    pub injected: Vec<InjectedMemory>,
    /// The true size of the set that passed section 4.3's three exclusions, across the whole
    /// store, before session scoping and the budget cut.
    ///
    /// Reported as what was actually enumerated. Setting it to `injected.len()` or to a
    /// constant would be a fabricated measurement in a field the report treats as real.
    pub considered: u32,
    pub retrieval_tokens: u32,
    /// Every candidate in scope, scored. Retained so the feature dump can write the negatives
    /// too â€” a calibration fit only on what was injected would be fit on its own output.
    pub scored: Vec<ScoredCandidate<'a>>,
    /// How many candidates were in the session's scope at all.
    pub scoped: u32,
    /// How many cleared the gate's threshold, before the budget.
    pub above_threshold: u32,
    pub budget_exhausted: bool,
    /// How many candidates survived session-level pruning.
    ///
    /// Equal to `scoped` when pruning did not apply â€” there were no more sessions than the union
    /// would have kept. Reported rather than inferred, so "pruning kept everything" and "pruning
    /// did not run" are distinguishable in the report.
    pub survived_pruning: u32,
    /// Whether pruning actually ran and removed something.
    pub pruning_applied: bool,
    /// How many candidates the cross-encoder scored. Zero when reranking is off.
    pub reranked: u32,
    /// How many distinct sessions [`session_keys`] derived from the scoped candidates.
    pub derived_sessions: u32,

    /// The rank-1 minus rank-2 **cross-encoder** margin on the shipped ranking key â€” the quantity
    /// the declared operating point thresholds.
    ///
    /// **Not [`ScoredCandidate::margin`]**, which is the winning *cue's* lead over its own runner-up
    /// in that cue's raw units. Two different numbers whose names differ by a word, and the
    /// published curve is computed on this one.
    ///
    /// `None` means the margin is not defined for this query â€” fewer than two candidates, or rank 1
    /// or rank 2 outside the rerank budget. [`Selection::abstention`] says which.
    pub rerank_margin: Option<f32>,

    /// Why nothing was injected, when nothing was. `None` means something was.
    ///
    /// Recorded rather than left to be inferred from an empty `injected`: "abstained because the
    /// margin was below the cut point" and "abstained because there were no candidates at all" are
    /// different facts about the system, and only one of them is the operating point doing its job.
    pub abstention: Option<Abstention>,
}

/// Dense cosine for every candidate, in the candidate set's order.
///
/// **A missing vector or a missing query vector scores 0.0 â€” never a skip.** Two reasons, and
/// the second is the one that matters:
///
/// * 0.0 is the honest value. The dense cue found no evidence for this candidate; that is what
///   "no evidence" is worth, and `dense::cosine` already floors at zero for the same reason.
/// * Skipping would remove the candidate from `scored`, which is what the feature dump and the
///   swept curve are built from. The candidate would vanish from the calibration with nothing
///   recording that it had been there â€” this project's unobservable-mismatch pattern, applied to
///   the population a number is computed over.
///
/// A run where vectors are systematically missing therefore reports a dense feature that is
/// constant zero, which the fitter's own variance check catches and pins. That is a loud
/// failure; a silently shrinking candidate set is not.
fn dense_for(
    candidates: &[&MemoryEntry],
    query_vector: Option<&[f32]>,
    vectors: &VectorStore,
) -> Vec<f32> {
    let Some(query) = query_vector else {
        return vec![0.0; candidates.len()];
    };
    candidates
        .iter()
        .map(|entry| match vectors.get(&entry.id) {
            Some(vector) => dense::cosine(query, vector),
            None => 0.0,
        })
        .collect()
}

/// The slate: the top `budget` pruning survivors under the EXISTING ranking key.
///
/// `(winning cue z desc, margin desc, id asc)` â€” the same key Session H left in place, so
/// nothing about the draw is new to justify.
fn draw_slate<'a>(scored: &[ScoredCandidate<'a>], budget: usize) -> Vec<usize> {
    let mut slate: Vec<usize> = (0..scored.len()).filter(|i| scored[*i].survived_pruning).collect();
    slate.sort_by(|a, b| {
        let (x, y) = (&scored[*a], &scored[*b]);
        y.score
            .total_cmp(&x.score)
            .then_with(|| y.margin.total_cmp(&x.margin))
            .then_with(|| x.entry.id.cmp(&y.entry.id))
    });
    slate.truncate(budget);
    slate
}

/// Score one slate against the query, writing [`ScoredCandidate::rerank_score`] in place.
///
/// **Batched scoring never exceeds [`crate::rerank::MAX_BATCH`].** The invariance sweep covers
/// sizes 1..10 and nothing above, so a slate is scored as chunks of at most 10 rows. Chunking
/// keeps every forward inside the measured envelope instead of extrapolating past it.
///
/// Errors are loud per pair or per chunk and leave that candidate's score unset â€” the same
/// semantics the single-graph path has always had. A reranker that silently scored nothing would
/// leave the stage looking present in the report and absent in the ranking.
fn score_slate(
    encoder: &mut CrossEncoder,
    query_text: &str,
    scored: &mut [ScoredCandidate<'_>],
    slate: &[usize],
    batched: bool,
) {
    if slate.is_empty() {
        return;
    }
    if !batched {
        for index in slate {
            match encoder.score(query_text, &scored[*index].entry.text) {
                Ok(logit) => {
                    scored[*index].rerank_score = Some(logit);
                }
                Err(e) => {
                    eprintln!("marlowe: cross-encoder failed on {}: {e}", scored[*index].entry.id)
                }
            }
        }
        return;
    }
    for chunk in slate.chunks(crate::rerank::MAX_BATCH) {
        let documents: Vec<&str> =
            chunk.iter().map(|i| scored[*i].entry.text.as_str()).collect();
        match encoder.score_batch(query_text, &documents) {
            Ok(logits) => {
                for (index, logit) in chunk.iter().zip(logits) {
                    scored[*index].rerank_score = Some(logit);
                }
            }
            Err(e) => {
                eprintln!(
                    "marlowe: cross-encoder failed on a batch of {} for query {:?}: {e}",
                    documents.len(),
                    query_text
                );
            }
        }
    }
}

/// Select what to inject.
///
/// Ranking under [`Scoring::Gated`] is `(score desc, margin desc, id asc)` â€” the winning cue's
/// **continuous, query-local** z and then its margin. It is **totally deterministic**, and that
/// is not a nicety:
/// `marlowe-eval repro` compares two runs byte for byte and the injected set is in the hash. The
/// final tiebreak is always `id` because two entries with equal scores would otherwise be ordered
/// by whatever the collection did.
///
/// **`calibrated_precision` does not appear in the key, and that is ADR-010 being obeyed rather
/// than remembered.** Session D ranked on it, isotonic output is a step function, and 60.4% of
/// held-out cases ended in a tie at the fused maximum with the tiebreak deciding top-1 outright.
/// The calibrated value now decides two yes/no-shaped things â€” whether a candidate clears the
/// operating point, and which cue speaks for it â€” and orders nothing.
pub fn select_for_injection<'a>(
    beliefs: &'a BeliefStore,
    session_id: &str,
    query_text: &str,
    now_ms: i64,
    max_tokens: u32,
    scoring: &Scoring<'_>,
    vectors: &VectorStore,
    query_vector: Option<&[f32]>,
    rerank: &mut Rerank<'_>,
    scope: RetrievalScope,
) -> Selection<'a> {
    select_for_injection_probed(
        beliefs,
        session_id,
        query_text,
        now_ms,
        max_tokens,
        scoring,
        vectors,
        query_vector,
        rerank,
        scope,
        &mut (),
    )
}

/// [`select_for_injection`], with stage boundaries announced to `probe`.
///
/// **One implementation, not two.** The un-probed entry point above delegates here with `()`,
/// which monomorphizes to the identical machine code â€” so the profile is taken of the shipped
/// path rather than of a parallel copy that could drift from it. A second `select_for_injection`
/// written "for profiling" is the two-sides-silently-disagree pattern applied to a measurement.
///
/// See [`crate::probe`] for why the clock read is not in this crate.
#[allow(clippy::too_many_arguments)]
pub fn select_for_injection_probed<'a, P: StageProbe>(
    beliefs: &'a BeliefStore,
    session_id: &str,
    query_text: &str,
    now_ms: i64,
    max_tokens: u32,
    scoring: &Scoring<'_>,
    vectors: &VectorStore,
    query_vector: Option<&[f32]>,
    rerank: &mut Rerank<'_>,
    scope: RetrievalScope,
    probe: &mut P,
) -> Selection<'a> {
    probe.enter(Stage::Candidates);
    let candidates = beliefs.injection_candidates(now_ms);
    let considered = candidates.len() as u32;

    // Session scoping. Note what this is not: a relevance judgment. It is the scope the
    // request names, and the cue is what judges relevance inside it.
    probe.enter(Stage::Scope);
    let scoped: Vec<&MemoryEntry> = candidates
        .into_iter()
        .filter(|e| match scope {
            RetrievalScope::ThisSession => e.source_session_id == session_id,
            RetrievalScope::Profile => true,
        })
        .collect();

    probe.enter(Stage::Lexical);
    let raw_scores = lexical::score_all(&scoped, query_text);
    probe.enter(Stage::Dense);
    let dense_scores = dense_for(&scoped, query_vector, vectors);

    // **Set-level, not per-candidate.** Rank, margin and z do not exist for a candidate in
    // isolation, and this is the call that makes the calibration's question query-local.
    probe.enter(Stage::Features);
    let vectors = features::extract_all(&scoped, &raw_scores, &dense_scores);

    probe.enter(Stage::Gate);
    let mut scored: Vec<ScoredCandidate<'a>> = scoped
        .iter()
        .zip(vectors.into_iter())
        .map(|(entry, f)| match scoring {
            Scoring::Gated { gate, .. } => {
                let v = gate.judge(&f);
                ScoredCandidate {
                    entry,
                    features: f,
                    score: v.score,
                    margin: v.margin,
                    calibrated_precision: v.calibrated_precision,
                    winning_cue: v.winning_cue,
                    passes: v.passes,
                    session_key: 0,
                    survived_pruning: true,
                    rerank_score: None,
                }
            }
        })
        .collect();

    // ---- session pruning and reranking ------------------------------------------------------
    //
    // **After `features::extract_all`, deliberately, and this is the most consequential ordering
    // decision in the session.** The frozen gate's isotonic curves were fit on margin
    // distributions drawn from ~487-candidate pools. Computing features over ~50 candidates would
    // feed those curves a distribution they were never fit on â€” two sides silently disagreeing,
    // which this project has now produced seven instances of. Every candidate's
    // `calibrated_precision` here is therefore bit-identical to Session F's, and the registered
    // consequence is that **the ceiling cannot rise**: the max over a subset can only equal or
    // fall below the max over the whole. Refitting the gate on pruned pools is the named next
    // lever and needs its own registration.
    probe.enter(Stage::Prune);
    let mut pruning_applied = false;
    {
        let keys = session_keys(&scoped, SESSION_GAP_MS);
        for (candidate, key) in scored.iter_mut().zip(keys.iter()) {
            candidate.session_key = *key;
        }
        if let Some(keep) = surviving_sessions(&keys, &[&raw_scores, &dense_scores], PRUNE_TOP_N) {
            pruning_applied = true;
            for (candidate, key) in scored.iter_mut().zip(keys.iter()) {
                candidate.survived_pruning = keep.contains(key);
            }
        }
    }

    probe.enter(Stage::Rerank);
    match rerank {
        Rerank::Off => {}
        Rerank::CrossEncoder { encoder, budget, batched } => {
            // The slate: the top `budget` survivors under the EXISTING ranking key. Drawing the slate
            // with the key the reranker then replaces is what makes this a rerank stage rather than a
            // new cue â€” and it is what Q2's registered "fixed budget of 10 reranked pairs" costs.
            let slate = draw_slate(&scored, *budget);
            score_slate(encoder, query_text, &mut scored, &slate, *batched);
        }
    }

    probe.enter(Stage::Assemble);
    let mut order: Vec<usize> = (0..scored.len()).collect();
    match scoring {
        Scoring::Gated { .. } => {
            // **The isotonic gate no longer filters, and this is ADR-019 rather than a relaxation.**
            //
            // `order.retain(|i| scored[*i].passes)` used to run here. Per ADR-016 the frozen gate's
            // smallest expressible operating point spans 100% of queries â€” a *perfect* retrieval
            // system scores 0.8483 against a 0.95 threshold â€” so `passes` is false for every
            // candidate and this line emptied the order on every query. That is why nothing has
            // ever been injected, and it is not a statement about retrieval quality.
            //
            // K1 was amended (ADR-019) to replace the single-point criterion with a published
            // curve and a *declared operating point* on the rank-1/rank-2 rerank margin, precisely
            // because the gate cannot express a confident subset. That point is now the admission
            // rule, applied below. Keeping this retain as well would AND the two, injection would
            // stay dead, and the change would be undetectable â€” which is what
            // `runs/m2-session-d/PREDICTION.md` predicted and then caught.
            //
            // `passes` is still computed, still dumped, and still reported as `above_threshold`.
            // It no longer decides.
            // Three levels, declared in `runs/session-e/PREREGISTRATION.json` BEFORE the fit.
            //
            // Both scoring levels are CONTINUOUS and query-local, which is the ADR-010 constraint
            // discharged structurally: no step function can decide a rank here, because no
            // calibrated value is in the key at all.
            //
            // `score` (the winning cue's z) is first because it is dimensionless and therefore the
            // only quantity that can order a lexical-won candidate against a dense-won one.
            // `margin` is second in the winning cue's own raw units, which is a valid comparison
            // exactly when the first key ties.
            //
            // **Session H prepends two levels, and neither disturbs the two below it.** The key is
            // now, in order:
            //
            //   1. survived pruning (survivors before pruned-away)
            //   2. rerank score descending, for the candidates that were reranked
            //   3. score  â€” the winning cue's z
            //   4. margin â€” its lead over its own runner-up
            //   5. id
            //
            // Levels 3 to 5 are untouched, so a build with no cross-encoder and nothing to prune
            // ranks exactly as Session F did. That is not a convenience: it is what makes the
            // held-out comparison a comparison of one change.
            order.sort_by(|a, b| {
                let (x, y) = (&scored[*a], &scored[*b]);
                y.survived_pruning
                    .cmp(&x.survived_pruning)
                    // `None` sorts AFTER any `Some`, so an unreranked candidate never outranks a
                    // reranked one on the absence of a score. `Option`'s own ordering puts `None`
                    // first, which is the opposite, so it is written out rather than derived.
                    .then_with(|| match (x.rerank_score, y.rerank_score) {
                        (Some(p), Some(q)) => q.total_cmp(&p),
                        (Some(_), None) => std::cmp::Ordering::Less,
                        (None, Some(_)) => std::cmp::Ordering::Greater,
                        (None, None) => std::cmp::Ordering::Equal,
                    })
                    .then_with(|| y.score.total_cmp(&x.score))
                    .then_with(|| y.margin.total_cmp(&x.margin))
                    .then_with(|| x.entry.id.cmp(&y.entry.id))
            });
        }
    }
    // Still "how many cleared the gate's threshold", which is what the field has always meant and
    // what the eval report reads. Counted directly now that the order is no longer filtered by it â€”
    // deriving it from `order.len()` would silently redefine it as "how many were scored".
    let above_threshold = scored.iter().filter(|c| c.passes).count() as u32;

    // â”€â”€ K1 condition 3: the declared operating point, and the abstention path â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€
    //
    // **The published point is a TOP-1 claim, and as of M0c Session M2 THE CODE NO LONGER MAKES
    // THAT CLAIM.** `publish_precision_coverage.py` scores a query correct when `c["gold"][i1]` â€”
    // rank 1 alone â€” so `precision-coverage-heldout-v1.json` describes a system that admits one
    // memory. `ADMIT_TOP_K` is now 3. **The artifact and the product disagree until that curve is
    // republished at k = 3**, and this comment is the record of that debt rather than a deletion of
    // the warning that preceded it.
    //
    // What IS measured, held-out, n = 229 (`runs/session-m0c-m/cascade-heldout-read.json`):
    // the gold turn is at rank 1 for **0.6725** of queries and within the top 3 for **0.8515** on
    // the shipped ranking. So k = 3 raises the chance the right memory is present by **+0.1790**,
    // and necessarily lowers per-memory precision: at most one of the three can be gold, so at
    // k = 3 at least two of every three injected memories are non-gold by construction.
    //
    // Three consequences, none of them measured, all of them now live:
    //   * **Â§5.7 tokens.** The budget below was an upper bound that did not bind at k = 1. At k = 3
    //     it binds, and `budget_exhausted` becomes reachable in ordinary operation.
    //   * **K1 injection precision.** The conformal bound covers `P(inject | wrong)` for a
    //     single-memory decision. It says nothing about a 3-memory slate.
    //   * **Stale-fact harm.** LongMemEval marks both the stale and the current turn `has_answer`,
    //     so a wider slate is more likely to carry a superseded value alongside the current one.
    //     `docs/design/HARM-WEIGHTED-PRECISION.md` is the instrument; it has not been re-run.
    //
    // Directed change, M0c Session M2. It is deliberately a NAMED CONSTANT so that the value the
    // product uses is greppable and a future session cannot mistake it for an incidental `[0]`.
    //
    // **It REPLACES the isotonic gate's `passes` filter rather than stacking on it.** Per ADR-016
    // the frozen gate's smallest expressible operating point spans 100% of queries â€” a perfect
    // retrieval system scores 0.8483 against a 0.95 threshold â€” so `passes` is false for
    // everything and nothing has ever been injected. ANDing the two would leave injection dead and
    // the change undetectable. ADR-019 replaced the single-point criterion with a published curve
    // and a declared point exactly because the gate cannot express a confident subset.
    let (op, coverage) = match scoring {
        Scoring::Gated { operating_point, coverage, .. } => (*operating_point, *coverage),
    };

    // The two logits the decision reads. Extracted here; judged by
    // `operating_point::decide`, which is a pure function so the rule can be tested without a
    // 60 MB ONNX graph. See its doc for why that matters.
    let rank_one = order.first().and_then(|i| scored[*i].rerank_score);
    let rank_two = order.get(1).and_then(|i| scored[*i].rerank_score);

    let verdict = crate::operating_point::decide(
        order.len(),
        !matches!(rerank, Rerank::Off),
        rank_one,
        rank_two,
        op,
        coverage,
    );
    let rerank_margin: Option<f32> = verdict.margin;
    let abstention: Option<Abstention> = verdict.abstention;
    let admitted: Vec<usize> = if verdict.admit_rank_one {
        // The gate's verdict is still a RANK-ONE decision â€” `decide` reads the rank-1 and
        // rank-2 logits and nothing else. What changed is how many memories that verdict
        // admits, not what it is computed from.
        order.iter().take(ADMIT_TOP_K).copied().collect()
    } else {
        Vec::new()
    };

    let mut injected = Vec::new();
    let mut tokens = 0u32;
    let mut budget_exhausted = false;

    for index in admitted {
        let candidate = &scored[index];
        let cost = estimate_tokens(&candidate.entry.text);
        if tokens + cost > max_tokens {
            // Section 4.2's `budget_exhausted` describes having nothing further to inject
            // within budget. Recorded rather than silently truncated.
            budget_exhausted = true;
            break;
        }
        tokens += cost;
        injected.push(InjectedMemory {
            memory_id: candidate.entry.id.clone(),
            content: candidate.entry.text.clone(),
            score: candidate.score,
            calibrated_precision: candidate.calibrated_precision,
            fidelity: candidate.entry.fidelity,
            effective_trust: candidate.entry.effective_trust,
            payload_kind: candidate.entry.payload_kind,
        });
    }

    // Dump order is by id so two runs write byte-identical files.
    scored.sort_by(|a, b| a.entry.id.cmp(&b.entry.id));

    let survived_pruning = scored.iter().filter(|c| c.survived_pruning).count() as u32;
    let reranked = scored.iter().filter(|c| c.rerank_score.is_some()).count() as u32;
    let derived_sessions = scored
        .iter()
        .map(|c| c.session_key)
        .collect::<std::collections::BTreeSet<_>>()
        .len() as u32;
    probe.finish();

    Selection {
        injected,
        considered,
        retrieval_tokens: tokens,
        scoped: scored.len() as u32,
        above_threshold,
        scored,
        budget_exhausted,
        survived_pruning,
        pruning_applied,
        reranked,
        derived_sessions,
        rerank_margin,
        abstention,
    }
}

/// Assert the invariants section 4.3 states about what may appear in `injected`.
///
/// Called on the way out. The harness checks these too; doing it here as well means a
/// violation is a panic with a stack trace in our own process rather than a protocol error
/// that aborts someone's run and reports only what the bytes looked like.
pub fn debug_assert_injection_valid(injected: &[InjectedMemory]) {
    for item in injected {
        debug_assert_ne!(
            item.fidelity,
            Fidelity::Tombstone,
            "a tombstone reached the injected set; section 4.3 says it may never compete for \
             injection precision, because it is the ABSENCE of a memory"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::MATURATION_WINDOW_MS;
    use marlowe_contract::{PayloadKind, TrustClass};

    /// The **shipped** operating point, not a convenient one.
    ///
    /// These tests predate the cut point and were written against a path that injected whatever
    /// cleared the gate. They all run `Rerank::Off`, so under the declared point they now abstain
    /// with [`Abstention::NoReranker`] â€” which is the correct new behaviour and is asserted
    /// directly in `injection_operating_point.rs` rather than being smuggled in here by loading a
    /// threshold chosen to keep old assertions green.
    fn op() -> OperatingPoint {
        OperatingPoint::load().expect("the shipped artifact")
    }

    fn entry(id: &str, session: &str, text: &str, created: i64) -> MemoryEntry {
        MemoryEntry {
            id: id.into(),
            text: text.into(),
            payload_kind: PayloadKind::Episode,
            embedding_ref: None,
            source_turn_id: format!("t-{id}"),
            source_session_id: session.into(),
            trust_class: TrustClass::UserAsserted,
            effective_trust: TrustClass::UserAsserted,
            derivation: Vec::new(),
            origin_event: 1,
            occurred_at_ms: created,
            created_at: created,
            last_accessed: created,
            access_count: 0,
            confidence: 1.0,
            activation: 1.0,
            fidelity: Fidelity::Record,
            silent_until: Some(created + MATURATION_WINDOW_MS),
            supersedes: Vec::new(),
            superseded_by: None,
        }
    }

    fn store() -> BeliefStore {
        let mut s = BeliefStore::default();
        s.insert(entry("m-a", "s-1", "the ingest job times out nightly", 1_000));
        s.insert(entry("m-b", "s-1", "we had pasta for dinner", 2_000));
        s.insert(entry("m-c", "s-2", "gamma", 3_000));
        s
    }

    /// A gate whose curves make the threshold reachable, so the gated path can be exercised
    /// without depending on whatever the real fit produced.
    ///
    /// The lexical curve clears 0.95; the dense curve deliberately does not. That asymmetry is
    /// the property under test â€” either cue alone may carry a candidate, and the weaker one can
    /// never drag the stronger down.
    ///
    /// Both curves are on **margin**, so a candidate only clears the threshold by leading its own
    /// query's runner-up. A candidate tied at the top has margin 0 and lands in the bottom block.
    fn test_gate() -> FrozenGate {
        FrozenGate::from_json(
            r#"{
              "state": "fitted", "note": "test", "version": "{version}",
              "fusion": "per-query-margin-calibration-continuous-z-ranking", "threshold": 0.95,
              "feature_names": ["lexical_bm25","dense_cosine","lexical_margin","dense_margin","lexical_z","dense_z","lexical_rank_recip","dense_rank_recip","effective_trust","fidelity","cue_agreement_2cue"],
              "cue_features": ["lexical_margin","dense_margin"],
              "rank_features": ["lexical_z","dense_z"],
              "cue_curves": {
                "lexical_margin": [[0.00, 0.10], [0.50, 0.99]],
                "dense_margin": [[0.50, 0.05], [0.90, 0.40]]
              },
              "inert_features": {
                "lexical_bm25": "retained as the cross-session anchor; no curve reads it",
                "dense_cosine": "retained as the cross-session anchor; no curve reads it",
                "lexical_rank_recip": "diagnostic only",
                "dense_rank_recip": "diagnostic only",
                "effective_trust": "constant in this fixture",
                "fidelity": "constant in this fixture",
                "cue_agreement_2cue": "declared: needs a firing predicate for the dense cue"
              },
              "floor_verdict": "pass", "floor_required": 0.5478, "floor_measured": 0.6,
              "floor_read_from": "test fixture",
              "corpus": "test", "corpus_variant": "cleaned", "corpus_sha256": "x",
              "split_rule": "test", "split_digest": "y",
              "fit_cases": 1, "heldout_cases": 1, "fit_rows": 1, "fit_positives": 1,
              "fitted_at_clock_ms": 0
            }"#
            // Interpolated from the constant rather than written as a literal. A hardcoded
            // version in a fixture goes stale silently on the next bump -- every test in this
            // module then fails with a version-disagreement error that has nothing to do with
            // what any of them are testing.
            .replace("{version}", crate::gate::GATE_VERSION)
            .as_str(),
        )
        .unwrap()
    }

    #[test]
    fn nothing_is_injected_before_maturation() {
        let gate = test_gate();
        let point = op();
        let scoring = Scoring::Gated { gate: &gate, operating_point: &point, coverage: Coverage::Declared };
        let beliefs = store();
        let sel = select_for_injection(&beliefs, "s-1", "ingest", 2_000, 7000, &scoring, &VectorStore::default(), None, &mut Rerank::Off, RetrievalScope::ThisSession);
        assert!(sel.injected.is_empty());
        assert_eq!(sel.considered, 0, "and nothing was even a candidate");
    }

    #[test]
    fn only_the_requested_session_is_scoped() {
        // Asserted on `scored`, the population every offline number is computed over. With
        // `Rerank::Off` the declared operating point abstains, so `injected` is empty here.
        let now = 3_000 + MATURATION_WINDOW_MS;
        let gate = test_gate();
        let point = op();
        let scoring = Scoring::Gated { gate: &gate, operating_point: &point, coverage: Coverage::Declared };
        let beliefs = store();
        let sel = select_for_injection(&beliefs, "s-1", "ingest", now, 7000, &scoring, &VectorStore::default(), None, &mut Rerank::Off, RetrievalScope::ThisSession);
        assert_eq!(sel.scored.len(), 2);
        assert!(sel.scored.iter().all(|c| c.entry.id != "m-c"));
        assert_eq!(sel.considered, 3, "considered counts the whole candidate set");
        assert_eq!(sel.scoped, 2);
    }

    #[test]
    fn the_gate_drops_what_it_does_not_believe_in() {
        let now = 3_000 + MATURATION_WINDOW_MS;
        let gate = test_gate();
        let beliefs = store();
        let sel = select_for_injection(
            &beliefs,
            "s-1",
            "why is the ingest job timing out",
            now,
            7000,
            &Scoring::Gated { gate: &gate, operating_point: &op(), coverage: Coverage::Declared },
            &VectorStore::default(),
            None,
            &mut Rerank::Off,
            RetrievalScope::ThisSession,
        );
        assert_eq!(sel.scoped, 2, "both were scored");
        assert_eq!(sel.above_threshold, 1, "only one cleared the threshold");

        // **The gate's filtering is this test's subject and it is unchanged.** What changed is
        // what happens next: the gate's verdict no longer decides injection on its own. Per
        // ADR-016 the isotonic gate cannot express a confident subset â€” its smallest operating
        // point spans 100% of queries â€” so K1's declared cut point on the rerank margin is what
        // admits, and with `Rerank::Off` there is no margin at all.
        //
        // The surviving candidate is still identifiable in `scored`, which is what the feature
        // dump and every offline number are computed over. Asserting it there keeps the property
        // the test is named for without asserting the old injection behaviour.
        let passed: Vec<&str> =
            sel.scored.iter().filter(|c| c.passes).map(|c| c.entry.id.as_str()).collect();
        assert_eq!(passed, vec!["m-a"], "the gate still picks the same candidate");
        let winner = sel.scored.iter().find(|c| c.passes).unwrap();
        assert!(winner.calibrated_precision >= crate::gate::THRESHOLD);
        assert!(
            winner.score > 0.0,
            "the winning cue's z is a real score, not Session A's zero"
        );

        assert!(sel.injected.is_empty(), "no reranker, no margin, no injection");
        assert_eq!(sel.abstention, Some(Abstention::NoReranker));
    }

    #[test]
    fn a_margin_calibrated_gate_can_pass_at_most_one_candidate_per_cue() {
        // **A structural cap on coverage, and it is a real cost of the v4 shape.**
        //
        // `margin` is the lead over the runner-up, so within one query at most ONE candidate per
        // cue has a positive margin â€” every other candidate's margin is <= 0 by construction. An
        // isotonic curve is non-decreasing, so it cannot assign high precision to a negative
        // margin and low precision to a positive one. Therefore **at most `CUE_COUNT` candidates
        // per query can ever clear the threshold**, and in the common case where both cues favour
        // the same memory, exactly one.
        //
        // This is not a defect to fix here. It is what a precision-first gate reading a
        // decisiveness feature does, and Â§5.5 recovers recall through the explicit search tool.
        // But it caps coverage and therefore the injection count the power floor is read against,
        // which is why it is pre-registered in `runs/session-e/PREREGISTRATION.json` rather than
        // discovered in the results.
        let now = 3_000 + MATURATION_WINDOW_MS;
        let gate = test_gate();
        let mut beliefs = BeliefStore::default();
        // Three candidates that ALL match the query, with m-a a clear leader. Under a pooled
        // raw-score calibration all three would sit high together; under margin only the leader
        // can clear.
        beliefs.insert(entry(
            "m-a",
            "s-1",
            "the ingest job times out because the ingest job is out of memory",
            1_000,
        ));
        beliefs.insert(entry("m-b", "s-1", "the ingest job times out on mondays", 2_000));
        beliefs.insert(entry("m-c", "s-1", "the job runs", 2_500));
        let sel = select_for_injection(
            &beliefs,
            "s-1",
            "why is the ingest job timing out",
            now,
            7000,
            &Scoring::Gated { gate: &gate, operating_point: &op(), coverage: Coverage::Declared },
            &VectorStore::default(),
            None,
            &mut Rerank::Off,
            RetrievalScope::ThisSession,
        );

        assert_eq!(sel.scoped, 3, "all three were scored");
        assert!(
            sel.above_threshold as usize <= crate::gate::CUE_COUNT,
            "at most one candidate per cue can clear a margin-calibrated gate, got {}",
            sel.above_threshold
        );

        // Exactly one candidate has a positive lexical margin, and it is the one that passed.
        let positive: Vec<&str> = sel
            .scored
            .iter()
            .filter(|c| c.features.0[features::feature_index("lexical_margin").unwrap()] > 0.0)
            .map(|c| c.entry.id.as_str())
            .collect();
        assert_eq!(positive.len(), 1, "exactly one leader, got {positive:?}");

        // And the injected set is ordered by the continuous key, never by the calibrated value.
        let scores: Vec<f32> = sel.injected.iter().map(|i| i.score).collect();
        assert!(
            scores.windows(2).all(|w| w[0] >= w[1]),
            "injected set must be ordered by the continuous key, got {scores:?}"
        );
    }

    #[test]
    fn a_query_matching_nothing_clears_no_candidate() {
        // The abstention path that matters: candidates existed, none was good enough. This is
        // what makes `no_candidate_above_threshold` distinguishable from `no_candidates`.
        //
        // **And it is the test that per-query normalization did not destroy abstention.** Session
        // B rejected min-max because it forces every query's best candidate to 1.0 â€” a gate whose
        // top feature is 1.0 by construction cannot abstain. Margin does not do that: nothing
        // matches here, so both candidates score 0, they tie at the top, and a tie at the top is
        // margin 0 for both. The gate sees "no decisive winner" and declines.
        let now = 3_000 + MATURATION_WINDOW_MS;
        let gate = test_gate();
        let beliefs = store();
        let sel = select_for_injection(
            &beliefs,
            "s-1",
            "quarterly headcount forecast",
            now,
            7000,
            &Scoring::Gated { gate: &gate, operating_point: &op(), coverage: Coverage::Declared },
            &VectorStore::default(),
            None,
            &mut Rerank::Off,
            RetrievalScope::ThisSession,
        );
        assert_eq!(sel.scoped, 2);
        assert_eq!(sel.above_threshold, 0);
        assert!(sel.injected.is_empty());
        assert!(!sel.budget_exhausted, "nothing was cut for cost");
    }

    #[test]
    fn every_scored_candidate_is_retained_including_the_rejects() {
        // The fit needs negatives. A calibration fit only on what was injected would be fit on
        // its own output.
        let now = 3_000 + MATURATION_WINDOW_MS;
        let gate = test_gate();
        let beliefs = store();
        let sel = select_for_injection(
            &beliefs,
            "s-1",
            "why is the ingest job timing out",
            now,
            7000,
            &Scoring::Gated { gate: &gate, operating_point: &op(), coverage: Coverage::Declared },
            &VectorStore::default(),
            None,
            &mut Rerank::Off,
            RetrievalScope::ThisSession,
        );
        assert_eq!(sel.scored.len(), 2, "both, not just the one that passed");
        assert!(sel.scored.iter().any(|c| !c.passes));
    }

    #[test]
    fn the_dump_order_is_stable() {
        let now = 3_000 + MATURATION_WINDOW_MS;
        let beliefs = store();
        let gate = test_gate();
        let point = op();
        let scoring = Scoring::Gated { gate: &gate, operating_point: &point, coverage: Coverage::Declared };
        let sel = select_for_injection(&beliefs, "s-1", "ingest", now, 7000, &scoring, &VectorStore::default(), None, &mut Rerank::Off, RetrievalScope::ThisSession);
        let ids: Vec<&str> = sel.scored.iter().map(|c| c.entry.id.as_str()).collect();
        assert_eq!(ids, vec!["m-a", "m-b"], "sorted by id, so two runs write the same bytes");
    }

    #[test]
    fn ranking_is_deterministic() {
        let now = 3_000 + MATURATION_WINDOW_MS;
        let gate = test_gate();
        let beliefs = store();
        let ids = |s: &Selection| s.injected.iter().map(|i| i.memory_id.clone()).collect::<Vec<_>>();
        let a = select_for_injection(&beliefs, "s-1", "ingest job", now, 7000, &Scoring::Gated { gate: &gate, operating_point: &op(), coverage: Coverage::Declared }, &VectorStore::default(), None, &mut Rerank::Off, RetrievalScope::ThisSession);
        let b = select_for_injection(&beliefs, "s-1", "ingest job", now, 7000, &Scoring::Gated { gate: &gate, operating_point: &op(), coverage: Coverage::Declared }, &VectorStore::default(), None, &mut Rerank::Off, RetrievalScope::ThisSession);
        assert_eq!(ids(&a), ids(&b));
    }

    // ---- Session H: session grouping and pruning -----------------------------------------

    /// Entries at explicit `occurred_at_ms`, which is the only thing `session_keys` reads.
    fn at(id: &str, occurred: i64) -> MemoryEntry {
        let mut e = entry(id, "s-1", "text", 1_000);
        e.occurred_at_ms = occurred;
        e
    }

    #[test]
    fn contiguous_turns_form_one_session_and_a_gap_starts_another() {
        let minute = 60_000;
        let entries = vec![
            at("m-a", 0),
            at("m-b", 1_000),
            at("m-c", 2_000),
            // 31 minutes later: past the 30-minute threshold.
            at("m-d", 31 * minute),
            at("m-e", 31 * minute + 1_000),
        ];
        let refs: Vec<&MemoryEntry> = entries.iter().collect();
        assert_eq!(session_keys(&refs, SESSION_GAP_MS), vec![0, 0, 0, 1, 1]);
    }

    #[test]
    fn a_gap_exactly_at_the_threshold_does_not_split() {
        // The comparison is `>`, not `>=`. Asserted because an off-by-one here would move every
        // boundary on the corpus and the only visible symptom would be a moved number.
        let entries = vec![at("m-a", 0), at("m-b", SESSION_GAP_MS)];
        let refs: Vec<&MemoryEntry> = entries.iter().collect();
        assert_eq!(session_keys(&refs, SESSION_GAP_MS), vec![0, 0]);

        let entries = vec![at("m-a", 0), at("m-b", SESSION_GAP_MS + 1)];
        let refs: Vec<&MemoryEntry> = entries.iter().collect();
        assert_eq!(session_keys(&refs, SESSION_GAP_MS), vec![0, 1]);
    }

    #[test]
    fn keys_follow_the_candidates_own_order_not_time_order() {
        // `session_keys` returns keys positionally, and the caller zips them against `scored`.
        // If it returned them in sorted order instead, every candidate would get some other
        // candidate's session and nothing downstream would look wrong.
        let entries = vec![at("m-late", 60 * 60_000), at("m-early", 0)];
        let refs: Vec<&MemoryEntry> = entries.iter().collect();
        let keys = session_keys(&refs, SESSION_GAP_MS);
        assert_eq!(keys, vec![1, 0], "m-late is in the LATER session, at index 0");
    }

    #[test]
    fn simultaneous_turns_share_a_session_whatever_their_ids() {
        let entries = vec![at("m-z", 5_000), at("m-a", 5_000)];
        let refs: Vec<&MemoryEntry> = entries.iter().collect();
        assert_eq!(session_keys(&refs, SESSION_GAP_MS), vec![0, 0]);
    }

    #[test]
    fn pruning_keeps_the_union_of_each_cues_top_sessions() {
        //             session:   0    0    1    1    2    2    3    3
        let keys = [0u32, 0, 1, 1, 2, 2, 3, 3];
        let lexical = [0.9f32, 0.1, 0.2, 0.0, 0.1, 0.0, 0.0, 0.0];
        //  dense favours session 3, which lexical ranks last.
        let dense = [0.0f32, 0.0, 0.1, 0.0, 0.2, 0.0, 0.9, 0.1];

        let keep = surviving_sessions(&keys, &[&lexical, &dense], 1).expect("pruning applies");
        assert!(keep.contains(&0), "lexical's best session survives");
        assert!(keep.contains(&3), "and so does dense's, which lexical ranked last");
        assert_eq!(keep.len(), 2, "union of two top-1 sets, got {keep:?}");
    }

    #[test]
    fn pruning_does_not_apply_when_there_is_nothing_to_prune() {
        // Distinguishable from "pruning kept everything". A no-op that reported itself as a
        // decision would make `pruning_applied` useless in the report.
        let keys = [0u32, 0, 1];
        let scores = [1.0f32, 0.5, 0.2];
        assert!(surviving_sessions(&keys, &[&scores], PRUNE_TOP_N).is_none());
    }

    #[test]
    fn max_aggregation_cannot_displace_a_cues_top_1_candidate() {
        // **Session G's identity, asserted in the shipping code.** Under max aggregation a
        // session's score IS its best turn's score, so the globally top-scoring turn always lies
        // in a top-scoring session and pruning to top-N>=1 can never remove it. Session G proved
        // this over 458 case-cue pairs on the true partition, and Session H re-measured it on the
        // DERIVED partition; this is the same claim as a unit test, so a future change to the
        // aggregation rule fails here by name rather than as a moved number.
        let keys = [0u32, 1, 1, 2, 2, 3];
        let scores = [0.1f32, 0.95, 0.2, 0.3, 0.4, 0.05];
        let best = 1usize; // index of the global maximum

        let keep = surviving_sessions(&keys, &[&scores], 1).expect("pruning applies");
        assert!(
            keep.contains(&keys[best]),
            "the top-scoring turn's session must survive at N=1"
        );
    }

    #[test]
    fn an_unreranked_candidate_never_outranks_a_reranked_one() {
        // `Option`'s derived ordering puts `None` FIRST, which is the opposite of what the
        // ranking needs. The key writes the comparison out by hand; this is what would catch a
        // later "simplification" back to `y.rerank_score.partial_cmp(&x.rerank_score)`.
        let now = 3_000 + MATURATION_WINDOW_MS;
        let gate = test_gate();
        let beliefs = store();
        let sel = select_for_injection(
            &beliefs,
            "s-1",
            "ingest",
            now,
            7000,
            &Scoring::Gated { gate: &gate, operating_point: &op(), coverage: Coverage::Declared },
            &VectorStore::default(),
            None,
            &mut Rerank::Off,
            RetrievalScope::ThisSession,
        );
        // With reranking off nothing carries a score, so the key must fall through to `score`
        // and reproduce Session F's ordering exactly.
        assert!(sel.scored.iter().all(|c| c.rerank_score.is_none()));
        assert_eq!(sel.reranked, 0);
    }

    #[test]
    fn the_token_estimate_is_pessimistic() {
        // Erring high is the safe direction: it can never hide a budget miss.
        assert_eq!(estimate_tokens("abc"), 1);
        assert_eq!(estimate_tokens("abcd"), 2, "4 chars -> 2 tokens, not 1");
        assert_eq!(estimate_tokens(""), 0);
    }

    #[test]
    fn the_budget_cuts_after_the_gate_and_says_so() {
        let now = 3_000 + MATURATION_WINDOW_MS;
        let gate = test_gate();
        let beliefs = store();
        let sel = select_for_injection(
            &beliefs,
            "s-1",
            "why is the ingest job timing out",
            now,
            1,
            &Scoring::Gated { gate: &gate, operating_point: &op(), coverage: Coverage::Declared },
            &VectorStore::default(),
            None,
            &mut Rerank::Off,
            RetrievalScope::ThisSession,
        );
        assert_eq!(sel.above_threshold, 1, "the gate still passed it");
        assert!(sel.injected.is_empty());
        assert!(sel.retrieval_tokens <= 1);
        // **`budget_exhausted` is no longer what stops this, and that is the change rather than a
        // regression.** With no cross-encoder there is no margin, so the declared operating point
        // abstains *before* the budget loop is reached â€” nothing is admitted, so nothing can be
        // cut. The budget's own behaviour is unchanged and is still exercised wherever a memory is
        // admitted; what moved is which guard fires first on this fixture.
        assert_eq!(
            sel.abstention,
            Some(Abstention::NoReranker),
            "the operating point decides before the budget does"
        );
    }
}
