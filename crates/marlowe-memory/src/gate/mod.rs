//! The frozen gate — HP1.
//!
//! Three properties, all of them load-bearing:
//!
//! 1. **The calibration is a build-time artifact**, fit offline on LongMemEval gold evidence.
//!    Nothing here learns at runtime and there is no code path that could.
//! 2. **Features are user-specific even though the calibration is not** — see [`features`].
//! 3. **The threshold is expressed in calibrated-precision units, not raw score.** [`THRESHOLD`]
//!    = 0.95 means *predicted precision ≥ 0.95*, which is what makes the operating point
//!    portable across profiles.
//!
//! # The shape, and why it changed in Session E
//!
//! `frozen-v3` calibrated each cue's **pooled raw score** and fused by taking the max. It failed
//! its floor — 0.4783 at top-1 against a required 0.5478 — and ADR-010 recorded why:
//!
//! > Isotonic calibration maps a continuous score to a step function. `max` over step functions
//! > has no resolution at the top, exactly where the operating point reads. Calibration puts cues
//! > in common units **by destroying the ordering inside each cue**. A fusion may use calibrated
//! > values to CHOOSE BETWEEN cues, but the ordering that decides the top of the ranking must come
//! > from a **continuous** score.
//!
//! `frozen-v5` obeys that constraint structurally rather than carefully:
//!
//! * the calibration reads **`{cue}_margin`** — the candidate's lead over its own runner-up, in
//!   raw score units. Query-local, so it no longer asks whether a candidate's *absolute* score
//!   predicts gold, which required BM25 and cosine to be comparable across queries when they are
//!   not. It decides **pass/fail** and **which cue speaks for a candidate**, and nothing else.
//! * the ranking reads **`{cue}_z`** — dimensionless, so a lexical-won candidate can be ordered
//!   against a dense-won one. Continuous and query-local.
//!
//! **Session D's failure mode is therefore impossible here, not merely mitigated**: a step
//! function never enters the ordering at all.
//!
//! Why the two roles are split rather than sharing one feature: within a query, raw score, z and
//! margin are all monotone transforms of one another and give the *identical* order, so the choice
//! only bites in two places and they want opposite properties. The ranking needs something
//! **dimensionless** (z). The threshold needs something that **preserves absolute magnitude** —
//! Session B rejected min-max normalization because it forces every query's best candidate to 1.0
//! and so destroys abstention, and σ-normalized z carries that defect in weaker form. Margin does
//! not: a query where everything is near zero has a tiny margin.
//!
//! **There is no default gate.** Every failure below is a load-time error naming the file and the
//! command that regenerates it. CLAUDE.md: *"Prefer a load-time error to a sensible default."* A
//! permissive fallback here would be the worst instance of that pattern in the project — a run
//! would report `frozen-v5` while scoring with a calibration nobody fit.

pub mod features;

use std::collections::BTreeMap;

use serde::Deserialize;

pub use features::{
    CueSpec, FeatureVector, CUES, CUE_COUNT, CUE_FEATURES, FEATURE_COUNT, FEATURE_NAMES,
    RANK_FEATURES,
};

/// The stamp a **calibrated** gate puts on `§4.2 gate.version`.
///
/// Never produced without a loaded, validated artifact. Asserted by test.
///
/// **`frozen-v5`, bumped in Session F — and NOT because the shape changed.** The feature vector,
/// the fusion and the ranking key are identical to v4. What changed is the **candidate set the
/// calibration is fit over**: §5.3 consolidation now supersedes near-duplicate members before
/// retrieval sees them, so v4 and v5 are curves over different populations.
///
/// Two artifacts fit over different pools must not carry one version. The failure that prevents is
/// the usual silent one — a v4 stamp on a v5 run produces a full set of numbers, and the only
/// symptom is a cross-session comparison that quietly is not one.
///
/// v4 was bumped from v3 for the other reason: both the feature vector and the combination
/// function changed. Earlier artifacts stay on disk as the provenance of Sessions B–E; none is
/// embedded.
pub const GATE_VERSION: &str = "frozen-v5";

/// The fusion this build implements, asserted against the artifact's own declaration.
///
/// A separate check rather than a comment because the failure it prevents is silent: a binary
/// reading a calibration fit under a different combination function would produce numbers for
/// every query. Session D added this check precisely because v2 and v3 shared feature *names*;
/// v4 changes the names too, but the check stays — the refusal set grows and never shrinks.
pub const FUSION: &str = "per-query-margin-calibration-continuous-z-ranking";

/// The stamp the feature-dump mode puts on `§4.2 gate.version`.
///
/// Same discipline as Session A's `ungated-v0`: a mode that computed features but calibrated
/// nothing must not be stampable as one that gated. A report reading `gate.version` has to be
/// able to tell which of the three states produced it.
pub const FIT_ONLY_VERSION: &str = "uncalibrated-fit-only";

/// K1's operating point, in calibrated-precision units.
///
/// **Frozen.** ROADMAP M10 is the only milestone that may move an operating point, and only by
/// beating this baseline. Lowering it because a run injected nothing is the tuning HP1 exists
/// to forbid — a low number at 0.95 is a result about the cue set.
pub const THRESHOLD: f32 = 0.95;

/// The artifact, embedded at compile time.
///
/// `include_str!` means a missing file is a **compile** error rather than a runtime one, and a
/// released binary can never be separated from the calibration it was measured with.
const ARTIFACT_JSON: &str = include_str!("../../artifacts/gate-frozen-v5.json");

const ARTIFACT_PATH: &str = "crates/marlowe-memory/artifacts/gate-frozen-v5.json";

