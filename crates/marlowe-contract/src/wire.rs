//! CONTRACTS.md sections 4.1–4.7 — the three interfaces.
//!
//! **The JSON is normative and these types are its binding, not the other way round.**
//! M0a is Python and M0b is Rust, so the wire format is the contract (section 4.2b, 4.0.9).
//!
//! Requests carry `deny_unknown_fields`, mirroring the harness's own `extra="forbid"`. An
//! undeclared field is either a typo of a real one — in which case ignoring it would
//! misscore a run — or an undeclared channel between harness and implementation, which the
//! M0a/M0b split exists to prevent. Both are worth failing on.

use serde::{Deserialize, Serialize};

use crate::common::{
    AbstentionReason, Channel, Clock, Fidelity, PayloadKind, Speaker, TrustClass,
};

pub const CONTRACT_VERSION: &str = "1.0";

/// The `contract_version` field on every outbound response, as a type that can only hold
/// the right answer.
///
/// It is zero-sized and serializes to the [`CONTRACT_VERSION`] constant. A `String` field
/// with a serde default would look equivalent and is not: `#[serde(default)]` applies to
/// *deserialization*, so on a serialize-only response it does nothing at all and the value
/// becomes whatever the constructing code happened to put there. That is the shape of every
/// unobservable mismatch in this project — two places that must agree, with only one of them
/// checked — so the field is made unable to disagree instead of being checked for agreement.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ContractVersion;

impl Serialize for ContractVersion {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(CONTRACT_VERSION)
    }
}

// =======================================================================================
// 4.6 — ingest
// =======================================================================================

/// Where the bytes came from. **The implementation derives trust from this.**
///
/// Note what is absent: there is no `trust_class` field, and there must never be one.
/// Section 4.6 — *"M0a declares `origin`. It never declares `trust_class`."* Letting the
/// eval set trust directly would bypass the mechanism it exists to test.
///
/// `actor` is a free string on the wire by contract. It may only ever cause a **rejection**;
/// it may never raise trust. See `marlowe_memory::trust`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Origin {
    pub channel: Channel,
    pub actor: String,
    #[serde(default)]
    pub r#ref: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Turn {
    pub turn_id: String,
    pub speaker: Speaker,
    pub text: String,
    pub occurred_at_ms: i64,
    pub origin: Origin,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IngestRequest {
    pub contract_version: String,
    pub clock: Clock,
    pub session_id: String,
    pub turns: Vec<Turn>,
}

/// `effective_trust` here is what the implementation **derived**. The laundering suite
/// compares against this value and never against anything the eval supplied.
#[derive(Debug, Clone, Serialize)]
pub struct Written {
    pub turn_id: String,
    pub memory_ids: Vec<String>,
    pub effective_trust: TrustClass,
}

