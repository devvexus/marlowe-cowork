//! K1 condition 3's abstention path — the declared operating point, and what it may say.
//!
//! `docs/design/PRECISION-COVERAGE.md` declares one: **coverage 10.0%, rank-1/rank-2 cross-encoder
//! margin ≥ 1.165071.** Below it, the system abstains. That number, and nothing else on that page,
//! is what this type carries into the running system.
//!
//! # This type deliberately cannot tell you the precision
//!
//! There is no `precision` field, no `ci95` field, and no accessor for either. That is a structural
//! statement of a rule rather than a comment asking for one:
//!
//! **The cut point transfers. The precision does not.** `tools/publish_precision_coverage.py`
//! computes 0.9130 on a population that excluded every query whose candidate pool held no gold turn
//! (`per_query`'s `if c["gold"].sum() == 0: continue`) and every LongMemEval abstention case. In a
//! live conversation, *"no relevant memory exists"* is the common case — and it is exactly the case
//! the denominator dropped. So 0.9130 is conditional on a correct memory existing, measured on
//! LongMemEval sessions, one query each, session-scoped.
//!
//! The threshold is a number in cross-encoder logit units, and the graph producing those logits is
//! digest-pinned ([`crate::rerank::MODEL_SHA256`]), so it means the same thing at runtime that it
//! meant offline. The precision attached to it is a property of a corpus this system is not
//! running on. A field that does not exist cannot be printed in `--dev`, logged, or quoted in a
//! status line by a later session that did not read this docstring.
//!
//! # A second reason abstention is not merely a precision knob
//!
//! The threshold is on the **rerank margin** — a score an attacker who controls memory text can
//! optimise directly, since high margin means "lexically and semantically dominates its rivals".
//! Conditioning on it filters *natural* distractors and does not filter, and may actively select
//! for, crafted ones. **This is a relevance filter. It is not an authenticity filter, and it
//! carries no adversarial guarantee.** See `docs/requirements/proposed-research-memory.md`.

use serde::Deserialize;

/// `include_str!`, for the reason `FrozenGate` gives: a released binary can never be separated from
/// the measurement it claims to operate at.
const ARTIFACT_JSON: &str =
    include_str!("../artifacts/precision-coverage-heldout-v1.json");

const ARTIFACT_PATH: &str = "crates/marlowe-memory/artifacts/precision-coverage-heldout-v1.json";

/// The model directory the published curve was measured on.
///
/// The **binding**, and it costs nothing to make impossible now. `CrossEncoder::load` already
/// refuses any graph whose digest is not [`crate::rerank::MODEL_SHA256`], so a loaded encoder is
/// necessarily the Session J fine-tune. What that check cannot see is whether *this artifact* was
/// measured on *that* graph. A threshold measured on one graph and applied silently to another is
/// the measurement-transfer family — correct number, sound reasoning, different system — and this
/// project has logged four instances of it.
const MEASURED_ON: &str = "ms-marco-MiniLM-L-2-v2-ft-session-j";

#[derive(Debug, thiserror::Error)]
pub enum OperatingPointError {
    #[error(
        "{ARTIFACT_PATH} does not parse: {0}. There is no default operating point; regenerate it \
         with `python tools/publish_precision_coverage.py`"
    )]
    Unparseable(#[from] serde_json::Error),

    #[error(
        "{ARTIFACT_PATH} was measured on {found:?}, but this build's cross-encoder is pinned to \
         {MEASURED_ON}. The margin threshold is in that graph's logit units and means nothing in \
         another's. Re-publish the curve against the shipped graph rather than reusing this one"
    )]
    WrongGraph { found: String },

    #[error(
        "{ARTIFACT_PATH} declares margin_threshold {found}, which is not a usable cut point. A \
         non-finite threshold would admit or reject everything with nothing observing which"
    )]
    UnusableThreshold { found: f64 },

    #[error("{ARTIFACT_PATH} is version {found}, this build reads version 1")]
    WrongVersion { found: u32 },
}

#[derive(Debug, Deserialize)]
struct Artifact {
    version: u32,
    configuration: String,
    split: String,
    declared_operating_point: Declared,
}

#[derive(Debug, Deserialize)]
struct Declared {
    margin_threshold: f64,
    actual_coverage: f64,
}

