//! CONTRACTS.md §9 — the permission decision and what the user is shown.

use marlowe_tools::{ConsequenceLevel, ToolId};
use serde::{Deserialize, Serialize};

use crate::taint::TaintSet;

/// Monotonic within a process. **Deliberately not a UUID:** a decision id appears in journal
/// events, and a run replayed at a fixed clock and seed must reproduce bit-identically
/// (M0a's acceptance, and the standing `repro --runs 2` check). A random id would put
/// per-process entropy into the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DecisionId(pub u64);

/// CONTRACTS.md §9.1 — per **action class**, not per app and not globally.
///
/// `shape` is a hash over the argument *names and roles*, not their values: two calls to the
/// same tool with the same argument shape are the same class however different the strings
/// are. Hashing values would make every call its own class and the ledger would never
/// accumulate evidence for anything.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct ActionClass {
    pub tool: ToolId,
    pub shape: u64,
    pub label: String,
}

/// CONTRACTS.md §9.1's ladder. **The ledger that moves an entry along it is M6** — at M2 the
/// tier is whatever the run's autonomy control says, and nothing promotes itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    Observe = 0,
    Suggest = 1,
    Draft = 2,
    Confirm = 3,
    Act = 4,
    Silent = 5,
}

/// §B9 is risk-tiered per §8.2. The tier picks the overlay's doubled border colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskTier {
    /// Routine writes. Batched.
    Routine,
    /// Irreversible or high-consequence. Blocking.
    Irreversible,
}

impl RiskTier {
    pub fn for_consequence(c: ConsequenceLevel) -> Self {
        match c {
            ConsequenceLevel::Inert | ConsequenceLevel::Reversible => RiskTier::Routine,
            ConsequenceLevel::Consequential | ConsequenceLevel::Irreversible => {
                RiskTier::Irreversible
            }
        }
    }
}

/// Why this call is unusual **for its class**. §9.1: novelty drops one tier automatically,
/// regardless of the class's standing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NoveltyReason {
    FirstTimeForClass,
    UnfamiliarTarget { target: String },
    UnusualAmount { micros_usd: u64 },
}

/// CONTRACTS.md §9 — **states blast radius, not the command.**
///
/// Not `Run: rm -rf ./build?` but `Delete 1,204 files in ./build · not recoverable`. There is
/// no field for the command string, which is the point: a surface cannot show what it was
/// never given, and a user approving a command they cannot evaluate is approval theatre.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BlastRadius {
    pub verb: String,
    pub scope: String,
    pub reversible: bool,
    pub novelty: Option<NoveltyReason>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockReason {
    /// The `(action, target)` rule. The one this layer exists for.
    UntrustedTarget { param: String, origin: marlowe_contract::TrustClass },
    UndeclaredPath { path: String, detail: String },
    EgressNotAllowed { host: String },
    BudgetExceeded { dimension: &'static str },
    TierInsufficient { have: Tier, need: Tier },
    /// A call naming a tool that is not registered, or not exposed to this run.
    ToolNotAvailable { tool: ToolId },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Allowed,
    AllowedBatched { batch: u64 },
    NeedsApproval { tier: RiskTier },
    Blocked { reason: BlockReason },
}

impl Outcome {
    pub fn is_blocked(&self) -> bool {
        matches!(self, Outcome::Blocked { .. })
    }

    pub fn needs_approval(&self) -> bool {
        matches!(self, Outcome::NeedsApproval { .. })
    }
}

/// A short, structured reason line. Free prose would end up in the journal as the only record
/// of *why* a call was allowed, and prose is not queryable by an audit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    ConsequenceLevel(ConsequenceLevel),
    TierAtOrAbove(Tier),
    NoveltyDroppedOneTier,
    AllTargetsTrusted,
    InertNoTargetCheck,
    /// **ADR-032 §3.1 — the host this call would reach, which the run has not been granted.**
    ///
    /// Emitted only on the `AllowApproved` ask-branch, so its presence means the outcome is
    /// `NeedsApproval` *for an egress reason*: `DenyAll` and a declared `Allow` list are terminal
    /// and never reach it.
    ///
    /// # Why the host travels in `reasons` rather than in a field on `Adjudication`
    ///
    /// The loop has to know **which** host a human just approved in order to record the grant,
    /// and it must not re-derive it: two places deciding "which host was this call about" is the
    /// shape where the enforcement site and the recording site silently disagree. The adjudicator
    /// already knows — it is the code that parsed the URL and intersected the two sets — so it
    /// says so, once, in the field that already exists for *why this outcome*.
    ///
    /// The audit story comes free: `reasons` is serialized into `PermissionDecided`, so the
    /// signed journal records the host that was asked about next to the decision that asked.
    /// The value is a parsed, lowercased [`crate::Host`] and never the raw URL — finding D9's
    /// note about attacker prose landing in a field named `host` applies to `BlockReason`, and
    /// this variant does not repeat it.
    EgressHostNeedsApproval { host: String },
}

/// CONTRACTS.md §9.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PermissionDecision {
    pub id: DecisionId,
    pub tool: ToolId,
    pub action_class: ActionClass,
    pub outcome: Outcome,
    /// What the **user** is shown, §B9.
    pub blast_radius: BlastRadius,
    pub taint: TaintSet,
    pub reasons: Vec<Reason>,
}

impl PermissionDecision {
    pub fn blocked(&self) -> Option<&BlockReason> {
        match &self.outcome {
            Outcome::Blocked { reason } => Some(reason),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blast_radius_has_no_field_for_the_command() {
        // Executable documentation for §9's "states blast radius, NOT the command". The
        // assertion is the shape of the struct: serializing it must not produce a `command`
        // key, because there is nowhere for one to come from.
        let br = BlastRadius {
            verb: "delete".into(),
            scope: "1,204 files in ./build".into(),
            reversible: false,
            novelty: None,
        };
        let json = serde_json::to_string(&br).unwrap();
        assert!(!json.contains("command"), "{json}");
        assert!(json.contains("1,204 files"));
    }

    #[test]
    fn risk_tier_follows_consequence() {
        assert_eq!(RiskTier::for_consequence(ConsequenceLevel::Inert), RiskTier::Routine);
        assert_eq!(RiskTier::for_consequence(ConsequenceLevel::Reversible), RiskTier::Routine);
        assert_eq!(
            RiskTier::for_consequence(ConsequenceLevel::Consequential),
            RiskTier::Irreversible
        );
    }

    #[test]
    fn the_tier_ladder_is_ordered() {
        assert!(Tier::Observe < Tier::Draft);
        assert!(Tier::Confirm < Tier::Act);
        assert!(Tier::Act < Tier::Silent);
    }
}