/// A first-class outcome. A suite that plants an unauthorized write must be able to **see
/// it refused** rather than infer refusal from absence — which is exactly what K3 measures.
#[derive(Debug, Clone, Serialize)]
pub struct RejectedWrite {
    pub turn_id: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct IngestLatency {
    pub total: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct IngestCost {
    pub ingest_tokens: u32,
    pub latency_ms: IngestLatency,
}

#[derive(Debug, Clone, Serialize)]
pub struct IngestResponse {
    pub contract_version: ContractVersion,
    pub session_id: String,
    pub written: Vec<Written>,
    pub rejected: Vec<RejectedWrite>,
    pub cost: IngestCost,
}

// =======================================================================================
// 4.1–4.4 — retrieve
// =======================================================================================

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetrievalBudget {
    pub max_tokens: u32,
    pub max_latency_ms: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetrievalRequest {
    pub contract_version: String,
    /// Same shape on all three interfaces. An earlier draft put a bare `now_ms` at the top
    /// level of this request only; that was pinned away on 2026-08-02.
    pub clock: Clock,
    pub query_id: String,
    pub session_id: String,
    pub turn_index: u32,
    pub query_text: String,
    pub budget: RetrievalBudget,
}

/// `content` is non-optional by contract: a tombstone can never appear here (section 4.3),
/// so there is no case where the text is legitimately absent.
#[derive(Debug, Clone, Serialize)]
pub struct InjectedMemory {
    pub memory_id: String,
    pub content: String,
    pub score: f32,
    pub calibrated_precision: f32,
    pub fidelity: Fidelity,
    pub effective_trust: TrustClass,
    pub payload_kind: PayloadKind,
}

#[derive(Debug, Clone, Serialize)]
pub struct GateStamp {
    pub version: String,
    pub threshold: f32,
    /// **False for M0.** Only M10 may set this true, and only after beating the frozen
    /// baseline on the M0a suite at equal or lower cost.
    pub adaptive: bool,
}

/// Section 4.2 shows total/embed/cues/fuse/gate.
///
/// Only `total` is required, and the stage fields are **omitted rather than zeroed** when a
/// stage does not exist. The harness says why: *"an implementation without an embed stage
/// should report its absence rather than report a zero that reads as a measurement."*
#[derive(Debug, Clone, Default, Serialize)]
pub struct RetrievalLatency {
    pub total: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embed: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cues: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fuse: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gate: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RetrievalCost {
    pub retrieval_tokens: u32,
    pub latency_ms: RetrievalLatency,
}

/// Section 4.2. **`cost` is not an Option.** A result without cost is unrepresentable, which
/// is how section 5.7's "report the pair" stops being a convention someone has to remember.
#[derive(Debug, Clone, Serialize)]
pub struct RetrievalResponse {
    pub contract_version: ContractVersion,
    pub query_id: String,
    pub abstained: bool,
    pub abstention_reason: Option<AbstentionReason>,
    pub injected: Vec<InjectedMemory>,
    pub considered: u32,
    pub gate: GateStamp,
    pub cost: RetrievalCost,
}

// =======================================================================================
// 4.7 — answer
// =======================================================================================

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnswerRequest {
    pub contract_version: String,
    pub clock: Clock,
    pub query_id: String,
    pub session_id: String,
    pub question: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct AnswerLatency {
    pub total: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retrieval: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation: Option<i64>,
}

/// Section 4.7 separates retrieval from generation tokens on purpose: section 5.7's ≤7,000
/// budget is a **retrieval** budget, and folding generation into it would hide a miss.
#[derive(Debug, Clone, Serialize)]
pub struct AnswerCost {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub retrieval_tokens: u32,
    pub latency_ms: AnswerLatency,
}

/// `retrieval` is **embedded, not referenced**. Injection precision and answer correctness
/// must be joinable on a single record or the headline metric cannot be attributed to a
/// retrieval decision — which is the entire reason both are measured.
#[derive(Debug, Clone, Serialize)]
pub struct AnswerResponse {
    pub contract_version: ContractVersion,
    pub query_id: String,
    pub answered: bool,
    pub answer: Option<String>,
    pub abstained: bool,
    pub abstention_reason: Option<AbstentionReason>,
    pub grounded_in: Vec<String>,
    pub retrieval: RetrievalResponse,
    pub cost: AnswerCost,
}

// =======================================================================================
// outbound self-check
// =======================================================================================

/// Section 4.2 and 4.7 exclusivity, checked on the way **out**.
///
/// The harness enforces these too, and that is the point of doing it here as well: a
/// violation caught at the boundary of our own process is a bug report with a stack trace,
/// while the same violation caught by the harness is a protocol error that aborts a run and
/// says only what the bytes looked like. Same rule, two sides, and neither trusts the other.
#[derive(Debug, thiserror::Error)]
pub enum ExclusivityError {
    #[error("abstained with {0} injected memories; an implementation with something to inject has not abstained")]
    AbstainedWithInjected(usize),
    #[error("abstention_reason set while abstained is false")]
    ReasonWithoutAbstention,
    #[error("injected nothing without abstaining; injecting nothing IS the abstention outcome")]
    EmptyInjectionWithoutAbstention,
    #[error("answered is false but answer carries {0} characters")]
    HedgedAbstention(usize),
    #[error("abstained while citing {0} grounding memories")]
    GroundedWhileAbstained(usize),
    #[error("answered is false without abstained being true; the scorer has no bin for that")]
    UnansweredWithoutAbstention,
    #[error("memory {0} would be injected at fidelity tombstone")]
    TombstoneInjected(String),
}

impl RetrievalResponse {
    pub fn check_exclusivity(&self) -> Result<(), ExclusivityError> {
        for item in &self.injected {
            if item.fidelity == Fidelity::Tombstone {
                return Err(ExclusivityError::TombstoneInjected(item.memory_id.clone()));
            }
        }
        if self.abstained && !self.injected.is_empty() {
            return Err(ExclusivityError::AbstainedWithInjected(self.injected.len()));
        }
        if !self.abstained && self.abstention_reason.is_some() {
            return Err(ExclusivityError::ReasonWithoutAbstention);
        }
        if !self.abstained && self.injected.is_empty() {
            return Err(ExclusivityError::EmptyInjectionWithoutAbstention);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contract_version_cannot_hold_a_wrong_value() {
        // The whole point of the zero-sized type: there is no constructor that produces a
        // different string, so a response cannot disagree with the constant.
        assert_eq!(
            serde_json::to_string(&ContractVersion).unwrap(),
            format!("\"{CONTRACT_VERSION}\"")
        );
    }

    #[test]
    fn absent_latency_stages_are_omitted_not_zeroed() {
        // "An implementation without an embed stage should report its absence rather than
        // report a zero that reads as a measurement."
        let cost = RetrievalCost {
            retrieval_tokens: 0,
            latency_ms: RetrievalLatency {
                total: 12,
                ..Default::default()
            },
        };
        let json = serde_json::to_string(&cost).unwrap();
        assert!(json.contains("\"total\":12"));
        assert!(!json.contains("embed"), "an absent stage must not appear at all");
    }

    fn abstaining_retrieval(query_id: &str) -> RetrievalResponse {
        RetrievalResponse {
            contract_version: ContractVersion,
            query_id: query_id.to_string(),
            abstained: true,
            abstention_reason: Some(AbstentionReason::NoCandidates),
            injected: Vec::new(),
            considered: 0,
            gate: GateStamp {
                version: "test".into(),
                threshold: 0.0,
                adaptive: false,
            },
            cost: RetrievalCost {
                retrieval_tokens: 0,
                latency_ms: RetrievalLatency::default(),
            },
        }
    }

    #[test]
    fn empty_injection_without_abstention_is_rejected_on_the_way_out() {
        let mut r = abstaining_retrieval("q");
        r.abstained = false;
        r.abstention_reason = None;
        assert!(matches!(
            r.check_exclusivity(),
            Err(ExclusivityError::EmptyInjectionWithoutAbstention)
        ));
    }

    #[test]
    fn hedged_abstention_is_rejected_on_the_way_out() {
        let a = AnswerResponse {
            contract_version: ContractVersion,
            query_id: "q".into(),
            answered: false,
            answer: Some("well, possibly SQLite".into()),
            abstained: true,
            abstention_reason: Some(AbstentionReason::NoCandidates),
            grounded_in: Vec::new(),
            retrieval: abstaining_retrieval("q"),
            cost: AnswerCost {
                prompt_tokens: 0,
                completion_tokens: 0,
                retrieval_tokens: 0,
                latency_ms: AnswerLatency::default(),
            },
        };
        assert!(matches!(
            a.check_exclusivity(),
            Err(ExclusivityError::HedgedAbstention(_))
        ));
    }

    #[test]
    fn an_honest_abstention_passes() {
        let a = AnswerResponse {
            contract_version: ContractVersion,
            query_id: "q".into(),
            answered: false,
            answer: None,
            abstained: true,
            abstention_reason: Some(AbstentionReason::NoCandidates),
            grounded_in: Vec::new(),
            retrieval: abstaining_retrieval("q"),
            cost: AnswerCost {
                prompt_tokens: 0,
                completion_tokens: 0,
                retrieval_tokens: 0,
                latency_ms: AnswerLatency::default(),
            },
        };
        assert!(a.check_exclusivity().is_ok());
    }
}

impl AnswerResponse {
    pub fn check_exclusivity(&self) -> Result<(), ExclusivityError> {
        if !self.answered {
            if let Some(text) = self.answer.as_deref() {
                if !text.trim().is_empty() {
                    return Err(ExclusivityError::HedgedAbstention(text.trim().len()));
                }
            }
        }
        if !self.abstained && self.abstention_reason.is_some() {
            return Err(ExclusivityError::ReasonWithoutAbstention);
        }
        if self.abstained && !self.grounded_in.is_empty() {
            return Err(ExclusivityError::GroundedWhileAbstained(self.grounded_in.len()));
        }
        if !self.answered && !self.abstained {
            return Err(ExclusivityError::UnansweredWithoutAbstention);
        }
        self.retrieval.check_exclusivity()
    }
}
