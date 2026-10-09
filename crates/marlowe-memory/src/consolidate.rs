//! Brief §5.2/§5.3 — consolidation, offline, as a **supersession edge**.
//!
//! Session F builds the minimum that answers one question: does merging near-duplicate turns
//! change what retrieval can reach? Everything else §5.3 names — contradiction resolution,
//! fidelity demotion, trend extraction, narrative gists — is deferred.
//!
//! # The merge creates no new belief, and that is the whole design
//!
//! A cluster of near-duplicates elects one of its **existing** members as the survivor and
//! writes [`EventKind::Superseded`] for the rest. §4.3's exclusion (2) then removes them from
//! the injection candidate set, which is the mechanism that already exists and is already
//! tested — this module writes the events and touches nothing downstream.
//!
//! This is not a measurement workaround. **HP5 already specifies it**: *"Merges are `supersedes`
//! edges and are therefore undoable."* Two properties follow, and the second is the one a later
//! session is most likely to trade away without noticing:
//!
//! 1. **Reversibility.** A merge is an appended edge over untouched beliefs, so an over-eager
//!    merge is inspectable and revertible. HP5's `over-eager merging -> detect + reverse` is
//!    structural rather than aspirational.
//! 2. **Every retrievable memory keeps the id ingest returned for exactly one turn.** M0a's
//!    `Attributor` builds its reverse map as `_turn_of[memory_id] = turn_id`, **last write
//!    wins**, and `evidence_precision` drops unattributable injections from its denominator
//!    entirely. A merge that minted a *new* id would therefore be scored against whichever
//!    constituent turn happened to be recorded last, silently — or, reported under no turn at
//!    all, would leave precision as a ratio over an empty set. Neither failure is visible in
//!    any number the harness prints.
//!
//! **The cost is real and is not corrected here.** When a cluster contains the benchmark's gold
//! turn and elects a different member, that case is lost. Under LongMemEval's per-turn evidence
//! key that is a genuine retrieval failure, not an attribution artifact, and "fixing" it would
//! be the implementation authoring its own measurement. It is counted and reported.
//!
//! # The survivor is the LATEST, not the earliest
//!
//! Elected by `(origin_event DESC, id ASC)` — the most recent statement wins.
//!
//! This is a correctness decision, not a convention. Supersession means a newer belief displaces
//! an older one, and LongMemEval's **knowledge-update** category is built on exactly that: the
//! gold turn is the *latest* statement of a fact whose earlier statements are distractors.
//! Electing the earliest member would systematically suppress gold in the one category whose
//! whole difficulty is recency, and the resulting damage would present as a retrieval-quality
//! problem with no visible cause.
//!
//! The election never reads gold, text length, or any score. It reads append order, which is
//! fixed before any query exists.
//!
//! # Offline, and where "offline" actually is
//!
//! Consolidation runs at the end of a §4.6 ingest call — session close, per §5.3 — on the
//! **ingest clock**, never a system clock (§4.5). No work lands on the §4.1 retrieval path, so
//! the 300 ms budget is untouched. The cost it does add is to ingest, which already sits under
//! a §4.0.7 30-second deadline; that cost is measured and reported rather than assumed small.
//!
//! # Planning and applying are separate functions
//!
//! [`plan`] is pure: beliefs and vectors in, a [`ConsolidationReport`] out, no journal and no
//! mutation. [`apply`] takes that report and writes it. The split is what lets the clustering
//! be unit-tested without a profile on disk, and it is what makes a dry run *structurally*
//! incapable of applying anything rather than merely choosing not to.

use std::collections::BTreeMap;

use marlowe_contract::{Clock, Fidelity};
use marlowe_journal::{Actor, AppendRequest, EventKind, Journal, Seq, TraceId};
use serde::{Deserialize, Serialize};

use crate::cue::dense::{cosine, vectors::VectorStore};
use crate::entry::{MemoryEntry, MemoryId};
use crate::error::MemoryError;
use crate::store::{BeliefStore, SupersededPayload};

/// The lowest threshold the dry-run sweep evaluates.
///
/// It bounds what the sweep can see, and nothing else: it never decides what is merged. A run
/// that applies consolidation reads its threshold from the frozen artifact, and [`Policy::load`]
/// rejects one below this floor — so the chosen point is always a point the sweep actually
/// measured, rather than an extrapolation off the end of it.
pub const DUMP_FLOOR: f32 = 0.70;

/// The thresholds a dry run clusters at.
///
/// **Declared here, in the binary, rather than passed in.** The sweep is the evidence the frozen
/// threshold is chosen from, and a caller that could choose the grid after seeing a first result
/// could walk the grid toward a number it liked. Widening this list is a code change with a diff.
pub const SWEEP_THRESHOLDS: &[f32] = &[
    0.70, 0.75, 0.80, 0.85, 0.88, 0.90, 0.92, 0.94, 0.95, 0.96, 0.97, 0.98, 0.99,
];

/// Bucket width for the dry run's full pairwise-similarity histogram.
///
/// The histogram exists so that [`DUMP_FLOOR`] being set too high is **visible** rather than
/// silently truncating the sweep. Dumping every pair would be ~121,000 rows per session; the
/// histogram is 50 integers and answers the only question the omitted rows would.
pub const HISTOGRAM_BUCKETS: usize = 50;