#[derive(Debug, thiserror::Error)]
pub enum GateError {
    #[error(
        "{ARTIFACT_PATH} does not parse: {0}. The gate has no default calibration; fix or \
         regenerate the artifact with `python tools/fit_gate.py`"
    )]
    Unparseable(#[from] serde_json::Error),

    #[error(
        "{ARTIFACT_PATH} is in state {found:?} and carries no fitted calibration. This is the \
         committed placeholder, not a gate. Run `python tools/preregister_session_f.py` then \
         `python tools/fit_gate.py` and rebuild. Refusing to run rather than inventing a curve"
    )]
    Unfitted { found: String },

    #[error(
        "{ARTIFACT_PATH} is fitted but omits {field:?}. A fitted artifact missing a field is \
         corrupt, not partial"
    )]
    MissingField { field: &'static str },

    #[error(
        "{ARTIFACT_PATH} declares version {found:?}; this build is {GATE_VERSION}. The artifact \
         and the binary disagree about which gate this is"
    )]
    VersionDisagrees { found: String },

    #[error(
        "{ARTIFACT_PATH} declares fusion {found:?}; this build implements {FUSION}. Applying a \
         calibration fit under one combination function under another would produce a number for \
         every query with nothing observing the mismatch"
    )]
    FusionDisagrees { found: String },

    #[error(
        "{ARTIFACT_PATH} declares features {found:?}; this build extracts {expected:?}. The \
         calibration was fit against a different feature vector"
    )]
    FeatureNamesDisagree {
        found: Vec<String>,
        expected: Vec<String>,
    },

    #[error(
        "{ARTIFACT_PATH} declares cue features {found:?}; this build calibrates {expected:?}. An \
         artifact naming fewer cues would produce a gate that silently stopped reading one, and \
         every downstream number would still be produced"
    )]
    CueFeaturesDisagree {
        found: Vec<String>,
        expected: Vec<String>,
    },

    #[error(
        "{ARTIFACT_PATH} declares rank features {found:?}; this build ORDERS BY {expected:?}. \
         The ranking key is pre-registered before the fit, and an artifact disagreeing about it \
         describes a different experiment than the one that runs"
    )]
    RankFeaturesDisagree {
        found: Vec<String>,
        expected: Vec<String>,
    },

    #[error(
        "{ARTIFACT_PATH} declares cue {feature:?} but carries no curve for it. A cue with no \
         calibration cannot be read and must not be silently dropped"
    )]
    CueCurveMissing { feature: String },

    #[error(
        "{ARTIFACT_PATH} carries a curve for {feature:?}, which is not one of this build's cue \
         features. It would never be read, so the fit it represents is not the fit that runs"
    )]
    CueCurveExtra { feature: String },

    #[error(
        "{ARTIFACT_PATH} does not declare {feature:?} inert, and it is neither a cue feature nor \
         a rank feature. Every feature outside all three roles must carry a stated reason, so \
         that a feature dropping out of the gate is a decision on the record rather than an \
         omission"
    )]
    NonCueFeatureNotInert { feature: String },

    #[error(
        "{ARTIFACT_PATH} declares {feature:?} inert, but it is one of this build's cue features. \
         A cue cannot be both calibrated and inert"
    )]
    InertFeatureIsACue { feature: String },

    #[error(
        "{ARTIFACT_PATH} declares {feature:?} inert, but it is one of this build's RANK features \
         — it decides the ordering. Calling a feature that orders the ranking inert is false, and \
         it is the claim a reader would rely on when deciding what is safe to remove"
    )]
    InertFeatureIsARankFeature { feature: String },

    #[error(
        "{ARTIFACT_PATH} declares unknown feature {feature:?} inert. It matches no name in this \
         build's feature vector, so the declaration protects nothing"
    )]
    InertFeatureUnknown { feature: String },

    #[error(
        "{ARTIFACT_PATH}'s curve for {cue:?} is not monotone at breakpoint {index}: precision \
         goes {previous} -> {current}. An isotonic curve that decreases is a corrupt fit, and \
         reading a threshold off it would give a precision the fit never predicted"
    )]
    CurveNotMonotone {
        cue: String,
        index: usize,
        previous: f32,
        current: f32,
    },

    #[error(
        "{ARTIFACT_PATH}'s curve for {cue:?} is not sorted by score at index {index}: \
         {previous} -> {current}. The lookup is a binary search and would return the wrong bin"
    )]
    CurveNotSorted {
        cue: String,
        index: usize,
        previous: f32,
        current: f32,
    },

    #[error(
        "{ARTIFACT_PATH}'s curve for {cue:?} has two blocks at the same score {value} (index \
         {index}). `partition_point` cannot resolve which block owns that score, so the lookup \
         would be ambiguous. Margin has a large atom at 0 — every query whose candidates tie at \
         the top contributes one — so quantile bucketing produces this unless the fitter pools \
         buckets that share a score bound"
    )]
    CurveDuplicateBreakpoint {
        cue: String,
        index: usize,
        value: f32,
    },

    #[error(
        "{ARTIFACT_PATH}'s curve for {cue:?} predicts precision {value} at index {index}, which \
         is outside [0, 1]. Precision is a probability; a curve exceeding 1.0 would clear the \
         frozen threshold on arithmetic rather than on evidence"
    )]
    CurvePrecisionOutOfRange {
        cue: String,
        index: usize,
        value: f32,
    },

    #[error("{ARTIFACT_PATH}'s curve for {cue:?} is empty; there is nothing to calibrate against")]
    CurveEmpty { cue: String },

    #[error(
        "{ARTIFACT_PATH} declares threshold {found}, this build's operating point is \
         {THRESHOLD}. The threshold is frozen under HP1 and M10 is the only milestone that may \
         move it"
    )]
    ThresholdDisagrees { found: f32 },

    #[error(
        "{ARTIFACT_PATH} records floor_verdict {verdict:?}, and its calibration reaches \
         {reachable} against the frozen threshold of {THRESHOLD} — so this gate WOULD INJECT. A \
         shape measured below its own best single cue must not decide what reaches the model. \
         Fit a shape that passes the floor, or re-measure this one and record the verdict with \
         `python tools/analyze_cue_overlap.py --run <RUN> --record-verdict`"
    )]
    FailedFloorWouldInject { verdict: String, reachable: f32 },

    #[error(
        "{ARTIFACT_PATH} records floor_verdict {found:?}, which is not one of \
         \"pass\" | \"fail\" | \"unmeasured\". An unrecognised verdict cannot be checked, and a \
         gate whose floor status is unreadable is a gate with no floor"
    )]
    FloorVerdictUnrecognised { found: String },
}

