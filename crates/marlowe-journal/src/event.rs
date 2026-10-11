//! CONTRACTS.md section 1.1 — actors and the closed set of event kinds.

use serde::{Deserialize, Serialize};

pub type Seq = u64;

/// Invariant 7's replay key.
pub type TraceId = uuid::Uuid;

/// **The model is not in this enum. That is the point: the model requests, the harness
/// appends.**
///
/// Because the model can never be an `Actor`, self-promotion is unrepresentable rather than
/// merely forbidden (section A8.4: *self-granted promotions — zero, structurally
/// impossible*). Adding a `Model` variant would quietly delete invariant 2 and invariant 3
/// at once, so there is a test asserting the variant list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "actor", rename_all = "snake_case")]
pub enum Actor {
    /// The loop's own bookkeeping.
    Harness,
    User { surface: String },
    /// The ONLY component that may emit tier-granting events.
    Permission,
    Consolidation { run: String },
    Trigger { trigger: String },
    Tool { tool: String, run: String },
}

impl Actor {
    /// A stable string for the signed form and for the `actor` column.
    pub fn canonical(&self) -> String {
        match self {
            Actor::Harness => "harness".into(),
            Actor::User { surface } => format!("user:{surface}"),
            Actor::Permission => "permission".into(),
            Actor::Consolidation { run } => format!("consolidation:{run}"),
            Actor::Trigger { trigger } => format!("trigger:{trigger}"),
            Actor::Tool { tool, run } => format!("tool:{tool}:{run}"),
        }
    }
}

/// CONTRACTS.md section 1.1. **Closed set.** Adding a kind is a minor version bump;
/// changing one is major.
///
/// The whole set is declared here even though M0b Session A writes only a handful, because
/// section 1's retention rule is absolute: *journal event payloads must stay decodable
/// forever — no expiry, ever.* An event kind that stops decoding makes every event after it
/// unreplayable, which destroys invariant 7 and the migration path in one stroke. Declaring
/// the vocabulary up front is what keeps a later addition additive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    // -- conversation & runs ----------------------------------------------------------
    TurnStarted,
    ModelStep,
    Interrupted,
    Checkpointed,
    RunSpawned,
    RunPaused,
    RunCompleted,
    RunFailed,
    RunCancelled,
    SteerReceived,
    ProviderFailedOver,

    // -- tools & permission -----------------------------------------------------------
    ToolRequested,
    PermissionDecided,
    ApprovalRequested,
    ApprovalGranted,
    ApprovalDenied,
    ToolCompleted,
    ToolFailed,
    EgressBlocked,
    /// The run's trust floor fell and will never rise again (ADR-023).
    ///
    /// Journalled because it is a permanent, run-level privilege change: from here on every
    /// model-composed target is blocked, and a reader reconstructing the run needs to know when
    /// that started and why the calls after it were refused.
    TrustFloorLatched,

    // -- memory: writes ---------------------------------------------------------------
    /// The ONLY way a belief comes into existence.
    MemoryWritten,
    MemoryWriteRejected,
    /// What the gate admitted, and why. Enables the section 5.7 audit.
    MemoryInjected,
    /// Explicit `recall` tool use.
    MemoryRecalled,

    // -- memory: forgetting is events, not computed state -----------------------------
    FidelityDemoted,
    Superseded,
    Tombstoned,
    Redacted,
    BlobEvicted,

    // -- consolidation ----------------------------------------------------------------
    ConsolidationRan,
    BeliefsMerged,
    ContradictionResolved,
    SkillInduced,

    // -- sessions & lineage -----------------------------------------------------------
    SessionStarted,
    SessionSummarized,
    SessionSpawned,
    SessionClosed,

    // -- trust ledger: appendable ONLY by Actor::Permission ----------------------------
    ActionObserved,
    PromotionProposed,
    TierGranted,
    TierDemoted,
    VerificationSampled,

    // -- triggers ---------------------------------------------------------------------
    TriggerFired,
    NoticingRaised,
    NoticingDismissed,
}