/// How a run consolidates.
///
/// Two states and deliberately no third. There is no "consolidation is optional" variant and no
/// default threshold: a permissive default here would let a run measure the unconsolidated
/// system under a consolidated label, which is precisely the class of silent mismatch CLAUDE.md
/// records this project as having paid for four times.
#[derive(Debug, Clone, Copy)]
pub enum Policy {
    /// Compute and report; **apply nothing, append nothing**. Used once, to produce the
    /// similarity distribution the frozen threshold is chosen from. Mirrors `--fit-mode`, which
    /// loads no gate for the same reason: the artifact cannot be derived from a run that already
    /// assumed it.
    DryRun,
    /// The shipping path. `threshold` comes from the frozen artifact and is never a literal.
    Frozen { threshold: f32 },
}

impl Policy {
    /// What the report stamps. A dry run must never be mistakable for a run that merged.
    pub fn label(self) -> &'static str {
        match self {
            Policy::DryRun => "dry-run-applied-nothing",
            Policy::Frozen { .. } => "frozen-applied",
        }
    }

    fn threshold(self) -> f32 {
        match self {
            Policy::DryRun => DUMP_FLOOR,
            Policy::Frozen { threshold } => threshold,
        }
    }
}

/// The merge threshold, embedded at compile time.
///
/// `include_str!` for the same reason the gate uses it: a missing file is a **compile** error
/// rather than a runtime one, and a released binary can never be separated from the threshold it
/// was measured with.
const ARTIFACT_JSON: &str = include_str!("../artifacts/consolidation-frozen-v1.json");

const ARTIFACT_PATH: &str = "crates/marlowe-memory/artifacts/consolidation-frozen-v1.json";

/// The artifact's on-disk shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConsolidationArtifact {
    pub version: String,
    /// `"unregistered"` until a pre-registration writes the threshold. There is no third state
    /// and no default: an artifact that is neither is a corrupt artifact, not a partial one.
    pub state: String,
    pub rule: String,
    pub threshold: Option<f32>,
    pub derived_from: Option<String>,
    /// The split the sweep was taken on. Checked against `tools/split.json` by the driver, so a
    /// threshold chosen on one split cannot be reported against another.
    pub split_digest: Option<String>,
    pub notes: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    #[error(
        "{ARTIFACT_PATH} does not parse: {0}. Consolidation has no default threshold; fix or \
         regenerate the artifact with `python tools/preregister_session_f.py`"
    )]
    Unparseable(#[from] serde_json::Error),

    #[error(
        "{ARTIFACT_PATH} is in state {found:?} and carries no threshold. This is the committed \
         placeholder. Run the dry-run pass, then `python tools/preregister_session_f.py`, then \
         rebuild. Refusing to run rather than inventing a merge threshold"
    )]
    Unregistered { found: String },

    #[error(
        "{ARTIFACT_PATH} is registered but omits {field:?}. A registered artifact missing a \
         field is corrupt, not partial"
    )]
    MissingField { field: &'static str },

    #[error(
        "{ARTIFACT_PATH} declares threshold {found}, which is below the dry run's DUMP_FLOOR of \
         {DUMP_FLOOR}. The sweep the threshold was chosen from never recorded pairs down there, \
         so the chosen point was not in the evidence"
    )]
    BelowDumpFloor { found: f32 },

    #[error(
        "{ARTIFACT_PATH} declares threshold {found}, which is not a similarity. Cosine here is \
         floored at zero and capped at one"
    )]
    NotASimilarity { found: f32 },
}

impl Policy {
    /// The shipping policy, from the embedded artifact.
    ///
    /// **No fallback.** A default threshold would let a build that never had a registered
    /// artifact merge anyway, and the run would produce a full set of numbers under a threshold
    /// nobody chose — the same failure shape `FrozenGate::load` refuses for weights.
    pub fn load() -> Result<Self, PolicyError> {
        Self::from_json(ARTIFACT_JSON)
    }

    pub fn from_json(json: &str) -> Result<Self, PolicyError> {
        let artifact: ConsolidationArtifact = serde_json::from_str(json)?;
        if artifact.state != "registered" {
            return Err(PolicyError::Unregistered {
                found: artifact.state,
            });
        }
        let threshold = artifact
            .threshold
            .ok_or(PolicyError::MissingField { field: "threshold" })?;
        artifact
            .derived_from
            .ok_or(PolicyError::MissingField { field: "derived_from" })?;
        artifact
            .split_digest
            .ok_or(PolicyError::MissingField { field: "split_digest" })?;
        if !(0.0..=1.0).contains(&threshold) || threshold.is_nan() {
            return Err(PolicyError::NotASimilarity { found: threshold });
        }
        if threshold < DUMP_FLOOR {
            return Err(PolicyError::BelowDumpFloor { found: threshold });
        }
        Ok(Policy::Frozen { threshold })
    }
}

/// One near-duplicate cluster.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cluster {
    pub representative: MemoryId,
    /// The members that were (or, in a dry run, would be) superseded. Sorted by id.
    pub merged: Vec<MemoryId>,
    /// The strongest edge inside the cluster.
    pub max_cosine: f32,
    /// The **weakest** edge that held it together. Single-link clustering is transitive, so a
    /// cluster can span a pair nowhere near the threshold; reporting only the maximum would hide
    /// exactly that. HP5 lists over-eager merging as a failure to *detect*, and this is the
    /// number that detects it.
    pub min_cosine: f32,
}

