//! Gate features — **deliberately artifact-free**, and **per-query as of Session E**.
//!
//! This module knows how to turn a candidate set into numbers. It knows nothing about
//! calibration or a threshold. That separation is what makes the fit possible without a
//! bootstrap placeholder: `marlowe --dump-gate-features` runs *this* code and nothing in
//! `gate/mod.rs`, so there is never a moment where the binary needs a gate artifact in order
//! to produce the data the gate artifact is fit from.
//!
//! HP1 property 2: **features are user-specific even though the calibration is not.** Everything
//! here reads this profile's own data; the curves that read them are a build-time constant.
//!
//! # Why extraction is set-level now
//!
//! Through v3 a candidate's features were computed in isolation and the calibration was fit on
//! ~119,340 raw scores pooled across every fit query. That asks whether a candidate's *absolute*
//! BM25 or cosine predicts gold — which requires the two to be comparable **across** queries, and
//! they are not. [`crate::cue::lexical::BM25_SATURATION`] is deliberately an *absolute* map rather
//! than min-max (min-max would force every query's best candidate to 1.0 and destroy abstention),
//! so a query whose wording matches a lot of text has all its candidates scoring high and one with
//! unusual phrasing has all of them scoring low. The calibration's top block therefore filled with
//! candidates from high-scoring **queries** rather than high-scoring **matches**.
//!
//! Rank, margin and z do not exist for a candidate in isolation, so [`extract_all`] takes the
//! whole scored set. There is deliberately **no single-candidate `extract`**: one would have to
//! return zeros for every per-query feature, and a caller reaching for it would get a silently
//! unrankable candidate.
//!
//! # The three roles, and why there are three
//!
//! * [`CUE_FEATURES`] — **calibrated.** One isotonic curve each. These are the `_margin` features.
//! * [`RANK_FEATURES`] — **order the ranking.** Never calibrated. These are the `_z` features.
//! * everything else — **inert**, and the artifact must carry a stated reason for each.
//!
//! v3 had only two roles, and calling a feature that decides the ranking "inert" would be false.
//! `FrozenGate::load` refuses an artifact that leaves a feature out of all three, so a feature
//! cannot silently leave the ordering any more than it can silently leave the calibration.

use marlowe_contract::{Fidelity, TrustClass};

use crate::cue::lexical;
use crate::entry::MemoryEntry;

/// The feature vector's field order, and the **single source of that order**.
///
/// **v3, Session E.** Names change, so `FrozenGate::load`'s `FeatureNamesDisagree` refuses a v3
/// calibration under a v4 binary without anything else having to notice. That refusal is the most
/// valuable check in this module: features and curves living in two places with only one of them
/// checked is the exact shape of every unobservable mismatch this project has paid for.
///
/// `lexical_bm25` and `dense_cosine` are **retained rather than deleted**. They are inert — no
/// curve reads them — but they are the cross-session anchor `tools/score_longmemeval.py`'s
/// Number 3 sweeps and the quantity `analyze_cue_overlap.py`'s unchanged-cue check reads. Deleting
/// them would silently end the only comparison that can tell a changed cue set from a changed
/// held-out population.
pub const FEATURE_NAMES: [&str; 11] = [
    "lexical_bm25",
    "dense_cosine",
    "lexical_margin",
    "dense_margin",
    "lexical_z",
    "dense_z",
    "lexical_rank_recip",
    "dense_rank_recip",
    "effective_trust",
    "fidelity",
    "cue_agreement_2cue",
];

pub const FEATURE_COUNT: usize = FEATURE_NAMES.len();

/// One cue's three features, kept together so an index cannot drift between them.
///
/// Parallel arrays would let `CUE_FEATURES[1]`'s margin be read with `RANK_FEATURES[0]`'s z after
/// an edit, and every downstream number would still be produced. Grouping them makes that
/// impossible to express.
#[derive(Debug, Clone, Copy)]
pub struct CueSpec {
    /// The cue's short name, used in messages and in the artifact's curve map keys.
    pub cue: &'static str,
    /// The raw score. Inert — retained as the cross-session anchor.
    pub raw: &'static str,
    /// **Calibrated.** Margin over the runner-up, in raw score units.
    pub margin: &'static str,
    /// **Orders the ranking.** z against this query's own candidates. Never calibrated.
    pub z: &'static str,
}