/// The artifact as it sits on disk.
///
/// Fit-specific fields are `Option` **only** so the committed placeholder parses. Every one of
/// them is required by [`FrozenGate::from_artifact`] when `state` is `fitted`, and the error names
/// the missing field.
///
/// `deny_unknown_fields` is what makes each version boundary refuse in **both** directions: a v3
/// artifact under a v4 binary is missing `rank_features` and declares the wrong version, and a v4
/// artifact under a v3 binary hits unknown `rank_features`. Neither can be loaded by the wrong
/// build.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateArtifact {
    pub state: String,
    pub note: String,
    pub version: Option<String>,
    pub fusion: Option<String>,
    pub threshold: Option<f32>,
    pub feature_names: Option<Vec<String>>,
    /// The subset of `feature_names` the calibration reads, in order.
    pub cue_features: Option<Vec<String>>,
    /// The subset of `feature_names` the **ranking** reads, in order. Never calibrated.
    pub rank_features: Option<Vec<String>>,
    /// cue feature name -> ascending `[score_upper, precision]` pairs.
    pub cue_curves: Option<BTreeMap<String, Vec<[f32; 2]>>>,
    /// feature name -> why it takes no part in the calibration or the ranking.
    pub inert_features: Option<BTreeMap<String, String>>,
    /// `"pass" | "fail" | "unmeasured"` — the pre-registered floor condition's verdict for the
    /// shape this artifact implements. See [`FrozenGate::from_artifact`].
    pub floor_verdict: Option<String>,
    /// What the floor required, what was measured, and where it was read. Carried so the verdict
    /// travels with its evidence instead of being a bare word.
    pub floor_required: Option<f32>,
    pub floor_measured: Option<f32>,
    pub floor_read_from: Option<String>,
    pub corpus: Option<String>,
    pub corpus_variant: Option<String>,
    pub corpus_sha256: Option<String>,
    pub split_rule: Option<String>,
    pub split_digest: Option<String>,
    pub fit_cases: Option<u32>,
    pub heldout_cases: Option<u32>,
    pub fit_rows: Option<u32>,
    pub fit_positives: Option<u32>,
    pub fitted_at_clock_ms: Option<i64>,
}

/// One cue's calibration, with the feature positions resolved once at load.
#[derive(Debug, Clone)]
pub struct CueCurve {
    /// The cue's short name (`"lexical"`, `"dense"`).
    pub cue: &'static str,
    /// The **calibrated** feature's name — the artifact's curve-map key.
    pub name: String,
    /// Position of the calibrated (margin) feature within [`FEATURE_NAMES`].
    pub margin_index: usize,
    /// Position of the ranking (z) feature within [`FEATURE_NAMES`].
    pub z_index: usize,
    /// Ascending `[score_upper, precision]`, strictly increasing in score.
    pub curve: Vec<[f32; 2]>,
}

/// A validated, frozen gate.
#[derive(Debug, Clone)]
pub struct FrozenGate {
    cues: Vec<CueCurve>,
    threshold: f32,
    provenance: Provenance,
}

/// What the gate was fit on. Reported by `--dev` diagnostics and by the scoring driver, so a
/// number and the artifact that produced it can always be joined.
#[derive(Debug, Clone)]
pub struct Provenance {
    pub corpus: String,
    pub corpus_variant: String,
    pub corpus_sha256: String,
    pub split_rule: String,
    pub split_digest: String,
    pub fit_cases: u32,
    pub heldout_cases: u32,
    pub inert_features: BTreeMap<String, String>,
    /// `"pass" | "fail" | "unmeasured"`, with the evidence it was read from. Reported by `--dev`
    /// so a run and its floor status travel together.
    pub floor_verdict: String,
    pub floor_required: Option<f32>,
    pub floor_measured: Option<f32>,
    pub floor_read_from: Option<String>,
}

/// One candidate's gate verdict.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Verdict {
    /// The winning cue's **within-query z**. On the wire as `injected[].score`.
    ///
    /// **This field's meaning has now moved twice without the name changing**, which is exactly
    /// what a later reader would take for "unchanged". Through v2 it was a squashed linear
    /// logistic; in v3 it was a percentile within the winning cue's curve; it is now a z-score.
    ///
    /// The reason it is on the wire is the same each time and it is verifiable: it is the
    /// **primary ranking key**, so a driver-side reader must have it to reproduce the gate's own
    /// ordering without re-implementing the gate in Python. (`CONTRACTS.md` §4 does not constrain
    /// `score` semantically — §4.2b types it `f32`, there is no range or monotonicity rule, and
    /// `validate.py` never inspects it. §4.4 mentions it only to say the threshold is in
    /// calibrated-precision units, "not score".)
    pub score: f32,
    /// The winning cue's **margin over its runner-up**, in that cue's raw units. The ranking key's
    /// second level. Not on the wire; carried in the feature dump for the same reason as `score`.
    pub margin: f32,
    /// **max** over the per-cue calibrated precisions. The value the threshold is compared
    /// against, and nothing else — under v4 it does not order anything.
    pub calibrated_precision: f32,
    /// Which cue's opinion won. Diagnostic; carried into the dump so a per-cue breakdown of the
    /// injected set does not have to be reconstructed by guessing.
    pub winning_cue: &'static str,
    pub passes: bool,
}

impl FrozenGate {
    /// Load and validate the embedded artifact. **The only constructor.**
    pub fn load() -> Result<Self, GateError> {
        Self::from_json(ARTIFACT_JSON)
    }

    pub fn from_json(json: &str) -> Result<Self, GateError> {
        let artifact: GateArtifact = serde_json::from_str(json)?;
        Self::from_artifact(artifact)
    }