/// What one consolidation pass did, or would have done.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsolidationReport {
    pub session_id: String,
    pub policy: String,
    /// The linkage that produced `clusters`, or `"swept"` on a dry run, which carries both.
    pub linkage: String,
    pub threshold: Option<f32>,
    /// Live entries in this session that were eligible to be clustered.
    pub scanned: usize,
    /// Entries with no vector. They are scanned, kept, and never merged — see [`plan`].
    pub without_vector: usize,
    pub clusters: Vec<Cluster>,
    /// Total members superseded. The candidate-pool reduction, exactly.
    pub suppressed: usize,
    /// Dry run only: the same session clustered at every threshold in [`SWEEP_THRESHOLDS`].
    /// Empty on an applied pass, which carries exactly one clustering by definition.
    pub sweep: Vec<SweepPoint>,
    /// Dry run only: counts over all pairwise similarities, `HISTOGRAM_BUCKETS` wide across
    /// `[0, 1]`. Present so a badly chosen [`DUMP_FLOOR`] is observable.
    pub histogram: Vec<u64>,
}

/// One threshold's clustering of one session, in a dry run's sweep.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SweepPoint {
    pub threshold: f32,
    pub linkage: String,
    pub clusters: Vec<Cluster>,
    pub suppressed: usize,
    /// Members in the biggest cluster, including its representative. The single-link collapse is
    /// only visible here — `suppressed` alone cannot distinguish "many small merges" from "the
    /// whole session became one cluster", and those have opposite meanings.
    pub largest_cluster: usize,
}

/// The payload of a `BeliefsMerged` event.
///
/// Carries the `Seq` range the cluster spans, which is HP5's *temporal compression* detector:
/// *"A merged belief records the `Seq` range it covers. A belief whose derivation spans a wider
/// range than its claimed window is flagged."*
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BeliefsMergedPayload {
    pub representative: MemoryId,
    pub merged: Vec<MemoryId>,
    pub seq_first: Seq,
    pub seq_last: Seq,
    pub min_cosine: f32,
    pub max_cosine: f32,
}

/// The payload of a `ConsolidationRan` event.
///
/// §5.3: *"Consolidation is itself an episodic event. The system remembers having consolidated,
/// what it merged, and what it discarded."* Written even when it merged nothing — a pass that
/// found no duplicates is a fact about the history, and inferring it from the absence of
/// `BeliefsMerged` events would be indistinguishable from consolidation never having run.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConsolidationRanPayload {
    pub session_id: String,
    pub threshold: f32,
    pub scanned: usize,
    pub clusters: usize,
    pub suppressed: usize,
}

/// How a cluster is allowed to grow.
///
/// **Measured, not assumed — and single link was measured to be wrong here.** On the fit split,
/// jina's similarity distribution over conversational turns is strongly anisotropic: 99.3% of all
/// 30.6M pairs score ≥ 0.60 and 40.8% score ≥ 0.70, because sentence embeddings of chat turns
/// occupy a narrow cone rather than the whole sphere. Single link only needs a *chain* of pairwise
/// edges, so under that distribution it collapses a session: at threshold 0.90 it removes 60% of
/// the candidate pool and builds clusters of 62 members; at 0.70 it removes 99.8% and the largest
/// cluster is 616 turns — the entire session declared one near-duplicate.
///
/// A 616-member near-duplicate cluster is a broken rule, not a finding about the corpus, so the
/// shipping linkage is [`Linkage::Complete`]: a member joins only if it is within threshold of
/// **every** existing member. Both are swept in the dry run so the rejection stays on the record
/// rather than becoming folklore.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Linkage {
    /// Transitive: one edge above threshold joins two clusters. Measured to chain; kept only so
    /// the sweep can show why it was rejected.
    Single,
    /// Every pair inside a cluster is above threshold. This is what "these turns say the same
    /// thing" actually claims, and it is the only one of the two whose `min_cosine` is bounded
    /// below by the threshold.
    Complete,
}

impl Linkage {
    pub fn as_str(self) -> &'static str {
        match self {
            Linkage::Single => "single",
            Linkage::Complete => "complete",
        }
    }
}

/// The linkage a run that **applies** consolidation uses.
///
/// Not configurable from the CLI. The dry run sweeps both to record the comparison; the shipping
/// path has one rule, and a flag able to change it would be a quality knob outside HP1's frozen
/// artifact.
pub const APPLIED_LINKAGE: Linkage = Linkage::Complete;

/// One session's pairwise similarity structure — computed **once**, clustered many times.
///
/// The separation exists because the dry-run sweep clusters the same session at every threshold
/// in [`SWEEP_THRESHOLDS`], and recomputing ~121,000 cosines per threshold would multiply the
/// ingest cost by thirteen for no new information.
pub struct Similarities {
    ids: Vec<MemoryId>,
    origin: Vec<Seq>,
    /// `(i, j, cosine)` for every pair at or above [`DUMP_FLOOR`], `i < j`, in scan order.
    /// Pairs below the floor cannot affect any threshold the sweep evaluates, so they are
    /// counted into the histogram and then dropped.
    edges: Vec<(usize, usize, f32)>,
    /// Every pairwise similarity, bucketed. This is what makes a badly chosen [`DUMP_FLOOR`]
    /// visible instead of silently truncating the sweep.
    pub histogram: Vec<u64>,
    pub scanned: usize,
    pub without_vector: usize,
}