/// The cues, in fusion order.
///
/// Cue 3 extends this array, which — together with the `cue_agreement_2cue` rename its denominator
/// forces — makes adding a cue a deliberate change that cannot happen without a re-fit.
pub const CUES: [CueSpec; 2] = [
    CueSpec {
        cue: "lexical",
        raw: "lexical_bm25",
        margin: "lexical_margin",
        z: "lexical_z",
    },
    CueSpec {
        cue: "dense",
        raw: "dense_cosine",
        margin: "dense_margin",
        z: "dense_z",
    },
];

/// The features the calibration reads, in [`CUES`] order. Asserted against [`CUES`] by test.
pub const CUE_FEATURES: [&str; 2] = ["lexical_margin", "dense_margin"];

/// The features the **ranking** reads, in [`CUES`] order. Asserted against [`CUES`] by test.
///
/// Dimensionless by construction, which is the whole reason the ordering reads these and not the
/// margins: a margin in BM25 units cannot be compared against a margin in cosine units, and the
/// ranking has to order a lexical-won candidate against a dense-won one.
pub const RANK_FEATURES: [&str; 2] = ["lexical_z", "dense_z"];

pub const CUE_COUNT: usize = CUE_FEATURES.len();

/// The position of a feature within [`FEATURE_NAMES`].
///
/// Returns `None` for a name this build does not extract, which `FrozenGate::load` turns into a
/// refusal rather than a skipped feature.
pub fn feature_index(name: &str) -> Option<usize> {
    FEATURE_NAMES.iter().position(|n| *n == name)
}

/// One candidate's features, in [`FEATURE_NAMES`] order.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FeatureVector(pub [f32; FEATURE_COUNT]);

impl FeatureVector {
    pub fn as_slice(&self) -> &[f32; FEATURE_COUNT] {
        &self.0
    }