    pub fn from_artifact(artifact: GateArtifact) -> Result<Self, GateError> {
        if artifact.state != "fitted" {
            return Err(GateError::Unfitted {
                found: artifact.state,
            });
        }

        fn required<T>(value: Option<T>, field: &'static str) -> Result<T, GateError> {
            value.ok_or(GateError::MissingField { field })
        }

        let version = required(artifact.version, "version")?;
        if version != GATE_VERSION {
            return Err(GateError::VersionDisagrees { found: version });
        }

        let fusion = required(artifact.fusion, "fusion")?;
        if fusion != FUSION {
            return Err(GateError::FusionDisagrees { found: fusion });
        }

        let threshold = required(artifact.threshold, "threshold")?;
        if (threshold - THRESHOLD).abs() > f32::EPSILON {
            return Err(GateError::ThresholdDisagrees { found: threshold });
        }

        // Names, in order, against the array that produced the values -- not a count, which
        // would pass a transposition.
        let names = required(artifact.feature_names, "feature_names")?;
        if names.len() != FEATURE_COUNT || names.iter().zip(FEATURE_NAMES).any(|(a, b)| a != b) {
            return Err(GateError::FeatureNamesDisagree {
                found: names,
                expected: FEATURE_NAMES.iter().map(|s| s.to_string()).collect(),
            });
        }

        // Same treatment for the calibrated subset, and for the same reason: an artifact naming
        // one cue would produce a gate that quietly stopped reading the other.
        let cue_names = required(artifact.cue_features, "cue_features")?;
        if cue_names.len() != CUE_COUNT || cue_names.iter().zip(CUE_FEATURES).any(|(a, b)| a != b) {
            return Err(GateError::CueFeaturesDisagree {
                found: cue_names,
                expected: CUE_FEATURES.iter().map(|s| s.to_string()).collect(),
            });
        }

        // ...and for the ORDERING subset. New in v4, and it is not decoration: the ranking key is
        // pre-registered before the fit precisely so it cannot be chosen with the top-1 number in
        // view, and an artifact that disagrees about it describes a different experiment.
        let rank_names = required(artifact.rank_features, "rank_features")?;
        if rank_names.len() != CUE_COUNT || rank_names.iter().zip(RANK_FEATURES).any(|(a, b)| a != b)
        {
            return Err(GateError::RankFeaturesDisagree {
                found: rank_names,
                expected: RANK_FEATURES.iter().map(|s| s.to_string()).collect(),
            });
        }

        let mut curves = required(artifact.cue_curves, "cue_curves")?;

        // Every cue needs a curve, and the margin/z index pair is resolved from `CUES` so the two
        // cannot drift apart.
        let mut cues: Vec<CueCurve> = Vec::with_capacity(CUE_COUNT);
        for spec in CUES.iter() {
            let curve = curves
                .remove(spec.margin)
                .ok_or_else(|| GateError::CueCurveMissing {
                    feature: spec.margin.to_string(),
                })?;
            validate_curve(spec.margin, &curve)?;
            cues.push(CueCurve {
                cue: spec.cue,
                name: spec.margin.to_string(),
                margin_index: features::feature_index(spec.margin)
                    .expect("CUES names are asserted against FEATURE_NAMES by test"),
                z_index: features::feature_index(spec.z)
                    .expect("CUES names are asserted against FEATURE_NAMES by test"),
                curve,
            });
        }
        // ...and nothing may carry a curve that is never read.
        if let Some(extra) = curves.keys().next() {
            return Err(GateError::CueCurveExtra {
                feature: extra.clone(),
            });
        }

        // Every feature outside ALL THREE roles must carry a stated reason. v2 checked that a
        // pinned weight was zero; with no weight vector the property is COVERAGE -- a feature
        // cannot leave the gate silently. v4 adds the third role, because calling a feature that
        // decides the ordering "inert" would be a false claim on the record.
        let inert = required(artifact.inert_features, "inert_features")?;
        for feature in inert.keys() {
            if features::feature_index(feature).is_none() {
                return Err(GateError::InertFeatureUnknown {
                    feature: feature.clone(),
                });
            }
            if CUE_FEATURES.contains(&feature.as_str()) {
                return Err(GateError::InertFeatureIsACue {
                    feature: feature.clone(),
                });
            }
            if RANK_FEATURES.contains(&feature.as_str()) {
                return Err(GateError::InertFeatureIsARankFeature {
                    feature: feature.clone(),
                });
            }
        }
        for name in FEATURE_NAMES {
            let covered = CUE_FEATURES.contains(&name)
                || RANK_FEATURES.contains(&name)
                || inert.contains_key(name);
            if !covered {
                return Err(GateError::NonCueFeatureNotInert {
                    feature: name.to_string(),
                });
            }
        }

        // ---------------------------------------------------------------- the floor interlock
        //
        // Session D's fusion FAILED its pre-registered floor: 0.4783 at top-1 against a required
        // 0.5478, worse than its own best single cue. It stayed embedded only because its
        // calibration topped out at 0.3090 against a frozen 0.95, so it injected nothing.
        //
        // **That argument expires exactly when a session succeeds**, and Session E's whole goal is
        // to make the gate inject. Leaving the swap to a session remembering to do it is the
        // failure mode this project keeps paying for, so it is an interlock instead: a failed
        // floor and a calibration that would inject cannot coexist in a loadable artifact.
        //
        // The check is on `"fail"` only, not on `"unmeasured"`. Refusing "unmeasured" would
        // deadlock: the floor is read from a scoring run's feature dump, which requires this gate
        // to load in order to produce it.
        let verdict = required(artifact.floor_verdict, "floor_verdict")?;
        if !matches!(verdict.as_str(), "pass" | "fail" | "unmeasured") {
            return Err(GateError::FloorVerdictUnrecognised { found: verdict });
        }
        let reachable = cues
            .iter()
            .filter_map(|c| c.curve.last().map(|bp| bp[1]))
            .fold(0.0f32, f32::max);
        if verdict == "fail" && reachable >= threshold {
            return Err(GateError::FailedFloorWouldInject { verdict, reachable });
        }

        Ok(Self {
            cues,
            threshold,
            provenance: Provenance {
                corpus: required(artifact.corpus, "corpus")?,
                corpus_variant: required(artifact.corpus_variant, "corpus_variant")?,
                corpus_sha256: required(artifact.corpus_sha256, "corpus_sha256")?,
                split_rule: required(artifact.split_rule, "split_rule")?,
                split_digest: required(artifact.split_digest, "split_digest")?,
                fit_cases: required(artifact.fit_cases, "fit_cases")?,
                heldout_cases: required(artifact.heldout_cases, "heldout_cases")?,
                inert_features: inert,
                floor_verdict: verdict,
                floor_required: artifact.floor_required,
                floor_measured: artifact.floor_measured,
                floor_read_from: artifact.floor_read_from,
            },
        })
    }

    pub fn threshold(&self) -> f32 {
        self.threshold
    }

    pub fn provenance(&self) -> &Provenance {
        &self.provenance
    }

    pub fn cues(&self) -> &[CueCurve] {
        &self.cues
    }

    /// One cue's calibration lookup: margin → predicted precision.
    ///
    /// Below the first breakpoint returns the first block; above the last returns the last. Both
    /// are the fit's own predictions at the extremes rather than extrapolations — isotonic
    /// regression is a step function and does not extrapolate.
    pub fn calibrate_cue(curve: &[[f32; 2]], value: f32) -> f32 {
        let index = curve.partition_point(|bp| bp[0] < value);
        curve[index.min(curve.len() - 1)][1]
    }