/// Compute a session's pairwise similarity structure. **Pure: mutates nothing.**
///
/// Determinism, which `repro --runs 2` covers byte for byte: candidates are taken in
/// [`BeliefStore`]'s own `BTreeMap` order (id ascending), and the scan is a fixed `i < j` double
/// loop over that order.
pub fn similarities(
    beliefs: &BeliefStore,
    session_id: &str,
    vectors: &VectorStore,
) -> Similarities {
    // Live beliefs in this session. **Not `injection_candidates`** — that filter also excludes
    // unmatured entries, and at ingest time every entry written in this call is unmatured, so
    // consolidation would see an empty set and silently do nothing. Maturation gates *unprompted
    // influence*, not existence, and consolidation is not injection.
    let scoped: Vec<&MemoryEntry> = beliefs
        .recall_candidates()
        .into_iter()
        .filter(|e| {
            e.source_session_id == session_id
                && e.superseded_by.is_none()
                && e.fidelity > Fidelity::Tombstone
        })
        .collect();

    let n = scoped.len();
    let ids: Vec<MemoryId> = scoped.iter().map(|e| e.id.clone()).collect();
    let origin: Vec<Seq> = scoped.iter().map(|e| e.origin_event).collect();
    // Resolved once. A candidate with no vector is not similar to anything: scoring it as
    // "no evidence" rather than dropping it keeps it in `scanned` and in the pool, which is the
    // same rule `retrieve::dense_for` applies and for the same reason — a silently shrinking
    // population is this project's unobservable-mismatch pattern.
    let vecs: Vec<Option<&[f32]>> = ids.iter().map(|id| vectors.get(id)).collect();
    let without_vector = vecs.iter().filter(|v| v.is_none()).count();

    let mut edges: Vec<(usize, usize, f32)> = Vec::new();
    let mut histogram = vec![0u64; HISTOGRAM_BUCKETS];
    for i in 0..n {
        let Some(vi) = vecs[i] else { continue };
        for j in (i + 1)..n {
            let Some(vj) = vecs[j] else { continue };
            let similarity = cosine(vi, vj);
            let bucket =
                ((similarity * HISTOGRAM_BUCKETS as f32) as usize).min(HISTOGRAM_BUCKETS - 1);
            histogram[bucket] += 1;
            if similarity >= DUMP_FLOOR {
                edges.push((i, j, similarity));
            }
        }
    }

    Similarities {
        ids,
        origin,
        edges,
        histogram,
        scanned: n,
        without_vector,
    }
}