    /// Pair each value with its name. Used by the feature dump, so the fitter reads names
    /// rather than positions and a reorder here cannot silently transpose the fit.
    pub fn named(&self) -> [(&'static str, f32); FEATURE_COUNT] {
        let mut out = [("", 0.0f32); FEATURE_COUNT];
        for (i, name) in FEATURE_NAMES.iter().enumerate() {
            out[i] = (name, self.0[i]);
        }
        out
    }
}

/// Trust as an ordinal in `[0, 1]`.
///
/// **`effective_trust` — the propagated value — never `trust_class`.** §3.3's worst-case
/// propagation is the whole point of the field, and a gate that scored the declared class
/// would hand a laundered memory its original trust back at the last step before injection.
fn trust_ordinal(trust: TrustClass) -> f32 {
    (trust as u8) as f32 / (TrustClass::UserAsserted as u8) as f32
}

/// Fidelity as an ordinal in `[0, 1]`.
///
/// `Tombstone` maps to 0.0 but can never appear: §4.3 excludes tombstones from the candidate
/// set before scoring, and `debug_assert_injection_valid` re-checks it on the way out.
fn fidelity_ordinal(fidelity: Fidelity) -> f32 {
    (fidelity as u8) as f32 / (Fidelity::Record as u8) as f32
}

/// One cue's within-query statistics, in the candidate set's own order.
struct PerQuery {
    margin: Vec<f32>,
    z: Vec<f32>,
    rank_recip: Vec<f32>,
}

/// Compute margin, z and reciprocal rank for one cue over one query's candidate set.
///
/// **Every reduction below runs in candidate-slice index order, and the slice order is the belief
/// store's own** (`BTreeMap::values()`, key-ordered — `HashMap` is banned project-wide by a
/// determinism guard). Floating-point summation is order-dependent, so this is the determinism
/// surface Session E adds and it is pinned rather than assumed. Accumulation is `f64` to keep the
/// rounding well below `f32` resolution; the order would make it reproducible either way.
fn per_query(scores: &[f32]) -> PerQuery {
    let n = scores.len();
    if n == 0 {
        return PerQuery {
            margin: Vec::new(),
            z: Vec::new(),
            rank_recip: Vec::new(),
        };
    }

    let mut sum = 0.0f64;
    for s in scores {
        sum += *s as f64;
    }
    let mean = sum / n as f64;

    let mut ss = 0.0f64;
    for s in scores {
        let d = *s as f64 - mean;
        ss += d * d;
    }
    let sd = (ss / n as f64).sqrt();

    // The two largest values, so `max over j != i` is one pass rather than n passes.
    let mut top1 = f32::NEG_INFINITY;
    let mut top2 = f32::NEG_INFINITY;
    for s in scores {
        if *s > top1 {
            top2 = top1;
            top1 = *s;
        } else if *s > top2 {
            top2 = *s;
        }
    }

    let mut margin = Vec::with_capacity(n);
    let mut z = Vec::with_capacity(n);
    for s in scores {
        // The runner-up *from this candidate's point of view*. A candidate that is not the unique
        // maximum competes against `top1`; the unique maximum competes against `top2`.
        //
        // Ties at the top therefore give margin 0.0 to **both** tied candidates, which is the
        // property this feature exists for: a query with no decisive winner has no decisive
        // winner, and the calibration should see that rather than a spurious lead.
        let runner_up = if *s >= top1 && top2 < top1 {
            // Sole maximum. With one candidate `top2` is -inf, and the honest runner-up is the
            // score of nothing at all: 0.0. Matching `retrieve::dense_for`'s rule that absent
            // evidence is worth 0.0 and never a skip.
            if top2.is_finite() {
                top2
            } else {
                0.0
            }
        } else {
            top1
        };
        margin.push(*s - runner_up);

        // **sigma = 0 is defined, not defaulted.** Every candidate scoring identically is the
        // common all-zero-BM25 query, not an edge case, and 0.0 is the honest value: this
        // candidate stands out from its competition by nothing.
        z.push(if sd > 0.0 {
            ((*s as f64 - mean) / sd) as f32
        } else {
            0.0
        });
    }

    // Reciprocal rank, a diagnostic only. Ordered by (score desc, index asc) so it is a total
    // order and two runs assign the same ranks.
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|a, b| scores[*b].total_cmp(&scores[*a]).then_with(|| a.cmp(b)));
    let mut rank_recip = vec![0.0f32; n];
    for (position, index) in order.into_iter().enumerate() {
        rank_recip[index] = 1.0 / (position + 1) as f32;
    }

    PerQuery {
        margin,
        z,
        rank_recip,
    }
}