impl EventKind {
    /// Section 1.1's invariant, expressed where it is enforced.
    ///
    /// *"`TierGranted` and `TierDemoted` are rejected unless `actor == Actor::Permission`."*
    pub fn requires_permission_actor(self) -> bool {
        matches!(self, EventKind::TierGranted | EventKind::TierDemoted)
    }

    pub fn as_str(self) -> &'static str {
        // Round-tripping through serde keeps this in step with the wire spelling rather
        // than duplicating a 40-arm match that could drift from it.
        match serde_json::to_value(self) {
            Ok(serde_json::Value::String(s)) => Box::leak(s.into_boxed_str()),
            _ => unreachable!("EventKind serializes to a string"),
        }
    }
}

/// What a caller hands to `Journal::append`.
///
/// Note what is **not** here: `seq`, `ts`, and `signature`. The journal stamps all three.
/// There is no way for a caller to supply them, which is how invariant 2 — *every belief has
/// a birth certificate* — is structural rather than checked. A memory without provenance is
/// not rejected at read time; it is unrepresentable at write time.
#[derive(Debug, Clone)]
pub struct AppendRequest {
    pub trace_id: TraceId,
    pub session_id: Option<String>,
    pub run_id: Option<String>,
    pub actor: Actor,
    pub kind: EventKind,
    /// Small and structured. Large content goes by `ContentRef` (section 2).
    ///
    /// The journal **does not interpret this** (ARCHITECTURE section 2.1: *"the journal does
    /// not know what a memory means"*). It signs the exact serialized bytes and stores them.
    pub payload: serde_json::Value,
}

/// A durable, signed entry. Constructed only by the journal.
///
/// `signature` has no public constructor and no setter: the only way to obtain a
/// `JournalEvent` is to read one back or to have the journal produce it. **There is no
/// unsigned variant**, which is what makes the 0% unsigned-write ASR target (K3) structural
/// rather than a matter of filtering.
#[derive(Debug, Clone, Serialize)]
pub struct JournalEvent {
    pub seq: Seq,
    /// UTC millis, from the caller-supplied clock, **never** a system clock and never
    /// model-supplied. Section 4.5 is binding on every path reachable from the three
    /// interfaces, and the journal is on all three.
    pub ts: i64,
    pub trace_id: TraceId,
    pub session_id: Option<String>,
    pub run_id: Option<String>,
    pub actor: Actor,
    pub kind: EventKind,
    pub payload: serde_json::Value,
    pub(crate) signature: String,
    pub(crate) prev_signature: String,
}

impl JournalEvent {
    pub fn signature(&self) -> &str {
        &self.signature
    }
    pub fn prev_signature(&self) -> &str {
        &self.prev_signature
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_model_is_not_an_actor() {
        // Section 1: "The model is not in this enum. That is the point."
        // Serializing every variant and asserting the set is what stops a future `Model`
        // variant from being added quietly -- it would delete invariants 2 and 3 together.
        let all = [
            Actor::Harness,
            Actor::User { surface: "terminal".into() },
            Actor::Permission,
            Actor::Consolidation { run: "r".into() },
            Actor::Trigger { trigger: "t".into() },
            Actor::Tool { tool: "web".into(), run: "r".into() },
        ];
        let names: Vec<String> = all.iter().map(|a| a.canonical()).collect();
        assert!(
            !names.iter().any(|n| n.contains("model")),
            "no actor may denote the model: {names:?}"
        );
        assert_eq!(all.len(), 6, "a variant was added or removed; is it the model?");
    }

    #[test]
    fn only_permission_may_grant_a_tier() {
        assert!(EventKind::TierGranted.requires_permission_actor());
        assert!(EventKind::TierDemoted.requires_permission_actor());
        assert!(!EventKind::MemoryWritten.requires_permission_actor());
    }
}