impl Similarities {
    /// Cluster at one threshold under one linkage.
    ///
    /// Both linkages are deterministic by construction, which `repro --runs 2` covers byte for
    /// byte:
    ///
    /// * **single** unions into the numerically smaller index, so a component's root is a pure
    ///   function of the set rather than of the visit order. Union-by-rank would be faster and
    ///   would make the root depend on the order components happened to be built in.
    /// * **complete** walks candidate edges in descending similarity with `(i, j)` breaking ties,
    ///   and merges two clusters only when *every* cross pair clears the threshold. The sort is
    ///   total, so the agglomeration order is fixed.
    pub fn cluster(&self, threshold: f32, linkage: Linkage) -> Vec<Cluster> {
        let n = self.ids.len();
        // Similarity lookup, built only over recorded edges: a pair below `DUMP_FLOOR` reads as
        // absent. That is correct for both uses below, because `Policy::load` refuses a threshold
        // beneath the floor — so an absent pair is always a pair below the threshold.
        let mut lookup: BTreeMap<(usize, usize), f32> = BTreeMap::new();
        for (i, j, similarity) in &self.edges {
            lookup.insert((*i, *j), *similarity);
        }
        let similarity_of = |a: usize, b: usize| -> f32 {
            *lookup.get(&(a.min(b), a.max(b))).unwrap_or(&0.0)
        };

        let mut parent: Vec<usize> = (0..n).collect();
        fn find(parent: &mut [usize], mut x: usize) -> usize {
            while parent[x] != x {
                parent[x] = parent[parent[x]];
                x = parent[x];
            }
            x
        }

        match linkage {
            Linkage::Single => {
                for (i, j, similarity) in &self.edges {
                    if *similarity < threshold {
                        continue;
                    }
                    let (a, b) = (find(&mut parent, *i), find(&mut parent, *j));
                    if a != b {
                        parent[a.max(b)] = a.min(b);
                    }
                }
            }
            Linkage::Complete => {
                let mut candidates: Vec<(usize, usize, f32)> = self
                    .edges
                    .iter()
                    .copied()
                    .filter(|(_, _, s)| *s >= threshold)
                    .collect();
                // Descending similarity, then (i, j). A total order, so the agglomeration is
                // reproducible; `sort_by` is stable but the tiebreak makes that irrelevant.
                candidates.sort_by(|x, y| {
                    y.2.total_cmp(&x.2).then_with(|| (x.0, x.1).cmp(&(y.0, y.1)))
                });
                // Live membership per root, so the cross-pair check does not rescan everything.
                let mut group: BTreeMap<usize, Vec<usize>> =
                    (0..n).map(|i| (i, vec![i])).collect();
                for (i, j, _) in candidates {
                    let (a, b) = (find(&mut parent, i), find(&mut parent, j));
                    if a == b {
                        continue;
                    }
                    let (left, right) = (&group[&a], &group[&b]);
                    // **Every** cross pair, not just the one that proposed the merge. This is the
                    // whole difference from single link, and it is what bounds a cluster's
                    // diameter by the threshold instead of letting a chain grow without limit.
                    let joins = left
                        .iter()
                        .all(|x| right.iter().all(|y| similarity_of(*x, *y) >= threshold));
                    if !joins {
                        continue;
                    }
                    let (keep, drop) = (a.min(b), a.max(b));
                    parent[drop] = keep;
                    let moved = group.remove(&drop).unwrap_or_default();
                    let target = group.entry(keep).or_default();
                    target.extend(moved);
                    target.sort_unstable();
                }
            }
        }

        // Roots resolved once, after all unions. Reading a root mid-scan gives a stale answer.
        let roots: Vec<usize> = (0..n).map(|i| find(&mut parent, i)).collect();
        let mut members: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for (i, root) in roots.iter().enumerate() {
            members.entry(*root).or_default().push(i);
        }

        let mut clusters: Vec<Cluster> = Vec::new();
        for group in members.values() {
            if group.len() < 2 {
                continue;
            }
            // The survivor: latest append, then id ascending. See the module docstring — electing
            // the earliest would suppress gold across the whole knowledge-update category.
            let winner = *group
                .iter()
                .max_by(|a, b| {
                    self.origin[**a]
                        .cmp(&self.origin[**b])
                        .then_with(|| self.ids[**b].cmp(&self.ids[**a]))
                })
                .expect("a group of at least two has a maximum");
            let mut merged: Vec<usize> = group.iter().copied().filter(|x| *x != winner).collect();
            merged.sort_by(|a, b| self.ids[*a].cmp(&self.ids[*b]));

            // **Over ALL pairs inside the finished cluster, not over the edges that joined it.**
            // Every joining edge is above the threshold by construction, so a minimum taken over
            // those could never fall below it — the statistic would be structurally incapable of
            // showing the thing it exists to show. Single-link clustering is transitive: a~b~c
            // can put a and c in one cluster while the a-c pair sits far below the threshold,
            // and that is exactly HP5's *over-eager merging*.
            let mut min_cosine = 1.0f32;
            let mut max_cosine = 0.0f32;
            for (a, x) in group.iter().enumerate() {
                for y in group.iter().skip(a + 1) {
                    let similarity = similarity_of(*x, *y);
                    min_cosine = min_cosine.min(similarity);
                    max_cosine = max_cosine.max(similarity);
                }
            }
            clusters.push(Cluster {
                representative: self.ids[winner].clone(),
                merged: merged.iter().map(|x| self.ids[*x].clone()).collect(),
                max_cosine,
                min_cosine,
            });
        }
        clusters.sort_by(|a, b| a.representative.cmp(&b.representative));
        clusters
    }
}

/// Plan one consolidation pass over a session's live beliefs. **Pure: mutates nothing.**
pub fn plan(
    beliefs: &BeliefStore,
    session_id: &str,
    vectors: &VectorStore,
    policy: Policy,
) -> ConsolidationReport {
    let similarity = similarities(beliefs, session_id, vectors);
    let clusters = similarity.cluster(policy.threshold(), APPLIED_LINKAGE);
    let suppressed = clusters.iter().map(|c| c.merged.len()).sum();
    ConsolidationReport {
        session_id: session_id.to_string(),
        policy: policy.label().to_string(),
        linkage: APPLIED_LINKAGE.as_str().to_string(),
        threshold: match policy {
            Policy::DryRun => None,
            Policy::Frozen { threshold } => Some(threshold),
        },
        scanned: similarity.scanned,
        without_vector: similarity.without_vector,
        clusters,
        suppressed,
        sweep: Vec::new(),
        histogram: Vec::new(),
    }
}

/// The dry run: cluster one session at **every** threshold in [`SWEEP_THRESHOLDS`], and carry
/// the full similarity histogram.
///
/// This is the evidence `tools/preregister_session_f.py` chooses the frozen threshold from. It
/// applies nothing and journals nothing — [`ConsolidationReport::threshold`] stays `None`, which
/// is what [`apply`] refuses on.
pub fn dry_run(
    beliefs: &BeliefStore,
    session_id: &str,
    vectors: &VectorStore,
) -> ConsolidationReport {
    let similarity = similarities(beliefs, session_id, vectors);
    // **Both linkages, every threshold.** Single link is not the shipping rule and is swept
    // anyway: the reason it was rejected is a measurement on this corpus, and a rejected
    // alternative with no numbers beside it becomes folklore one session later.
    let mut sweep: Vec<SweepPoint> = Vec::new();
    for linkage in [Linkage::Complete, Linkage::Single] {
        for threshold in SWEEP_THRESHOLDS {
            let clusters = similarity.cluster(*threshold, linkage);
            sweep.push(SweepPoint {
                threshold: *threshold,
                linkage: linkage.as_str().to_string(),
                largest_cluster: clusters.iter().map(|c| c.merged.len() + 1).max().unwrap_or(0),
                suppressed: clusters.iter().map(|c| c.merged.len()).sum(),
                clusters,
            });
        }
    }
    ConsolidationReport {
        session_id: session_id.to_string(),
        policy: Policy::DryRun.label().to_string(),
        linkage: "swept".to_string(),
        threshold: None,
        scanned: similarity.scanned,
        without_vector: similarity.without_vector,
        clusters: Vec::new(),
        suppressed: 0,
        sweep,
        histogram: similarity.histogram,
    }
}