/// Extract features for a whole query's candidate set.
///
/// `raw_bm25` comes from [`lexical::score_all`] and `dense_cosine` from
/// [`crate::cue::dense::cosine`], both computed over `entries` in the same order.
///
/// **The lexical margin and z are computed on RAW BM25, before saturation.** Identified before the
/// fit and it matters: `saturate(s) = s/(s+10)` compresses the top of the range, so at `s = 30 ->
/// 0.750` and `s = 40 -> 0.800` the margin is 0.050 while at `s = 0 -> 0.000` and `s = 1 -> 0.091`
/// it is 0.091. Computing the margin on the saturated value would make **low**-score margins look
/// **larger** than high-score ones — inverting the absolute-magnitude property the margin is
/// chosen for. Ranks are unaffected either way, because saturation is monotone.
pub fn extract_all(
    entries: &[&MemoryEntry],
    raw_bm25: &[f32],
    dense_cosine: &[f32],
) -> Vec<FeatureVector> {
    assert_eq!(
        entries.len(),
        raw_bm25.len(),
        "the lexical cue scored a different number of candidates than were passed"
    );
    assert_eq!(
        entries.len(),
        dense_cosine.len(),
        "the dense cue scored a different number of candidates than were passed"
    );

    let lex = per_query(raw_bm25);
    let den = per_query(dense_cosine);

    entries
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            let raw = raw_bm25[i];
            let dense = dense_cosine[i];

            // Still pinned to zero, and the reason is unchanged from Session D: making it
            // informative needs a *firing predicate* for the dense cue, and unlike BM25's
            // `raw > 0` any cosine floor is an unmeasured constant entering the frozen path.
            // Unpin at cue 3, with the predicate pre-registered first.
            let fired = u8::from(raw > 0.0) + u8::from(dense > 0.0);

            FeatureVector([
                lexical::saturate(raw),
                dense,
                lex.margin[i],
                den.margin[i],
                lex.z[i],
                den.z[i],
                lex.rank_recip[i],
                den.rank_recip[i],
                trust_ordinal(entry.effective_trust),
                fidelity_ordinal(entry.fidelity),
                f32::from(fired) / 2.0,
            ])
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::MATURATION_WINDOW_MS;
    use marlowe_contract::PayloadKind;

    fn entry(id: &str, trust: TrustClass, fidelity: Fidelity) -> MemoryEntry {
        MemoryEntry {
            id: id.into(),
            text: "the ingest job times out".into(),
            payload_kind: PayloadKind::Episode,
            embedding_ref: None,
            source_turn_id: "t-1".into(),
            source_session_id: "s-1".into(),
            // Deliberately the OPPOSITE of `effective_trust` in this fixture, so a test that
            // reads the wrong field fails loudly instead of coincidentally agreeing.
            trust_class: TrustClass::UserAsserted,
            effective_trust: trust,
            derivation: Vec::new(),
            origin_event: 1,
            occurred_at_ms: 1_000,
            created_at: 1_000,
            last_accessed: 1_000,
            access_count: 0,
            confidence: 1.0,
            activation: 1.0,
            fidelity,
            silent_until: Some(1_000 + MATURATION_WINDOW_MS),
            supersedes: Vec::new(),
            superseded_by: None,
        }
    }

    fn plain(n: usize) -> Vec<MemoryEntry> {
        (0..n)
            .map(|i| entry(&format!("m-{i}"), TrustClass::UserAsserted, Fidelity::Record))
            .collect()
    }

    fn refs(entries: &[MemoryEntry]) -> Vec<&MemoryEntry> {
        entries.iter().collect()
    }

    fn at(name: &str, v: &FeatureVector) -> f32 {
        v.0[feature_index(name).expect("known feature")]
    }

    // ------------------------------------------------------------------ the role declarations

    #[test]
    fn the_role_arrays_agree_with_the_cue_specs() {
        // Parallel arrays are the hazard `CueSpec` exists to remove; this is the check that they
        // have not drifted apart anyway.
        assert_eq!(CUES.len(), CUE_COUNT);
        for (i, spec) in CUES.iter().enumerate() {
            assert_eq!(CUE_FEATURES[i], spec.margin, "cue {} margin", spec.cue);
            assert_eq!(RANK_FEATURES[i], spec.z, "cue {} z", spec.cue);
            for name in [spec.raw, spec.margin, spec.z] {
                assert!(feature_index(name).is_some(), "{name} is not in FEATURE_NAMES");
            }
        }
    }

    #[test]
    fn no_feature_takes_two_roles() {
        for name in CUE_FEATURES {
            assert!(!RANK_FEATURES.contains(&name), "{name} is both calibrated and a rank key");
        }
    }

    #[test]
    fn feature_names_are_unique() {
        // A duplicate name would make `feature_index` return the first position for both, so one
        // of them would silently never be read.
        for (i, name) in FEATURE_NAMES.iter().enumerate() {
            assert_eq!(feature_index(name), Some(i), "{name} is duplicated");
        }
    }

    // ------------------------------------------------------------------ the per-query features

    #[test]
    fn margin_is_the_lead_over_the_runner_up_and_is_negative_for_everyone_else() {
        let e = plain(3);
        let f = extract_all(&refs(&e), &[5.0, 2.0, 1.0], &[0.0, 0.0, 0.0]);
        assert_eq!(at("lexical_margin", &f[0]), 3.0, "5 - 2, the runner-up");
        assert_eq!(at("lexical_margin", &f[1]), -3.0, "2 - 5, the leader");
        assert_eq!(at("lexical_margin", &f[2]), -4.0, "1 - 5");
    }

    #[test]
    fn a_tie_at_the_top_gives_both_candidates_zero_margin() {
        // The property the feature exists for: a query with no decisive winner must not report a
        // decisive lead for either of the tied candidates.
        let e = plain(3);
        let f = extract_all(&refs(&e), &[5.0, 5.0, 1.0], &[0.0, 0.0, 0.0]);
        assert_eq!(at("lexical_margin", &f[0]), 0.0);
        assert_eq!(at("lexical_margin", &f[1]), 0.0);
        assert_eq!(at("lexical_margin", &f[2]), -4.0);
    }

    #[test]
    fn a_lone_candidate_competes_against_nothing_worth_zero() {
        // Defined rather than left to fall out of an empty `top2`.
        let e = plain(1);
        let f = extract_all(&refs(&e), &[7.0], &[0.25]);
        assert_eq!(at("lexical_margin", &f[0]), 7.0, "7 - 0, the score of nothing");
        assert_eq!(at("dense_margin", &f[0]), 0.25);
        assert_eq!(at("lexical_z", &f[0]), 0.0, "one candidate has no spread");
    }

    #[test]
    fn margin_preserves_absolute_magnitude_which_is_why_it_is_the_calibrated_feature() {
        // Session B rejected min-max because it forces every query's best candidate to 1.0 and so
        // destroys abstention. This is the test that the calibrated feature does NOT do that: a
        // query where everything is near zero must produce a small margin, not a large one.
        let e = plain(3);
        let strong = extract_all(&refs(&e), &[40.0, 10.0, 1.0], &[0.0, 0.0, 0.0]);
        let weak = extract_all(&refs(&e), &[0.4, 0.1, 0.01], &[0.0, 0.0, 0.0]);
        assert!(
            at("lexical_margin", &strong[0]) > at("lexical_margin", &weak[0]) * 10.0,
            "a strong query's lead must dwarf a weak query's, or the gate cannot abstain"
        );
    }

    #[test]
    fn z_is_dimensionless_and_scale_invariant_which_is_why_it_is_the_RANK_key() {
        // The mirror of the test above, and the reason the two roles are split: z is invariant to
        // the query's scale, which makes it comparable ACROSS cues -- and unusable as the
        // threshold-bearing feature for exactly the same reason.
        let e = plain(3);
        let big = extract_all(&refs(&e), &[40.0, 10.0, 1.0], &[0.0, 0.0, 0.0]);
        let small = extract_all(&refs(&e), &[4.0, 1.0, 0.1], &[0.0, 0.0, 0.0]);
        for i in 0..3 {
            let d = (at("lexical_z", &big[i]) - at("lexical_z", &small[i])).abs();
            assert!(d < 1e-5, "z must not move when the query is scaled: index {i}, delta {d}");
        }
    }

    #[test]
    fn sigma_zero_yields_zero_not_a_division() {
        // The common all-zero-BM25 query. Every candidate identical -> no candidate stands out.
        let e = plain(4);
        let f = extract_all(&refs(&e), &[0.0, 0.0, 0.0, 0.0], &[0.5, 0.5, 0.5, 0.5]);
        for v in &f {
            assert_eq!(at("lexical_z", v), 0.0);
            assert_eq!(at("dense_z", v), 0.0);
            assert!(at("lexical_z", v).is_finite());
            assert!(at("dense_z", v).is_finite());
        }
    }

    #[test]
    fn z_sums_to_zero_and_has_unit_spread() {
        let e = plain(5);
        let f = extract_all(&refs(&e), &[9.0, 7.0, 4.0, 2.0, 1.0], &[0.0; 5]);
        let zs: Vec<f32> = f.iter().map(|v| at("lexical_z", v)).collect();
        let mean: f32 = zs.iter().sum::<f32>() / zs.len() as f32;
        assert!(mean.abs() < 1e-5, "z is mean-centred, got {mean}");
        let var: f32 = zs.iter().map(|z| z * z).sum::<f32>() / zs.len() as f32;
        assert!((var - 1.0).abs() < 1e-4, "z has unit variance, got {var}");
    }

    #[test]
    fn reciprocal_rank_is_a_total_order_even_under_ties() {
        let e = plain(4);
        let f = extract_all(&refs(&e), &[3.0, 3.0, 9.0, 1.0], &[0.0; 4]);
        // 9 first, then the two 3s in index order, then 1.
        assert_eq!(at("lexical_rank_recip", &f[2]), 1.0);
        assert_eq!(at("lexical_rank_recip", &f[0]), 0.5);
        assert!((at("lexical_rank_recip", &f[1]) - 1.0 / 3.0).abs() < 1e-6);
        assert_eq!(at("lexical_rank_recip", &f[3]), 0.25);
    }

    // ------------------------------------------------------------------ determinism

    #[test]
    fn extraction_is_bit_identical_across_calls() {
        // The determinism surface Session E adds: per-query features are a REDUCTION over the
        // candidate set, and floating-point summation is order-dependent. The order is pinned to
        // candidate-slice index order; this is the assertion that it stays pinned.
        let e = plain(64);
        let raw: Vec<f32> = (0..64).map(|i| (i as f32 * 0.37) % 11.0).collect();
        let dense: Vec<f32> = (0..64).map(|i| ((i as f32 * 0.11) % 1.0).abs()).collect();
        let a = extract_all(&refs(&e), &raw, &dense);
        let b = extract_all(&refs(&e), &raw, &dense);
        for (x, y) in a.iter().zip(b.iter()) {
            assert_eq!(x.0, y.0, "two extractions over the same set must be bit-identical");
        }
    }

    #[test]
    fn an_empty_candidate_set_extracts_nothing_rather_than_panicking() {
        let f = extract_all(&[], &[], &[]);
        assert!(f.is_empty());
    }

    // ------------------------------------------------------------------ the retained features

    #[test]
    fn the_raw_scores_are_retained_because_number_3_sweeps_them() {
        // Deleting them would silently end the only check that can tell a changed cue set from a
        // changed held-out population.
        let e = plain(2);
        let f = extract_all(&refs(&e), &[30.0, 0.0], &[0.62, 0.10]);
        assert!(at("lexical_bm25", &f[0]) > 0.7, "saturated, in [0,1]");
        assert_eq!(at("dense_cosine", &f[0]), 0.62, "raw cosine, unchanged");
    }

    #[test]
    fn the_gate_scores_effective_trust_not_declared_trust() {
        // The fixture declares `user_asserted` and propagates `untrusted_content`. A gate reading
        // the declared field would hand a laundered memory its original trust back at the last
        // step before injection.
        let e = vec![entry("m-a", TrustClass::UntrustedContent, Fidelity::Record)];
        let f = extract_all(&refs(&e), &[5.0], &[0.0]);
        assert_eq!(at("effective_trust", &f[0]), 0.0, "untrusted_content is the bottom");

        let e = vec![entry("m-a", TrustClass::UserAsserted, Fidelity::Record)];
        let f = extract_all(&refs(&e), &[5.0], &[0.0]);
        assert_eq!(at("effective_trust", &f[0]), 1.0);
    }

    #[test]
    fn ordinals_span_zero_to_one() {
        let e = vec![entry("m-a", TrustClass::AgentObserved, Fidelity::Summary)];
        let f = extract_all(&refs(&e), &[5.0], &[0.0]);
        assert!((at("effective_trust", &f[0]) - 2.0 / 3.0).abs() < 1e-6);
        assert!((at("fidelity", &f[0]) - 2.0 / 3.0).abs() < 1e-6);
    }

    #[test]
    fn cue_agreement_counts_how_many_of_the_two_cues_fired() {
        let e = plain(1);
        let one = |raw, dense| at("cue_agreement_2cue", &extract_all(&refs(&e), &[raw], &[dense])[0]);
        assert_eq!(one(0.0, 0.0), 0.0, "neither cue fired");
        assert_eq!(one(0.1, 0.0), 0.5, "lexical only");
        assert_eq!(one(0.0, 0.4), 0.5, "dense only");
        assert_eq!(one(0.1, 0.4), 1.0, "both");
    }

    #[test]
    fn names_and_values_stay_aligned() {
        let e = plain(1);
        let f = extract_all(&refs(&e), &[30.0], &[0.62]);
        let named = f[0].named();
        assert_eq!(FEATURE_COUNT, 11);
        for (i, name) in FEATURE_NAMES.iter().enumerate() {
            assert_eq!(named[i].0, *name);
            assert_eq!(named[i].1, f[0].0[i]);
        }
    }
}
