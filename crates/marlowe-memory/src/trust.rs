//! CONTRACTS.md section 3.3 — trust derivation and worst-case propagation.
//!
//! This module is the one the laundering suite is pointed at. Two rules, and they are not
//! symmetric:
//!
//! - **Channel decides trust.** The table below is *total over the closed set with no
//!   default arm*. Section 4.6: *"an unrecognized value is a load-time error on both sides.
//!   It is never mapped to a default."*
//! - **Actor may only ever reject, never elevate.** `origin.actor` is a free string on the
//!   wire, so it is attacker-controllable in exactly the way `channel` is not. A rule that
//!   let it *raise* trust would be a self-service trust escalation; a rule that lets it
//!   *refuse a write* costs an attacker nothing they had.
//!
//! The asymmetry is the whole design. It is why the forged `actor: "permission:grant"` turn
//! in the poisoning suite comes back in `rejected` rather than being quietly written at some
//! lower trust class.

use marlowe_contract::{Channel, TrustClass};

/// Section 3.3, and the reason the laundering suite can measure anything.
///
/// **There is no catch-all arm and there must never be one.** Rust's exhaustiveness checking
/// is doing real work here: adding a `Channel` variant fails to compile until someone decides
/// what it is worth, which is the load-time error section 4.6 asks for. A `_ =>` arm would
/// turn that compile error into a silent default, the laundering suite would keep passing
/// while measuring nothing, and if the default were trusted it would hide the failure while
/// causing it.
///
/// **The distinction that makes this usable** (section 3.3): trust class tracks *the
/// authority of the origin*, not the safety of the bytes. A fact the harness computed — an
/// exit code, a hash, a line count — is `AgentObserved`. A fact the model extracted from
/// bytes inherits the bytes' origin class.
pub fn trust_for_channel(channel: Channel) -> TrustClass {
    match channel {
        // The user said it, on an authenticated surface.
        Channel::Terminal => TrustClass::UserAsserted,
        Channel::Voice => TrustClass::UserAsserted,

        // Inbound from a third party. Section 3.3 names these explicitly as untrusted:
        // "web, inbound mail, MCP server output, third-party skill output".
        Channel::Web => TrustClass::UntrustedContent,
        Channel::Email => TrustClass::UntrustedContent,
        Channel::Messaging => TrustClass::UntrustedContent,
        Channel::Mcp => TrustClass::UntrustedContent,

        // The harness computed this: exit codes, hashes, line counts.
        Channel::ToolOutput => TrustClass::AgentObserved,

        // A file's *bytes* carry no authority: a vendored README, a checked-in config, or a
        // downloaded artifact all arrive this way, and section 9 calls the workspace write a
        // durable channel into a later run's context. Reading a file is not the user
        // speaking.
        Channel::File => TrustClass::UntrustedContent,

        // A harness-mediated reader over external bytes. The *structure* is harness-authored,
        // which is why this is not `Channel::Web` — the page never emitted those bytes, the
        // quarantined reader did, and recording Web would assert a provenance the harness
        // knows to be false. But the *content* is a condensation of untrusted input, and
        // section 3.3's rule is that a fact the model extracted from bytes inherits the bytes'
        // origin class. So an honest origin buys no more authority than the bytes had
        // (ADR-062 section 3.2). Note the contrast with `ToolOutput` above: that is
        // `AgentObserved` because the harness *computed* the value; here the harness only
        // carried bytes it did not author.
        Channel::Agent => TrustClass::UntrustedContent,
    }
}

/// Section 3.3's propagation algorithm, verbatim:
///
/// ```text
/// fn effective_trust(own, parents) = parents.map(effective_trust).chain(own).min()
/// ```
///
/// Computed once at write time and stored, because parents' effective trust is already
/// transitively minimal. **No content signal is consulted**, because content signals cannot
/// survive derivation — that is the entire finding behind HP6.
///
/// A fact the agent wrote in fluent prose, derived through four LLM transformations from a
/// web page, carries `UntrustedContent`.
pub fn effective_trust(own: TrustClass, parents: &[TrustClass]) -> TrustClass {
    parents.iter().copied().chain(std::iter::once(own)).min().unwrap_or(own)
}

/// Actor namespaces that no inbound turn may claim.
///
/// These name components that are `Actor` variants in section 1 — the harness's own
/// bookkeeping, the permission layer, consolidation, triggers. A turn arriving over a wire
/// and asserting one of them is claiming to *be* a privileged component of the system.
///
/// Section 1 already makes such a write unrepresentable at the journal: the model is not in
/// the `Actor` enum, and ingest constructs `Actor::Tool`/`Actor::User` regardless of what a
/// turn claims. So this check does not stop a write that would otherwise succeed — it makes
/// the attempt **visible**. The poisoning suite asserts on exactly that: a planted
/// unauthorized write must appear in `rejected`, because *"a suite that plants a malformed or
/// unauthorized write must be able to see it refused rather than infer refusal from
/// absence."*
pub const RESERVED_ACTOR_PREFIXES: &[&str] = &[
    "permission:",
    "harness:",
    "consolidation:",
    "trigger:",
    "system:",
];

