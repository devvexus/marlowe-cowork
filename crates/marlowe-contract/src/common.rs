//! Shared vocabulary for the section 4 boundary.
//!
//! Every closed set here is closed *in the type*: there is no `Other(String)` variant
//! anywhere in this file, and there must never be one. Section 4.6 is explicit —
//! *"an unrecognized value is a load-time error on both sides. It is never mapped to a
//! default."* A catch-all variant is how that rule dies quietly: the laundering suite would
//! keep passing while measuring nothing, and if the default were trusted it would hide the
//! failure while causing it.
//!
//! serde gives us that for free. An unknown string fails to deserialize, the eval adapter
//! turns the failure into a section 4.0.4 class A `malformed_body`, and the run aborts —
//! which is the loud outcome the contract asks for.

use serde::{Deserialize, Serialize};

/// CONTRACTS.md section 4.5 — the injectable clock.
///
/// **Normative and binding: on any path reachable from sections 4.1, 4.6 or 4.7 the
/// implementation MUST NOT read a system clock.** Every timestamp derives from this value.
/// In production the harness supplies the real clock; under eval M0a supplies a synthetic
/// one. Same code path, so the eval exercises shipping behaviour rather than a test double.
///
/// This type is `Copy` so it can be threaded through every call without ceremony. Making it
/// awkward to pass would create pressure to reach for a global instead, and the global
/// would be a system clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Clock {
    pub now_ms: i64,
}

impl Clock {
    pub fn new(now_ms: i64) -> Self {
        Self { now_ms }
    }

    /// Advance by a duration. Used for `silent_until`, never for "what time is it".
    pub fn plus_ms(&self, delta: i64) -> i64 {
        self.now_ms + delta
    }
}

/// CONTRACTS.md section 3.3. **Ordered — the ordering IS the propagation rule.**
///
/// `derive(PartialOrd, Ord)` is load-bearing, not decoration: worst-case propagation is
/// `min()` over the lineage, so the discriminant order is the algorithm. Reordering these
/// variants silently changes what trust means.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustClass {
    /// web, inbound mail, MCP server output, third-party skill output
    UntrustedContent = 0,
    /// the model concluded it
    AgentInferred = 1,
    /// the HARNESS computed it: exit codes, hashes, line counts
    AgentObserved = 2,
    /// the user said it, on an authenticated surface
    UserAsserted = 3,
}

/// CONTRACTS.md section 4.6. Closed set; wire values are snake_case.
///
/// **`Agent` is implementation-only and never appears on the section 4 wire.** The harness
/// emits the other eight and has no construction site for this one; `Channel` occurs in
/// exactly one wire type — `Origin`, inside `IngestRequest` — which travels harness ->
/// implementation and is never echoed back, so no eval deserializer can ever be handed
/// `"agent"`. Adding it therefore widens what this side *accepts* by one string nobody sends,
/// which is why a closed set could gain a member without breaking the binding. Recorded as an
/// amendment to section 4.6 (ADR-062 section 4, Option B), not made silently.
///
/// **New variants go at the END.** These discriminants are ordered by `Ord`; serde keys off
/// the name, so appending is invisible on the wire while an insertion would silently renumber
/// every variant after it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    Terminal,
    Voice,
    Messaging,
    Email,
    Web,
    Mcp,
    ToolOutput,
    File,
    /// A harness-mediated reader over external bytes: a quarantined child condensed content
    /// the harness fetched, and the resulting claim is recorded under the reader rather than
    /// under the page. `Channel::Web` would record a provenance the harness *knows to be
    /// false* — the page never emitted those bytes.
    ///
    /// **NOTHING CONSTRUCTS THIS VARIANT.** Its only consumer is `ingest_external`, which has
    /// no caller (ADR-062 section 2.1, section 7 — layer 3 is unreachable in the shipped
    /// daemon, and that is the correct state until Session D's caller exists). The slot is
    /// declared and classified; it is not a live path, and reading it as one would be exactly
    /// the declared-control-nothing-reads mistake this file's other comments are about.
    Agent,
}

/// CONTRACTS.md section 4.6. Closed set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Speaker {
    User,
    Assistant,
    Tool,
}

/// CONTRACTS.md section 3.1 `FidelityTier`, as it appears on the wire in section 4.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Fidelity {
    Tombstone = 0,
    Gist = 1,
    Summary = 2,
    Record = 3,
}

/// CONTRACTS.md section 3.2 `Payload` variants, snake_cased for the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PayloadKind {
    Episode,
    Fact,
    Entity,
    Edge,
    Procedure,
    Commitment,
    Person,
    Relationship,
    VoiceParams,
    Noticing,
}

/// CONTRACTS.md section 4.2 — a closed set, given explicitly.
///
/// All four values describe *having nothing to inject*, which is what makes the mapping
/// total: every response either injects something or abstains, and never both or neither.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AbstentionReason {
    NoCandidateAboveThreshold,
    NoCandidates,
    BudgetExhausted,
    DegradedPath,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trust_ordering_is_the_propagation_rule() {
        // Section 3.3: effective_trust is min() over the lineage. If this ordering is ever
        // reordered, propagation silently changes meaning -- so it is asserted, not assumed.
        assert!(TrustClass::UntrustedContent < TrustClass::AgentInferred);
        assert!(TrustClass::AgentInferred < TrustClass::AgentObserved);
        assert!(TrustClass::AgentObserved < TrustClass::UserAsserted);

        let lineage = [TrustClass::UserAsserted, TrustClass::UntrustedContent];
        assert_eq!(
            lineage.iter().copied().min().unwrap(),
            TrustClass::UntrustedContent,
            "one untrusted parent must drag the whole derivation down"
        );
    }

    #[test]
    fn unknown_channel_is_an_error_not_a_default() {
        // The rule section 4.6 states in as many words. This is the test that keeps a
        // future `Other(String)` variant from being added without someone noticing.
        let err = serde_json::from_str::<Channel>("\"carrier_pigeon\"");
        assert!(err.is_err(), "an unknown channel must fail to deserialize");
    }

    #[test]
    fn the_agent_channel_round_trips_as_agent() {
        // The spelling is not cosmetic: `external_turn_id` (marlowe-daemon) feeds this exact
        // serde string, length-prefixed, into a UUIDv5, so the belief id derived for a
        // harness-mediated reader is a function of it. Pinning it here means a rename shows up
        // as a failing test rather than as belief ids that quietly stop matching.
        let json = serde_json::to_string(&Channel::Agent).unwrap();
        assert_eq!(json, "\"agent\"");
        assert_eq!(serde_json::from_str::<Channel>(&json).unwrap(), Channel::Agent);
    }

    #[test]
    fn wire_spellings_are_snake_case() {
        assert_eq!(
            serde_json::to_string(&TrustClass::UntrustedContent).unwrap(),
            "\"untrusted_content\""
        );
        assert_eq!(
            serde_json::to_string(&Channel::ToolOutput).unwrap(),
            "\"tool_output\""
        );
        assert_eq!(serde_json::to_string(&Channel::Agent).unwrap(), "\"agent\"");
        assert_eq!(
            serde_json::to_string(&Fidelity::Summary).unwrap(),
            "\"summary\""
        );
    }
}