/// The declared cut point, and the provenance needed to say where it came from.
///
/// **No precision. See the module docs.**
#[derive(Debug, Clone, PartialEq)]
pub struct OperatingPoint {
    /// Rank-1 minus rank-2 cross-encoder logit. At or above this, inject rank 1; below it, abstain.
    margin_threshold: f32,
    /// What fraction of queries the offline measurement injected on. Diagnostic only — it describes
    /// the LongMemEval split, not this conversation.
    offline_coverage: f64,
    split: String,
    configuration: String,
}

impl OperatingPoint {
    pub fn load() -> Result<Self, OperatingPointError> {
        Self::from_json(ARTIFACT_JSON)
    }

    pub fn from_json(json: &str) -> Result<Self, OperatingPointError> {
        let a: Artifact = serde_json::from_str(json)?;
        if a.version != 1 {
            return Err(OperatingPointError::WrongVersion { found: a.version });
        }
        // Substring rather than equality: `configuration` is prose that also carries the precision
        // and depth. What must hold is that it names the graph this build pins.
        if !a.configuration.contains(MEASURED_ON) {
            return Err(OperatingPointError::WrongGraph { found: a.configuration });
        }
        let t = a.declared_operating_point.margin_threshold;
        if !t.is_finite() {
            return Err(OperatingPointError::UnusableThreshold { found: t });
        }
        Ok(Self {
            margin_threshold: t as f32,
            offline_coverage: a.declared_operating_point.actual_coverage,
            split: a.split,
            configuration: a.configuration,
        })
    }

    pub fn margin_threshold(&self) -> f32 {
        self.margin_threshold
    }

    /// What this operating point is, in one line, for `--dev`.
    ///
    /// **States the split and the coverage and never the precision**, so a diagnostic cannot become
    /// a claim about how often the running system is right.
    pub fn describe(&self) -> String {
        format!(
            "margin >= {:.6} · offline coverage {:.1}% on the {} split · {} · \
             the offline precision does NOT describe live behaviour (it excluded queries with no \
             gold in the pool)",
            self.margin_threshold,
            self.offline_coverage * 100.0,
            self.split,
            self.configuration
        )
    }
}

/// Why a query injected nothing. Recorded rather than collapsed into "no".
///
/// **Three ways to be below the bar are three different facts**, and `publish_precision_coverage.py`
/// treats two of them as *refusals* rather than as low-margin queries — it drops from the measured
/// population any query with no rank 2, or whose rank 1 or 2 carries no rerank score. Abstaining
/// there is therefore the behaviour consistent with the population the number was measured on;
/// injecting there would be injecting outside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Abstention {
    /// Nothing survived the earlier stages. Not an operating-point decision at all.
    NoCandidates,
    /// One candidate, so there is no runner-up and no margin to take.
    NoRunnerUp,
    /// Rank 1 or rank 2 was outside the rerank budget, so the margin would cross a key boundary
    /// and is not comparable to the calibration.
    MarginUndefined,
    /// The margin exists and is below the cut point. The ordinary case, and the one the declared
    /// 10% coverage refers to.
    BelowThreshold,
    /// No cross-encoder is loaded, so no margin exists for any query. `--reranking off` cannot
    /// auto-inject at all — a named consequence, not a degraded fallback.
    NoReranker,
}

impl Abstention {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NoCandidates => "no candidates survived scoping and the §4.3 exclusions",
            Self::NoRunnerUp => "only one candidate; no rank 2 to take a margin from",
            Self::MarginUndefined => "rank 1 or rank 2 was not reranked; the margin is undefined",
            Self::BelowThreshold => "the rank-1/rank-2 margin is below the declared cut point",
            Self::NoReranker => {
                "no cross-encoder is loaded, so no margin exists and auto-injection is off"
            }
        }
    }
}