/// Journal a planned consolidation and fold it into the belief store.
///
/// **Refuses a dry-run report.** A dry run's whole purpose is to be the one pass that could not
/// have applied anything; letting its report through this function would delete that property
/// with a one-line call-site mistake.
pub fn apply(
    journal: &mut Journal,
    beliefs: &mut BeliefStore,
    report: &ConsolidationReport,
    clock: Clock,
) -> Result<(), MemoryError> {
    let Some(threshold) = report.threshold else {
        // Unreachable from `consolidate`, which is why it is an assertion rather than a silent
        // return: a future caller reaching here has confused the two policies.
        panic!("apply() was handed a dry-run report; a dry run applies nothing by construction");
    };
    let session_id = &report.session_id;

    let trace_id = TraceId::new_v5(&TraceId::NAMESPACE_OID, session_id.as_bytes());
    // Deterministic and derived from the session rather than generated: a replayed run must
    // produce the same actor string, or two identical runs give two different journals.
    let actor = Actor::Consolidation {
        run: format!("consolidate-{session_id}"),
    };

    for cluster in &report.clusters {
        let seqs: Vec<Seq> = std::iter::once(&cluster.representative)
            .chain(cluster.merged.iter())
            .filter_map(|id| beliefs.get(id).map(|e| e.origin_event))
            .collect();
        journal.append(
            clock,
            AppendRequest {
                trace_id,
                session_id: Some(session_id.clone()),
                run_id: None,
                actor: actor.clone(),
                kind: EventKind::BeliefsMerged,
                payload: serde_json::to_value(BeliefsMergedPayload {
                    representative: cluster.representative.clone(),
                    merged: cluster.merged.clone(),
                    // HP5's temporal-compression detector.
                    seq_first: seqs.iter().copied().min().unwrap_or(0),
                    seq_last: seqs.iter().copied().max().unwrap_or(0),
                    min_cosine: cluster.min_cosine,
                    max_cosine: cluster.max_cosine,
                })?,
            },
        )?;

        for loser in &cluster.merged {
            journal.append(
                clock,
                AppendRequest {
                    trace_id,
                    session_id: Some(session_id.clone()),
                    run_id: None,
                    actor: actor.clone(),
                    kind: EventKind::Superseded,
                    payload: serde_json::to_value(SupersededPayload {
                        id: loser.clone(),
                        by: cluster.representative.clone(),
                    })?,
                },
            )?;
            // The in-memory view, kept in step with the log. `BeliefStore::derive` folds the same
            // edge on rebuild, and `tests/ingest.rs` asserts the two agree — one derivation,
            // checked, rather than two that could drift.
            beliefs.supersede(loser, &cluster.representative);
        }
    }

    // Written even when nothing merged. A pass that found no duplicates is a fact about the
    // history; inferring it from the absence of `BeliefsMerged` would be indistinguishable from
    // consolidation never having run at all.
    journal.append(
        clock,
        AppendRequest {
            trace_id,
            session_id: Some(session_id.clone()),
            run_id: None,
            actor,
            kind: EventKind::ConsolidationRan,
            payload: serde_json::to_value(ConsolidationRanPayload {
                session_id: session_id.clone(),
                threshold,
                scanned: report.scanned,
                clusters: report.clusters.len(),
                suppressed: report.suppressed,
            })?,
        },
    )?;
    Ok(())
}