    /// The verdict: calibrate each cue's margin, take the cue with the highest predicted
    /// precision, and report **that cue's continuous z and margin** for the ranking.
    ///
    /// This is ADR-010's permitted use of calibration and only that use. The calibrated value
    /// answers two yes/no-shaped questions — *does this candidate clear the operating point* and
    /// *which cue speaks for it* — and never orders anything.
    ///
    /// Cue selection ties resolve on higher z, then on `CUES` order, so the verdict is a pure
    /// function of the features with no dependence on map iteration order.
    pub fn judge(&self, f: &FeatureVector) -> Verdict {
        let values = f.as_slice();
        let mut best: Option<(f32, f32, f32, &'static str)> = None;

        for cue in &self.cues {
            let p = Self::calibrate_cue(&cue.curve, values[cue.margin_index]);
            let z = values[cue.z_index];
            let margin = values[cue.margin_index];
            let take = match best {
                None => true,
                Some((bp, bz, _, _)) => p > bp || (p == bp && z > bz),
            };
            if take {
                best = Some((p, z, margin, cue.cue));
            }
        }

        let (p, z, margin, cue) = best.expect("a validated gate has at least one cue");
        Verdict {
            score: z,
            margin,
            calibrated_precision: p,
            winning_cue: cue,
            passes: p >= self.threshold,
        }
    }

    /// The highest precision this gate predicts anywhere — the max over the cues' top blocks.
    ///
    /// Reported when the gate abstains everywhere, which has been the outcome since Session B: it
    /// distinguishes *"nothing scored well today"* from *"this cue set cannot reach the operating
    /// point at all"*, and only the second is a statement about the design.
    ///
    /// **This is Session E's headline.** It is a per-cue quantity — the combination function does
    /// not enter it — which is what makes it a clean read on how precise the best cue's most
    /// confident region is once the calibration is asked a query-local question.
    pub fn max_calibrated_precision(&self) -> f32 {
        self.cues
            .iter()
            .filter_map(|c| c.curve.last().map(|bp| bp[1]))
            .fold(0.0f32, f32::max)
    }

    /// Per-cue top-block precision, for the calibration generalization pair.
    ///
    /// The standing check compares each cue's fit-split prediction against the held-out
    /// measurement, and it only exists if someone looks: a memorized calibration is invisible in
    /// precision, coverage, ASR and latency alike.
    pub fn max_calibrated_precision_per_cue(&self) -> Vec<(String, f32)> {
        self.cues
            .iter()
            .map(|c| (c.name.clone(), c.curve.last().map_or(0.0, |bp| bp[1])))
            .collect()
    }
}

/// Curve invariants, checked per cue so the error names which one.
fn validate_curve(cue: &str, curve: &[[f32; 2]]) -> Result<(), GateError> {
    if curve.is_empty() {
        return Err(GateError::CurveEmpty {
            cue: cue.to_string(),
        });
    }
    for (index, bp) in curve.iter().enumerate() {
        if !(0.0..=1.0).contains(&bp[1]) {
            return Err(GateError::CurvePrecisionOutOfRange {
                cue: cue.to_string(),
                index,
                value: bp[1],
            });
        }
    }
    for i in 1..curve.len() {
        if curve[i][0] < curve[i - 1][0] {
            return Err(GateError::CurveNotSorted {
                cue: cue.to_string(),
                index: i,
                previous: curve[i - 1][0],
                current: curve[i][0],
            });
        }
        // Strictly increasing, not merely non-decreasing. Two blocks at the same score make
        // `partition_point` ambiguous, and margin has a large atom at 0 -- every query whose
        // candidates tie at the top contributes one.
        if curve[i][0] == curve[i - 1][0] {
            return Err(GateError::CurveDuplicateBreakpoint {
                cue: cue.to_string(),
                index: i,
                value: curve[i][0],
            });
        }
        if curve[i][1] < curve[i - 1][1] {
            return Err(GateError::CurveNotMonotone {
                cue: cue.to_string(),
                index: i,
                previous: curve[i - 1][1],
                current: curve[i][1],
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FEATURES_JSON: &str = r#"["lexical_bm25","dense_cosine","lexical_margin","dense_margin","lexical_z","dense_z","lexical_rank_recip","dense_rank_recip","effective_trust","fidelity","cue_agreement_2cue"]"#;

    fn fitted_json() -> String {
        format!(
            r#"{{
  "state": "fitted",
  "note": "test fixture",
  "version": "{GATE_VERSION}",
  "fusion": "per-query-margin-calibration-continuous-z-ranking",
  "threshold": 0.95,
  "feature_names": {FEATURES_JSON},
  "cue_features": ["lexical_margin", "dense_margin"],
  "rank_features": ["lexical_z", "dense_z"],
  "cue_curves": {{
    "lexical_margin": [[0.2, 0.1], [0.5, 0.4], [0.8, 0.97]],
    "dense_margin": [[0.3, 0.05], [0.6, 0.2], [0.9, 0.5]]
  }},
  "inert_features": {{
    "lexical_bm25": "retained as Number 3's cross-session anchor; no curve reads it",
    "dense_cosine": "retained as Number 3's cross-session anchor; no curve reads it",
    "lexical_rank_recip": "diagnostic only",
    "dense_rank_recip": "diagnostic only",
    "effective_trust": "zero variance across the fit split",
    "fidelity": "zero variance across the fit split",
    "cue_agreement_2cue": "declared: needs a firing predicate for the dense cue"
  }},
  "floor_verdict": "pass",
  "floor_required": 0.5478,
  "floor_measured": 0.6,
  "floor_read_from": "test fixture",
  "corpus": "longmemeval-s",
  "corpus_variant": "cleaned",
  "corpus_sha256": "abc",
  "split_rule": "test",
  "split_digest": "def",
  "fit_cases": 250,
  "heldout_cases": 250,
  "fit_rows": 1000,
  "fit_positives": 10,
  "fitted_at_clock_ms": 1780000000000
}}"#
        )
    }

    /// A feature vector by name, so a test never depends on positional order.
    fn vector(pairs: &[(&str, f32)]) -> FeatureVector {
        let mut v = [0.0f32; FEATURE_COUNT];
        for (name, value) in pairs {
            v[features::feature_index(name).expect("known feature")] = *value;
        }
        FeatureVector(v)
    }

    #[test]
    fn the_committed_artifact_is_whatever_it_says_it_is() {
        // Not an assertion about fitted-ness: this test passes before and after the fit, and
        // what it proves is that the embedded file parses and that `load` agrees with its own
        // declared state.
        let artifact: GateArtifact = serde_json::from_str(ARTIFACT_JSON).expect("artifact parses");
        match artifact.state.as_str() {
            "fitted" => {
                let gate = FrozenGate::load().expect("a fitted artifact must load");
                assert_eq!(gate.threshold(), THRESHOLD);
                assert_eq!(gate.cues().len(), CUE_COUNT);
            }
            "unfitted" => {
                assert!(matches!(FrozenGate::load(), Err(GateError::Unfitted { .. })));
            }
            other => panic!("unknown artifact state {other:?}"),
        }
    }

    #[test]
    fn an_unfitted_artifact_is_refused_rather_than_defaulted() {
        let json = r#"{"state": "unfitted", "note": "placeholder"}"#;
        let err = FrozenGate::from_json(json).unwrap_err();
        assert!(matches!(err, GateError::Unfitted { .. }));
        // The message has to say how to fix it: this is the error a fresh clone hits.
        assert!(err.to_string().contains("tools/fit_gate.py"), "{err}");
    }

    // ------------------------------------------------------------------ the v3/v4 boundary

    #[test]
    fn a_v3_artifact_is_refused_by_its_own_missing_role() {
        // v3 is a *fitted* artifact with plausible provenance. Its version differs, and it also
        // carries no `rank_features` -- two independent refusals, because the ordering role is
        // the thing v3 had no concept of.
        let v3 = r#"{
  "state": "fitted", "note": "v3", "version": "frozen-v3",
  "fusion": "max-per-cue-calibrated-precision", "threshold": 0.95,
  "feature_names": ["lexical_bm25","dense_cosine","effective_trust","fidelity","cue_agreement_2cue"],
  "cue_features": ["lexical_bm25","dense_cosine"],
  "cue_curves": {"lexical_bm25": [[1.0, 0.309]], "dense_cosine": [[1.0, 0.2876]]},
  "inert_features": {"effective_trust": "x", "fidelity": "y", "cue_agreement_2cue": "z"},
  "floor_verdict": "fail", "floor_required": 0.5478, "floor_measured": 0.4783,
  "floor_read_from": "runs/session-d/cue-overlap.json",
  "corpus": "longmemeval-s", "corpus_variant": "cleaned", "corpus_sha256": "abc",
  "split_rule": "r", "split_digest": "d", "fit_cases": 251, "heldout_cases": 249,
  "fit_rows": 119340, "fit_positives": 452, "fitted_at_clock_ms": 0
}"#;
        let err = FrozenGate::from_json(v3).unwrap_err();
        assert!(matches!(err, GateError::VersionDisagrees { .. }), "{err}");
    }

    #[test]
    fn a_v3_fusion_declared_on_a_v4_schema_is_refused_by_name() {
        let json = fitted_json().replace(
            r#""fusion": "per-query-margin-calibration-continuous-z-ranking""#,
            r#""fusion": "max-per-cue-calibrated-precision""#,
        );
        assert!(matches!(
            FrozenGate::from_json(&json),
            Err(GateError::FusionDisagrees { .. })
        ));
    }

    #[test]
    fn a_wrong_version_is_refused() {
        // Built from the constant, not from a literal. A hardcoded version here silently stops
        // testing anything the moment `GATE_VERSION` is bumped: the `replace` finds nothing, the
        // fixture stays valid, and the test asserts a refusal that never fires.
        let json = fitted_json().replace(
            &format!(r#""version": "{GATE_VERSION}""#),
            r#""version": "frozen-v0-not-this-build""#,
        );
        assert!(matches!(
            FrozenGate::from_json(&json),
            Err(GateError::VersionDisagrees { .. })
        ));
    }

    // ------------------------------------------------------------------ role identity

    #[test]
    fn reordered_feature_names_are_refused() {
        let json = fitted_json().replace(
            r#""lexical_bm25","dense_cosine","lexical_margin""#,
            r#""dense_cosine","lexical_bm25","lexical_margin""#,
        );
        assert!(matches!(
            FrozenGate::from_json(&json),
            Err(GateError::FeatureNamesDisagree { .. })
        ));
    }

    #[test]
    fn dropping_a_cue_is_refused_rather_than_silently_calibrating_one() {
        let json = fitted_json().replace(
            r#""cue_features": ["lexical_margin", "dense_margin"]"#,
            r#""cue_features": ["lexical_margin"]"#,
        );
        assert!(matches!(
            FrozenGate::from_json(&json),
            Err(GateError::CueFeaturesDisagree { .. })
        ));
    }

    #[test]
    fn a_disagreeing_ranking_key_is_refused() {
        // **The v4-specific refusal.** The ranking key is pre-registered before the fit so it
        // cannot be chosen with top-1 in view. An artifact ordering by something else is a
        // different experiment, and every downstream number would still be produced.
        let json = fitted_json().replace(
            r#""rank_features": ["lexical_z", "dense_z"]"#,
            r#""rank_features": ["dense_z", "lexical_z"]"#,
        );
        assert!(matches!(
            FrozenGate::from_json(&json),
            Err(GateError::RankFeaturesDisagree { .. })
        ));
    }

    #[test]
    fn a_missing_rank_features_declaration_is_refused() {
        let json = fitted_json().replace(
            r#"  "rank_features": ["lexical_z", "dense_z"],
"#,
            "",
        );
        let err = FrozenGate::from_json(&json).unwrap_err();
        assert!(
            matches!(err, GateError::MissingField { field: "rank_features" }),
            "{err}"
        );
    }

    #[test]
    fn a_cue_without_a_curve_is_refused() {
        let json = fitted_json().replace(r#""dense_margin": [[0.3, 0.05]"#, r#""fidelity": [[0.3, 0.05]"#);
        assert!(matches!(
            FrozenGate::from_json(&json),
            Err(GateError::CueCurveMissing { .. })
        ));
    }

    #[test]
    fn a_curve_that_is_never_read_is_refused() {
        let json = fitted_json().replace(
            r#""dense_margin": [[0.3, 0.05], [0.6, 0.2], [0.9, 0.5]]"#,
            r#""dense_margin": [[0.3, 0.05], [0.6, 0.2], [0.9, 0.5]],
    "lexical_z": [[0.5, 0.1]]"#,
        );
        assert!(matches!(
            FrozenGate::from_json(&json),
            Err(GateError::CueCurveExtra { .. })
        ));
    }

    // ------------------------------------------------------------------ inert coverage

    #[test]
    fn a_feature_in_no_role_with_no_stated_reason_is_refused() {
        let json = fitted_json().replace(
            r#"    "fidelity": "zero variance across the fit split",
"#,
            "",
        );
        let err = FrozenGate::from_json(&json).unwrap_err();
        assert!(
            matches!(err, GateError::NonCueFeatureNotInert { ref feature } if feature == "fidelity"),
            "{err}"
        );
    }

    #[test]
    fn declaring_a_cue_inert_is_refused() {
        let json = fitted_json().replace(
            r#""effective_trust": "zero variance across the fit split","#,
            r#""lexical_margin": "oops","#,
        );
        assert!(matches!(
            FrozenGate::from_json(&json),
            Err(GateError::InertFeatureIsACue { .. })
        ));
    }

    #[test]
    fn declaring_a_RANK_feature_inert_is_refused() {
        // The role v4 adds. Calling a feature that decides the ordering "inert" is a false claim,
        // and it is exactly the claim a later reader would rely on when deciding what is safe to
        // remove.
        let json = fitted_json().replace(
            r#""effective_trust": "zero variance across the fit split","#,
            r#""lexical_z": "takes no part","#,
        );
        let err = FrozenGate::from_json(&json).unwrap_err();
        assert!(
            matches!(err, GateError::InertFeatureIsARankFeature { ref feature } if feature == "lexical_z"),
            "{err}"
        );
    }

    #[test]
    fn declaring_an_unknown_feature_inert_is_refused() {
        let json = fitted_json().replace(
            r#""effective_trust": "zero variance across the fit split","#,
            r#""recency": "not a feature here","#,
        );
        assert!(matches!(
            FrozenGate::from_json(&json),
            Err(GateError::InertFeatureUnknown { .. })
        ));
    }

    // ------------------------------------------------------------------ curve invariants

    #[test]
    fn a_non_monotone_curve_is_refused_and_names_its_cue() {
        let json = fitted_json().replace("[0.5, 0.4]", "[0.5, 0.05]");
        let err = FrozenGate::from_json(&json).unwrap_err();
        assert!(
            matches!(err, GateError::CurveNotMonotone { ref cue, .. } if cue == "lexical_margin"),
            "{err}"
        );
    }

    #[test]
    fn an_unsorted_curve_is_refused() {
        let json = fitted_json().replace("[[0.2, 0.1], [0.5, 0.4]", "[[0.6, 0.1], [0.5, 0.4]");
        assert!(matches!(
            FrozenGate::from_json(&json),
            Err(GateError::CurveNotSorted { .. })
        ));
    }

    #[test]
    fn a_duplicate_breakpoint_is_refused() {
        // The hazard carried forward from Session D and re-argued for the new feature: margin has
        // a large atom at exactly 0 -- every query whose top two candidates tie contributes one --
        // so quantile bucketing produces blocks sharing a bound unless the fitter pools them.
        // `partition_point` cannot say which block owns that score, and a `<`-based sortedness
        // check would pass it.
        let json = fitted_json().replace("[[0.2, 0.1], [0.5, 0.4]", "[[0.2, 0.1], [0.2, 0.4]");
        let err = FrozenGate::from_json(&json).unwrap_err();
        assert!(
            matches!(err, GateError::CurveDuplicateBreakpoint { ref cue, .. } if cue == "lexical_margin"),
            "{err}"
        );
    }

    #[test]
    fn a_precision_above_one_is_refused() {
        let json = fitted_json().replace("[0.8, 0.97]", "[0.8, 1.4]");
        assert!(matches!(
            FrozenGate::from_json(&json),
            Err(GateError::CurvePrecisionOutOfRange { .. })
        ));
    }

    #[test]
    fn an_empty_curve_is_refused() {
        let json = fitted_json().replace("[[0.3, 0.05], [0.6, 0.2], [0.9, 0.5]]", "[]");
        assert!(matches!(
            FrozenGate::from_json(&json),
            Err(GateError::CurveEmpty { .. })
        ));
    }

    #[test]
    fn a_moved_threshold_is_refused() {
        // HP1's freeze, enforced. Lowering the operating point to make a run inject something is
        // the exact tuning the freeze exists to forbid.
        let json = fitted_json().replace("\"threshold\": 0.95", "\"threshold\": 0.6");
        assert!(matches!(
            FrozenGate::from_json(&json),
            Err(GateError::ThresholdDisagrees { .. })
        ));
    }

    #[test]
    fn a_missing_field_names_itself() {
        let json = fitted_json().replace("\"corpus\": \"longmemeval-s\",", "");
        let err = FrozenGate::from_json(&json).unwrap_err();
        assert!(matches!(err, GateError::MissingField { field: "corpus" }), "{err}");
    }

    // ------------------------------------------------------------------ the verdict

    #[test]
    fn the_calibration_reads_MARGIN_and_nothing_else() {
        // The whole point of the session, as a test: the raw score must not reach the curve.
        let gate = FrozenGate::from_json(&fitted_json()).unwrap();
        let a = gate.judge(&vector(&[("lexical_margin", 0.8), ("lexical_bm25", 0.0)]));
        let b = gate.judge(&vector(&[("lexical_margin", 0.8), ("lexical_bm25", 1.0)]));
        assert_eq!(a.calibrated_precision, b.calibrated_precision);
        assert_eq!(a.calibrated_precision, 0.97);
    }

    #[test]
    fn the_ranking_score_is_the_winning_cues_Z_not_its_calibrated_value() {
        // ADR-010, enforced. The value that orders the ranking has to be continuous, and z is the
        // only continuous cross-cue-comparable quantity in the vector.
        let gate = FrozenGate::from_json(&fitted_json()).unwrap();
        let v = gate.judge(&vector(&[
            ("lexical_margin", 0.8),
            ("lexical_z", 3.25),
            ("dense_margin", 0.3),
            ("dense_z", 0.5),
        ]));
        assert_eq!(v.winning_cue, "lexical");
        assert_eq!(v.score, 3.25, "the winning cue's z");
        assert_eq!(v.margin, 0.8, "the winning cue's margin");
        assert_eq!(v.calibrated_precision, 0.97);
    }

    #[test]
    fn two_candidates_in_the_same_calibration_block_are_still_ordered() {
        // **The Session D defect, as a regression test.** Under v3 every candidate above a
        // breakpoint collapsed to one calibrated value and the tiebreak decided top-1; 60.4% of
        // cases had a tie at the fused maximum. Here the two candidates share a block AND a
        // calibrated precision, and the ranking still separates them -- because it never reads
        // the calibrated value.
        let gate = FrozenGate::from_json(&fitted_json()).unwrap();
        let a = gate.judge(&vector(&[("lexical_margin", 0.81), ("lexical_z", 4.0)]));
        let b = gate.judge(&vector(&[("lexical_margin", 0.95), ("lexical_z", 2.0)]));
        assert_eq!(a.calibrated_precision, b.calibrated_precision, "same block");
        assert!(a.score > b.score, "and still strictly ordered by a continuous key");
    }

    #[test]
    fn the_weaker_cue_cannot_drag_the_verdict_down() {
        let gate = FrozenGate::from_json(&fitted_json()).unwrap();
        let alone = gate.judge(&vector(&[("lexical_margin", 0.8)]));
        let with_weak_dense = gate.judge(&vector(&[("lexical_margin", 0.8), ("dense_margin", 0.3)]));
        assert_eq!(alone.calibrated_precision, with_weak_dense.calibrated_precision);
    }

    #[test]
    fn either_cue_alone_can_carry_a_candidate() {
        let gate = FrozenGate::from_json(&fitted_json()).unwrap();
        let lexical_only = gate.judge(&vector(&[("lexical_margin", 0.8)]));
        assert_eq!(lexical_only.calibrated_precision, 0.97);
        assert_eq!(lexical_only.winning_cue, "lexical");
        // dense's curve tops out at 0.5 in this fixture, so it carries but does not pass
        let dense_only = gate.judge(&vector(&[("dense_margin", 0.95), ("lexical_margin", -1.0)]));
        assert_eq!(dense_only.calibrated_precision, 0.5);
        assert_eq!(dense_only.winning_cue, "dense");
        assert!(!dense_only.passes);
    }

    #[test]
    fn the_threshold_is_read_in_calibrated_units_not_score_units() {
        // A candidate with an enormous z on a cue whose calibration never reaches 0.95 must not
        // pass. This is what keeps the operating point portable.
        let gate = FrozenGate::from_json(&fitted_json()).unwrap();
        let v = gate.judge(&vector(&[
            ("dense_margin", 1.0),
            ("dense_z", 12.0),
            ("lexical_margin", -1.0),
        ]));
        assert_eq!(v.score, 12.0);
        assert!(!v.passes, "a huge z on a cue topping out at 0.5 must not pass");
    }

    #[test]
    fn an_inert_feature_cannot_move_the_verdict() {
        let gate = FrozenGate::from_json(&fitted_json()).unwrap();
        let with = gate.judge(&vector(&[
            ("lexical_margin", 0.5),
            ("effective_trust", 1.0),
            ("fidelity", 1.0),
            ("cue_agreement_2cue", 1.0),
            ("lexical_rank_recip", 1.0),
        ]));
        let without = gate.judge(&vector(&[("lexical_margin", 0.5)]));
        assert_eq!(with, without);
    }

    #[test]
    fn scoring_is_deterministic() {
        let gate = FrozenGate::from_json(&fitted_json()).unwrap();
        let f = vector(&[("lexical_margin", 0.37), ("dense_margin", 0.62), ("lexical_z", 1.1)]);
        assert_eq!(gate.judge(&f), gate.judge(&f));
    }

    #[test]
    fn max_calibrated_precision_is_the_best_cues_top_block() {
        let gate = FrozenGate::from_json(&fitted_json()).unwrap();
        assert_eq!(gate.max_calibrated_precision(), 0.97);
        let per_cue = gate.max_calibrated_precision_per_cue();
        assert_eq!(per_cue.len(), 2);
        assert_eq!(per_cue[0], ("lexical_margin".to_string(), 0.97));
        assert_eq!(per_cue[1], ("dense_margin".to_string(), 0.5));
    }

    // ------------------------------------------------------------------ the floor interlock

    #[test]
    fn a_failed_floor_loads_while_it_cannot_inject() {
        // 0.45, not something lower: the block below it predicts 0.4, and a curve that decreases
        // is refused by `CurveNotMonotone` before the floor interlock is ever reached.
        let json = fitted_json()
            .replace(r#""floor_verdict": "pass""#, r#""floor_verdict": "fail""#)
            .replace("[0.8, 0.97]", "[0.8, 0.45]");
        let gate = FrozenGate::from_json(&json).expect("a failed floor that cannot inject loads");
        assert!(gate.max_calibrated_precision() < THRESHOLD);
        assert_eq!(gate.provenance().floor_verdict, "fail");
    }

    #[test]
    fn a_failed_floor_that_would_inject_is_refused() {
        // **The interlock.** Two facts that must not coexist in a loadable artifact: this shape
        // ranks worse than its own best single cue, and this shape would inject.
        let json = fitted_json().replace(r#""floor_verdict": "pass""#, r#""floor_verdict": "fail""#);
        let err = FrozenGate::from_json(&json).unwrap_err();
        assert!(matches!(err, GateError::FailedFloorWouldInject { .. }), "{err}");
        assert!(err.to_string().contains("--record-verdict"), "{err}");
    }

    #[test]
    fn a_passing_floor_that_would_inject_loads() {
        // The interlock must not be a blanket ban on injecting gates -- that would make it fire
        // on the very shape it exists to let through.
        let gate = FrozenGate::from_json(&fitted_json()).unwrap();
        assert!(gate.max_calibrated_precision() >= THRESHOLD);
        assert_eq!(gate.provenance().floor_verdict, "pass");
    }

    #[test]
    fn an_unmeasured_floor_loads_because_refusing_it_would_deadlock() {
        let json =
            fitted_json().replace(r#""floor_verdict": "pass""#, r#""floor_verdict": "unmeasured""#);
        let gate = FrozenGate::from_json(&json).expect("unmeasured must load");
        assert_eq!(gate.provenance().floor_verdict, "unmeasured");
    }

    #[test]
    fn an_unrecognised_floor_verdict_is_refused() {
        let json = fitted_json()
            .replace(r#""floor_verdict": "pass""#, r#""floor_verdict": "probably fine""#);
        assert!(matches!(
            FrozenGate::from_json(&json),
            Err(GateError::FloorVerdictUnrecognised { .. })
        ));
    }

    #[test]
    fn the_two_stamps_are_distinguishable() {
        // A report reading `gate.version` must be able to tell a calibrated run from a
        // feature-dump run from Session A's ungated run. Three states, three strings.
        assert_ne!(GATE_VERSION, FIT_ONLY_VERSION);
        assert_ne!(GATE_VERSION, crate::retrieve::UNGATED_VERSION);
        assert_ne!(FIT_ONLY_VERSION, crate::retrieve::UNGATED_VERSION);
    }
}