/// Whether the declared cut point is in force. **A measurement control, and `Declared` ships.**
///
/// # Why this exists
///
/// After M2 Session D the poisoning suite reported ASR 0.000 across all five families — and it was
/// still vacuous. Before D2 the reason was that the gate injected nothing ever; after D2 it is that
/// the declared 10%-coverage point abstains on nearly everything. **An attack that fails because
/// nothing is injected is indistinguishable from one a guard caught**, and reporting 0.000 as a
/// security result would be the capability-report family applied to a defence.
///
/// The pair that discriminates is ASR at `Declared` versus ASR at `Full`. If both are ~0, the ASRs
/// are measuring the gate's candidate set and the operating point is doing no security work at all.
///
/// # Why this is not a bare boolean, and which direction the danger runs
///
/// CLAUDE.md's standing warning is about a default-off switch forgotten in a harness target string,
/// which measures one system under another's label. Here the shipped configuration is `Declared`, so
/// **forgetting the flag yields the truthful label** — the dangerous default would be `Full`.
/// It is still an explicit value at the command line and it is **stamped on every retrieval
/// response's gate version**, so a run always records which one produced it rather than leaving a
/// reader to infer it from a command line nobody kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Coverage {
    /// The shipped path: inject rank 1 only when the margin clears the published cut point.
    Declared,
    /// **Measurement only.** Admit rank 1 whenever a margin exists, ignoring the threshold. This is
    /// not a configuration anyone may ship: it is the un-gated arm of a controlled comparison, and
    /// K1 condition 3 fails a configuration that injects at low precision to raise coverage.
    Full,
}

impl Coverage {
    /// The suffix appended to the gate stamp, so a scored run carries its own arm.
    pub fn stamp(&self) -> &'static str {
        match self {
            Self::Declared => "",
            Self::Full => "+coverage-full",
        }
    }

    /// Parse an explicit command-line value. **No default here**; the caller decides, and
    /// `main.rs` documents that an omitted flag means `Declared`.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "declared" => Some(Self::Declared),
            "full" => Some(Self::Full),
            _ => None,
        }
    }
}

/// What the operating point decided for one query.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Admission {
    /// Whether rank 1 may be injected. **Rank 1 and nothing else** — the published point is a
    /// top-1 claim (`publish_precision_coverage.py` scores `c["gold"][i1]`), so admitting a slate
    /// would quote a precision nobody computed.
    pub admit_rank_one: bool,
    /// The rank-1/rank-2 cross-encoder margin, when it is defined.
    pub margin: Option<f32>,
    pub abstention: Option<Abstention>,
}