/// Plan, then apply unless the policy is a dry run.
pub fn consolidate(
    journal: &mut Journal,
    beliefs: &mut BeliefStore,
    session_id: &str,
    clock: Clock,
    vectors: &VectorStore,
    policy: Policy,
) -> Result<ConsolidationReport, MemoryError> {
    let report = plan(beliefs, session_id, vectors, policy);
    if !matches!(policy, Policy::DryRun) {
        apply(journal, beliefs, &report, clock)?;
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::MATURATION_WINDOW_MS;
    use marlowe_contract::{PayloadKind, TrustClass};

    fn entry(id: &str, seq: Seq) -> MemoryEntry {
        MemoryEntry {
            id: id.into(),
            text: format!("text of {id}"),
            payload_kind: PayloadKind::Episode,
            embedding_ref: None,
            source_turn_id: format!("t-{id}"),
            source_session_id: "s-1".into(),
            trust_class: TrustClass::UserAsserted,
            effective_trust: TrustClass::UserAsserted,
            derivation: Vec::new(),
            origin_event: seq,
            occurred_at_ms: 1_000,
            created_at: 1_000,
            last_accessed: 1_000,
            access_count: 0,
            confidence: 1.0,
            activation: 1.0,
            fidelity: Fidelity::Record,
            silent_until: Some(1_000 + MATURATION_WINDOW_MS),
            supersedes: Vec::new(),
            superseded_by: None,
        }
    }

    /// `(id, origin_event, basis direction)` — entries sharing a direction are identical vectors.
    fn fixture(rows: &[(&str, Seq, usize)]) -> (BeliefStore, VectorStore) {
        let mut beliefs = BeliefStore::default();
        let mut vectors = VectorStore::default();
        for (id, seq, direction) in rows {
            beliefs.insert(entry(id, *seq));
            let mut v = vec![0.0f32; crate::cue::dense::DIMENSIONS];
            v[*direction] = 1.0;
            vectors.insert_for_test(id, v);
        }
        (beliefs, vectors)
    }

    /// Fold a planned report into the store without a journal, mirroring `apply`'s store update.
    /// The journal path itself is covered end to end by `tests/ingest.rs`.
    fn fold(beliefs: &mut BeliefStore, report: &ConsolidationReport) {
        for cluster in &report.clusters {
            for loser in &cluster.merged {
                beliefs.supersede(loser, &cluster.representative);
            }
        }
    }

    #[test]
    fn a_dry_run_sweeps_every_threshold_and_marks_itself_unapplied() {
        // The property the two-state design rests on: the artifact is derived from a run that had
        // not already assumed it.
        let (beliefs, vectors) = fixture(&[("m-a", 1, 0), ("m-b", 2, 0), ("m-c", 3, 1)]);
        let report = dry_run(&beliefs, "s-1", &vectors);
        assert_eq!(report.policy, "dry-run-applied-nothing");
        assert_eq!(report.threshold, None, "and `apply` refuses on exactly this");
        // Both linkages, every threshold — single link is swept even though it does not ship, so
        // the measurement that rejected it stays on the record.
        assert_eq!(report.sweep.len(), 2 * SWEEP_THRESHOLDS.len());
        assert_eq!(report.linkage, "swept");
        // m-a and m-b are identical, so they merge everywhere under both linkages.
        for point in &report.sweep {
            assert_eq!(
                point.suppressed, 1,
                "at {} threshold {}",
                point.linkage, point.threshold
            );
            assert_eq!(point.largest_cluster, 2);
        }
        assert!(beliefs.get("m-a").unwrap().superseded_by.is_none());
    }

    // `apply` refusing a dry-run report needs a real journal, so it lives in
    // `tests/ingest.rs` alongside the other end-to-end journal properties.

    #[test]
    fn the_survivor_is_the_latest_not_the_earliest() {
        // Not a convention. LongMemEval's knowledge-update category makes the LATEST statement
        // gold; electing the earliest would suppress gold across the whole category and present
        // as an unexplained retrieval regression.
        let (mut beliefs, vectors) = fixture(&[("m-a", 1, 0), ("m-b", 2, 0), ("m-c", 3, 0)]);
        let report = plan(&beliefs, "s-1", &vectors, Policy::Frozen { threshold: 0.9 });
        assert_eq!(report.clusters.len(), 1);
        assert_eq!(report.clusters[0].representative, "m-c", "the newest survives");
        assert_eq!(report.clusters[0].merged, vec!["m-a", "m-b"]);
        fold(&mut beliefs, &report);
        assert_eq!(beliefs.get("m-c").unwrap().superseded_by, None);
        assert_eq!(beliefs.get("m-a").unwrap().superseded_by.as_deref(), Some("m-c"));
        assert_eq!(beliefs.get("m-c").unwrap().supersedes.len(), 2);
    }

    #[test]
    fn a_superseded_member_leaves_the_injection_candidate_set() {
        // §4.3 exclusion (2), which already existed. Consolidation writes the edge and changes
        // nothing downstream — this test asserts that claim rather than assuming it.
        let (mut beliefs, vectors) = fixture(&[("m-a", 1, 0), ("m-b", 2, 0)]);
        let matured = 1_000 + MATURATION_WINDOW_MS;
        assert_eq!(beliefs.injection_candidates(matured).len(), 2);
        let report = plan(&beliefs, "s-1", &vectors, Policy::Frozen { threshold: 0.9 });
        fold(&mut beliefs, &report);
        let live = beliefs.injection_candidates(matured);
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].id, "m-b");
        // And it stays reachable by explicit recall: the merge reduces accessibility, never
        // availability (§5.4).
        assert_eq!(beliefs.recall_candidates().len(), 2);
    }

    #[test]
    fn dissimilar_entries_are_never_merged() {
        let (beliefs, vectors) = fixture(&[("m-a", 1, 0), ("m-b", 2, 7), ("m-c", 3, 13)]);
        let report = plan(&beliefs, "s-1", &vectors, Policy::Frozen { threshold: 0.9 });
        assert!(report.clusters.is_empty());
        assert_eq!(report.suppressed, 0);
    }

    #[test]
    fn another_session_is_never_touched() {
        // Consolidation is scoped to the session that closed. A pass that clustered across the
        // whole store would merge one user's history into another's.
        let (mut beliefs, mut vectors) = fixture(&[("m-a", 1, 0)]);
        let mut other = entry("m-z", 2);
        other.source_session_id = "s-2".into();
        beliefs.insert(other);
        let mut v = vec![0.0f32; crate::cue::dense::DIMENSIONS];
        v[0] = 1.0;
        vectors.insert_for_test("m-z", v);
        let report = plan(&beliefs, "s-1", &vectors, Policy::Frozen { threshold: 0.9 });
        assert_eq!(report.scanned, 1, "only s-1's entry was in scope");
        assert!(report.clusters.is_empty());
    }

    #[test]
    fn consolidation_is_deterministic_across_runs() {
        // `repro --runs 2` compares two runs byte for byte and the injected set is in the hash.
        // Single-link clustering is order-sensitive unless the union rule is, which is why the
        // root is always the smaller index rather than the taller tree.
        let rows = &[
            ("m-a", 1, 0),
            ("m-b", 2, 0),
            ("m-c", 3, 0),
            ("m-d", 4, 5),
            ("m-e", 5, 5),
        ];
        let (first, first_vectors) = fixture(rows);
        let (second, second_vectors) = fixture(rows);
        let a = plan(&first, "s-1", &first_vectors, Policy::Frozen { threshold: 0.9 });
        let b = plan(&second, "s-1", &second_vectors, Policy::Frozen { threshold: 0.9 });
        assert_eq!(a.clusters, b.clusters);
        // Two clusters: {m-a, m-b, m-c} loses two members, {m-d, m-e} loses one.
        assert_eq!(a.clusters.len(), 2);
        assert_eq!(a.suppressed, 3);
    }

    #[test]
    fn an_entry_with_no_vector_is_kept_rather_than_merged_away() {
        // The same rule `retrieve::dense_for` applies: absent evidence is not evidence of
        // similarity. Dropping it from the pool would shrink the scored population with nothing
        // recording that it had been there.
        let (beliefs, mut vectors) = fixture(&[("m-a", 1, 0), ("m-b", 2, 0)]);
        vectors.forget_for_test("m-b");
        let report = plan(&beliefs, "s-1", &vectors, Policy::Frozen { threshold: 0.9 });
        assert_eq!(report.suppressed, 0);
        assert_eq!(report.scanned, 2, "still scanned, and still in the pool");
        assert_eq!(report.without_vector, 1, "and the absence is REPORTED, not silent");
    }

    /// A chain: adjacent pairs are similar, the two ends are not.
    fn chain() -> (BeliefStore, VectorStore) {
        let mut beliefs = BeliefStore::default();
        let mut vectors = VectorStore::default();
        let dims = crate::cue::dense::DIMENSIONS;
        for (index, id) in ["m-a", "m-b", "m-c"].iter().enumerate() {
            beliefs.insert(entry(id, index as Seq + 1));
            let angle = index as f32 * 0.30;
            let mut v = vec![0.0f32; dims];
            v[0] = angle.cos();
            v[1] = angle.sin();
            vectors.insert_for_test(id, v);
        }
        (beliefs, vectors)
    }

    #[test]
    fn single_link_chains_a_transitive_run_and_complete_link_does_not() {
        // The measured reason `APPLIED_LINKAGE` is `Complete`. On the fit split single link
        // removed 99.8% of the pool at threshold 0.70 and built a 616-member cluster, because
        // jina's similarity distribution over chat turns is anisotropic and one edge is enough to
        // join two clusters. This is that failure in miniature, asserted rather than described.
        let (beliefs, vectors) = chain();
        let similarity = similarities(&beliefs, "s-1", &vectors);

        let single = similarity.cluster(0.95, Linkage::Single);
        assert_eq!(single.len(), 1, "one edge below 0.95 still joined the ends");
        assert_eq!(single[0].merged.len(), 2);
        // HP5 lists over-eager merging as a failure to DETECT, and this is the number that
        // detects it: the weakest pair INSIDE the cluster sits below the threshold that built it.
        assert!(
            single[0].min_cosine < 0.95,
            "the chained pair must be visible: {:?}",
            single[0]
        );

        let complete = similarity.cluster(0.95, Linkage::Complete);
        assert_eq!(complete.len(), 1, "the adjacent pair still merges");
        assert_eq!(complete[0].merged.len(), 1, "but the far end is left out");
        assert!(
            complete[0].min_cosine >= 0.95,
            "complete link bounds a cluster's diameter by the threshold: {:?}",
            complete[0]
        );
    }

    #[test]
    fn the_applied_linkage_is_complete() {
        // A run that APPLIES consolidation must not be able to chain. Asserted so that changing
        // the constant is a deliberate act that breaks a test, not a quiet quality tweak.
        assert_eq!(APPLIED_LINKAGE, Linkage::Complete);
        let (beliefs, vectors) = chain();
        let report = plan(&beliefs, "s-1", &vectors, Policy::Frozen { threshold: 0.95 });
        assert_eq!(report.linkage, "complete");
        assert_eq!(report.suppressed, 1, "one merge, not a chained two");
    }

    #[test]
    fn the_histogram_makes_a_badly_chosen_dump_floor_visible() {
        // The histogram covers EVERY pair, including those below DUMP_FLOOR that the sweep drops.
        // Without it, a floor set above the whole distribution would produce an empty sweep that
        // looked exactly like a corpus with no duplicates.
        let (beliefs, vectors) = fixture(&[("m-a", 1, 0), ("m-b", 2, 7)]);
        let report = dry_run(&beliefs, "s-1", &vectors);
        assert_eq!(report.histogram.len(), HISTOGRAM_BUCKETS);
        assert_eq!(report.histogram.iter().sum::<u64>(), 1, "one pair, one count");
        assert_eq!(report.histogram[0], 1, "orthogonal, so in the bottom bucket");
        assert!(
            report.sweep.iter().all(|p| p.suppressed == 0),
            "and it is below every swept threshold, so nothing merges"
        );
    }
}