/// Why a turn was refused. A free string on the wire (section 4.6 pins no vocabulary), but
/// the reason must *name the rule*, not merely say no.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RejectionReason {
    ReservedActor { claimed: String },
}

impl RejectionReason {
    pub fn as_wire_string(&self) -> String {
        match self {
            RejectionReason::ReservedActor { claimed } => {
                format!("reserved_actor: origin.actor {claimed:?} claims a namespace reserved \
                         for harness components; an inbound turn may not assert one")
            }
        }
    }
}

/// The actor check. Returns `Some(reason)` when the turn must be visibly refused.
///
/// Note the return type: there is no variant that *raises* trust, so this function cannot be
/// used to elevate. That is enforced by the signature rather than by discipline.
pub fn check_actor(actor: &str) -> Option<RejectionReason> {
    let lowered = actor.to_ascii_lowercase();
    for prefix in RESERVED_ACTOR_PREFIXES {
        if lowered.starts_with(prefix) {
            return Some(RejectionReason::ReservedActor {
                claimed: actor.to_string(),
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_is_untrusted_no_matter_what() {
        assert_eq!(trust_for_channel(Channel::Web), TrustClass::UntrustedContent);
    }

    #[test]
    fn the_agent_channel_is_untrusted_content() {
        // A lookup table with no default arm; this is the thin test that is right for one,
        // and it is the same shape as `web_is_untrusted_no_matter_what` above.
        //
        // WHAT IT DOES NOT MEASURE, said plainly so nobody reads it as more: NO PRODUCTION
        // CODE IN `crates/` CONSTRUCTS `Channel::Agent`. (This comment used to read "nothing
        // in `crates/`", which the assertion five lines below refutes: unit tests construct
        // the variant, which is what a test module is for. A sentence a neighbouring line
        // disproves is how a true claim stops being believed.) Its only consumer is
        // `ingest_external`, which has no caller (ADR-062 sections 2.1 and 7), so this
        // asserts a classification for a slot, not the behaviour of a live path. On the day
        // Session D's caller exists, the test that matters is a loop-level one showing a
        // belief from this channel refused as a composed target.
        assert_eq!(trust_for_channel(Channel::Agent), TrustClass::UntrustedContent);

        // The discriminating pair: `ToolOutput` is `AgentObserved` because the harness
        // *computed* the value. Asserting them together shows the table separates the two
        // cases rather than agreeing everywhere.
        assert_eq!(trust_for_channel(Channel::ToolOutput), TrustClass::AgentObserved);
    }

    #[test]
    fn one_untrusted_parent_drags_the_whole_derivation_down() {
        // HP6: "A fact the agent wrote in fluent prose, derived through four LLM
        // transformations from a web page, carries UntrustedContent."
        let parents = [
            TrustClass::UserAsserted,
            TrustClass::AgentObserved,
            TrustClass::UntrustedContent,
        ];
        assert_eq!(
            effective_trust(TrustClass::UserAsserted, &parents),
            TrustClass::UntrustedContent
        );
    }

    #[test]
    fn derivation_depth_does_not_launder() {
        // Four hops, each one re-asserting the highest trust it can. The minimum is sticky
        // because every parent's effective_trust is already transitively minimal.
        let mut trust = trust_for_channel(Channel::Web);
        for _ in 0..4 {
            trust = effective_trust(TrustClass::UserAsserted, &[trust]);
        }
        assert_eq!(trust, TrustClass::UntrustedContent);
    }

    #[test]
    fn no_parents_means_own_trust() {
        assert_eq!(
            effective_trust(TrustClass::AgentObserved, &[]),
            TrustClass::AgentObserved
        );
    }

    #[test]
    fn reserved_actors_are_refused() {
        assert!(check_actor("permission:grant").is_some());
        assert!(check_actor("PERMISSION:grant").is_some(), "case must not be a bypass");
        assert!(check_actor("consolidation:run-7").is_some());
    }

    #[test]
    fn ordinary_actors_pass() {
        assert!(check_actor("user:primary").is_none());
        assert!(check_actor("tool:web").is_none());
        assert!(check_actor("").is_none());
    }

    #[test]
    fn the_actor_check_cannot_elevate() {
        // Structural: `check_actor` returns Option<RejectionReason>. There is no TrustClass
        // in its return type, so no call site can use it to raise trust even by mistake.
        // This test exists to make that a stated property rather than an accident of the
        // current signature.
        let outcome = check_actor("permission:grant");
        assert!(matches!(outcome, Some(RejectionReason::ReservedActor { .. })));
    }
}