/// The whole decision, as a pure function of the four things that decide it.
///
/// # Why this is not inline in `select_for_injection`
///
/// It was, and that made the most consequential new logic in M2 Session D **unreachable by any
/// unit test**: exercising it needs rank-1 and rank-2 cross-encoder logits, which needs a real
/// 60 MB ONNX graph and a model directory. The alternative to extracting it was a test-only
/// `Rerank` variant carrying fake logits — a second path through the shipped pipeline, which is
/// the two-sides-silently-disagree shape this project keeps logging.
///
/// So the pipeline extracts the two scores and calls this; the tests call it directly. One
/// implementation, and it is the one that ships.
pub fn decide(
    candidates_in_order: usize,
    reranker_present: bool,
    rank_one: Option<f32>,
    rank_two: Option<f32>,
    point: &OperatingPoint,
    coverage: Coverage,
) -> Admission {
    let no = |a: Abstention| Admission { admit_rank_one: false, margin: None, abstention: Some(a) };

    if candidates_in_order == 0 {
        return no(Abstention::NoCandidates);
    }
    if !reranker_present {
        // No cross-encoder means no margin exists for ANY query, so auto-injection is
        // structurally off rather than degraded. `--reranking off` is the pruning-only ablation
        // and it cannot reach the declared operating point.
        return no(Abstention::NoReranker);
    }
    if candidates_in_order < 2 {
        // `publish_precision_coverage.py` REFUSES these rather than handling them, so they are
        // outside the population the number was measured on. Abstaining matches that; injecting
        // would not.
        return no(Abstention::NoRunnerUp);
    }
    match (rank_one, rank_two) {
        (Some(top), Some(runner)) => {
            let margin = top - runner;
            // **The ONLY behavioural difference between the two arms**, and it is deliberately one
            // comparison rather than a branch that skips stages: the candidate set, the ranking, the
            // rerank and the top-1 rule are identical, so a difference in ASR between the arms is
            // attributable to the threshold and to nothing else.
            let clears = match coverage {
                Coverage::Declared => margin >= point.margin_threshold(),
                Coverage::Full => true,
            };
            if clears {
                Admission { admit_rank_one: true, margin: Some(margin), abstention: None }
            } else {
                Admission {
                    admit_rank_one: false,
                    margin: Some(margin),
                    abstention: Some(Abstention::BelowThreshold),
                }
            }
        }
        // Rank 1 or rank 2 fell outside `RERANK_BUDGET`, so the margin would cross a key boundary
        // and is not comparable to the calibration. Refused offline; abstained here.
        _ => no(Abstention::MarginUndefined),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shipped_artifact_loads_and_declares_the_published_cut_point() {
        let op = OperatingPoint::load().expect("the shipped artifact must load");
        // `docs/design/PRECISION-COVERAGE.md` states 1.1651. If this ever disagrees, one of the two
        // was regenerated without the other.
        assert!(
            (op.margin_threshold() - 1.165_071).abs() < 1e-5,
            "{} is not the published threshold",
            op.margin_threshold()
        );
    }

    /// The binding, with a control that it is not decorative.
    #[test]
    fn an_artifact_measured_on_another_graph_is_refused_by_name() {
        let json = ARTIFACT_JSON.replace(
            "ms-marco-MiniLM-L-2-v2-ft-session-j",
            "ms-marco-MiniLM-L-2-v2-int8",
        );
        let err = OperatingPoint::from_json(&json).expect_err("a foreign graph must be refused");
        assert!(
            matches!(err, OperatingPointError::WrongGraph { .. }),
            "{err}"
        );
        // The control: the unmodified artifact must load, or the refusal above would just be
        // "this never works".
        assert!(OperatingPoint::from_json(ARTIFACT_JSON).is_ok());
    }

    // ── the decision itself ───────────────────────────────────────────────────────────

    /// **Both directions, in one test.** Either alone passes on a build that returns a constant.
    #[test]
    fn a_margin_at_or_above_the_cut_point_admits_and_below_it_abstains() {
        let p = OperatingPoint::load().unwrap();
        let t = p.margin_threshold();

        let over = decide(5, true, Some(10.0), Some(10.0 - t - 0.01), &p, Coverage::Declared);
        assert!(over.admit_rank_one, "a margin clearly above the cut point must inject");

        let under = decide(5, true, Some(10.0), Some(10.0 - t + 0.01), &p, Coverage::Declared);
        assert!(!under.admit_rank_one, "a margin below it must abstain");
        assert_eq!(under.abstention, Some(Abstention::BelowThreshold));

        // The boundary is inclusive, matching `margins >= cut` in publish_precision_coverage.py.
        let exact = decide(5, true, Some(t), Some(0.0), &p, Coverage::Declared);
        assert!(exact.admit_rank_one, "the offline selection is `>=`, so this one must be too");
    }

    /// The margin is reported whenever it exists — including when it loses.
    #[test]
    fn the_margin_is_recorded_even_when_it_falls_short() {
        let p = OperatingPoint::load().unwrap();
        let v = decide(5, true, Some(3.0), Some(2.5), &p, Coverage::Declared);
        assert_eq!(v.margin, Some(0.5));
        assert!(!v.admit_rank_one);
    }

    /// Four ways to inject nothing, and they are four different facts.
    #[test]
    fn every_abstention_reason_is_distinguishable_from_the_others() {
        let p = OperatingPoint::load().unwrap();

        assert_eq!(decide(0, true, None, None, &p, Coverage::Declared).abstention, Some(Abstention::NoCandidates));
        assert_eq!(decide(5, false, None, None, &p, Coverage::Declared).abstention, Some(Abstention::NoReranker));
        assert_eq!(decide(1, true, Some(9.0), None, &p, Coverage::Declared).abstention, Some(Abstention::NoRunnerUp));
        assert_eq!(
            decide(5, true, Some(9.0), None, &p, Coverage::Declared).abstention,
            Some(Abstention::MarginUndefined),
            "rank 2 outside the rerank budget: the margin would cross a key boundary"
        );
        assert_eq!(
            decide(5, true, None, Some(1.0), &p, Coverage::Declared).abstention,
            Some(Abstention::MarginUndefined),
            "and so is rank 1 unreranked, which `_ =>` must catch rather than only the rank-2 case"
        );
    }

    /// **`--reranking off` cannot auto-inject, whatever the margin arguments say.**
    ///
    /// Checked with logits that would sail past the cut point, so this cannot pass merely because
    /// the numbers were small.
    #[test]
    fn without_a_reranker_nothing_injects_however_good_the_scores_look() {
        let p = OperatingPoint::load().unwrap();
        let v = decide(10, false, Some(99.0), Some(-99.0), &p, Coverage::Declared);
        assert!(!v.admit_rank_one);
        assert_eq!(v.abstention, Some(Abstention::NoReranker));
        // The control: identical scores WITH a reranker do inject, so the refusal above is about
        // the reranker's absence and not about the inputs.
        assert!(decide(10, true, Some(99.0), Some(-99.0), &p, Coverage::Declared).admit_rank_one);
    }

    /// At most one memory is ever admitted. The published point is a top-1 claim.
    #[test]
    fn admission_is_rank_one_alone_and_the_type_cannot_express_a_slate() {
        let p = OperatingPoint::load().unwrap();
        let v = decide(50, true, Some(20.0), Some(0.0), &p, Coverage::Declared);
        assert!(v.admit_rank_one);
        // `Admission` carries a bool, not a count or a list: a slate is not representable, so a
        // later change cannot widen coverage by admitting rank 2 without changing this type and
        // reading this test.
        let _: bool = v.admit_rank_one;
    }

    /// **The control arm differs from the shipped arm in the threshold and in nothing else.**
    ///
    /// This is what makes an ASR comparison between the two attributable. If `Full` also changed the
    /// candidate set, the ranking or the top-1 rule, a difference in ASR would have several possible
    /// causes and the comparison would answer no question.
    #[test]
    fn full_coverage_admits_exactly_where_declared_abstains_on_the_margin_and_nowhere_else() {
        let p = OperatingPoint::load().unwrap();
        let t = p.margin_threshold();

        // Below the cut point: the one case where the two arms differ.
        let below = (5, true, Some(1.0), Some(1.0 - t + 0.5));
        assert!(!decide(below.0, below.1, below.2, below.3, &p, Coverage::Declared).admit_rank_one);
        assert!(decide(below.0, below.1, below.2, below.3, &p, Coverage::Full).admit_rank_one);

        // Every other abstention reason is structural and must be IDENTICAL in both arms —
        // otherwise `Full` is not "the same system without the threshold", it is a different one.
        for (n, rerank, r1, r2) in [
            (0usize, true, None, None),               // no candidates
            (5, false, Some(9.0), Some(1.0)),         // no reranker
            (1, true, Some(9.0), None),               // no runner-up
            (5, true, Some(9.0), None),               // margin undefined
        ] {
            let d = decide(n, rerank, r1, r2, &p, Coverage::Declared);
            let f = decide(n, rerank, r1, r2, &p, Coverage::Full);
            assert_eq!(
                d.abstention, f.abstention,
                "the arms must differ ONLY on the threshold comparison; they differ at n={n}"
            );
            assert_eq!(d.admit_rank_one, f.admit_rank_one);
        }
    }

    /// The arm travels with the run's own stamp, so a scored artifact says which produced it.
    #[test]
    fn the_measurement_arm_is_recorded_rather_than_left_to_a_command_line_nobody_kept() {
        assert_eq!(Coverage::Declared.stamp(), "", "the shipped arm does not decorate the stamp");
        assert!(!Coverage::Full.stamp().is_empty(), "the measurement arm must be visible");
        assert_eq!(Coverage::parse("declared"), Some(Coverage::Declared));
        assert_eq!(Coverage::parse("full"), Some(Coverage::Full));
        // No default, no fuzzy matching: an unrecognised value is a refusal at the command line,
        // not a silent fallback to the shipped arm.
        assert_eq!(Coverage::parse("Full"), None);
        assert_eq!(Coverage::parse(""), None);
        assert_eq!(Coverage::parse("off"), None);
    }

    /// **The rule about precision, enforced rather than documented.**
    #[test]
    fn nothing_the_operating_point_can_say_mentions_precision() {
        let op = OperatingPoint::load().unwrap();
        let d = op.describe().to_lowercase();
        assert!(d.contains("margin"), "{d}");
        // 0.9130 and 0.91 must not appear anywhere a reader could take as a runtime claim.
        assert!(!d.contains("0.913"), "the offline precision leaked into a runtime string: {d}");
        assert!(
            d.contains("does not describe live behaviour")
                || d.contains("does NOT describe live behaviour".to_lowercase().as_str()),
            "the caveat must travel with the number: {d}"
        );
    }
}
