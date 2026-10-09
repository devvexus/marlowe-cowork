//! CONTRACTS.md section 3.1 — the memory envelope.
//!
//! **One envelope for every belief.** This is what makes memory the spine rather than a
//! module: a commitment and a preference carry identical provenance machinery.

use marlowe_contract::{Fidelity, PayloadKind, TrustClass};
use serde::{Deserialize, Serialize};

/// Section 5.3's engram maturation window.
///
/// A newly written belief enters in a low-activation *silent* state and cannot be
/// auto-injected until it has survived this long on the supplied clock. Section 4.3 calls it
/// *"the cheapest available defence against single-exposure poisoning"* — a fact planted once
/// cannot influence reasoning until it has survived a window during which contradiction can
/// supersede it.
///
/// **Six hours, chosen on design grounds and stated here so the choice is inspectable.**
/// Brief §5.3 requires "corroboration or elapsed stability" and pins no number. Six hours is
/// inside a working day, so a fact the user states in the morning is usable that afternoon,
/// while a single-exposure plant must survive half a day before it can reach the model.
///
/// **It was not tuned to make a probe pass, and that matters here specifically.** The
/// poisoning suite's laundering assertion only observes a derived trust class if the planted
/// memory is actually injected at the trigger query — and if it is never injected, the
/// harness scores the assertion as a *pass*. A window chosen to sit just under the suite's
/// one-day trigger would manufacture a non-vacuous result. So the window is set on its own
/// merits, and the eval adapter reports whether the assertion observed anything, rather than
/// letting a silent pass stand. See `docs/design/` and the Session A notes.
pub const MATURATION_WINDOW_MS: i64 = 6 * 60 * 60 * 1000;

pub type MemoryId = String;

/// Build a memory id.
///
/// **Derived from position, never from time.** Clock probe test A re-runs an identical
/// scenario with every supplied timestamp shifted by ten years and asserts the injected
/// `memory_id`s are unchanged; an id containing a timestamp fails translation invariance and
/// the probe reports `fail_translation_variance`. A workspace test greps for the obvious
/// spelling of this mistake, but this function is where it would actually happen.
pub fn memory_id(session_id: &str, turn_id: &str, index: usize) -> MemoryId {
    format!("m-{session_id}-{turn_id}-{index}")
}

/// Section 3.1. Fields M0b Session A does not yet drive are present and inert rather than
/// absent: the envelope is the contract, and a partial envelope would let a later session
/// think a field was never specified.
///
/// **There is no `structural_signature` field, deliberately — see ADR-009.** A signature
/// computed at write time does not demote when fidelity does and survives crypto-shredding as
/// plaintext residue of a redacted record. The option is preserved by the rebuild being a
/// versioned derivation, not by a nullable column.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub id: MemoryId,
    pub text: String,
    pub payload_kind: PayloadKind,
    /// Section 3.1's `embedding_ref`. `None` throughout Session A: there is no embedder yet,
    /// and ADR-004's local ONNX model arrives with the dense cue.
    pub embedding_ref: Option<String>,

    // -- provenance (brief §5.6; invariant 2) ------------------------------------------
    /// The turn this was derived from. Reported to the harness as `written[].turn_id`, which
    /// is how every evidence-precision number joins.
    pub source_turn_id: String,
    pub source_session_id: String,
    /// OWN class, before propagation.
    pub trust_class: TrustClass,
    /// AFTER worst-case propagation (§3.3). **Use this one.**
    pub effective_trust: TrustClass,
    /// FULL lineage, not just immediate parents.
    pub derivation: Vec<MemoryId>,
    /// The `MemoryWritten` event that created it.
    pub origin_event: u64,

    /// The turn's own `occurred_at_ms`, from the §4.6 request.
    ///
    /// **This is not `created_at` and the difference is the whole reason the field exists.**
    /// `created_at` is the *ingest clock* — the moment the system learned the fact. The harness
    /// ingests an entire history in **one** §4.6 call, so `created_at` is identical for every turn
    /// in that call and carries no ordering whatsoever. `occurred_at_ms` is when the underlying
    /// utterance happened, and it is the only per-turn time the store has.
    ///
    /// Session H added it because §4.6 carries it on the wire and this implementation was
    /// discarding it. [`crate::retrieve::session_keys`] is what needed it.
    ///
    /// **It is never read by the maturation path, and that exclusion is load-bearing.**
    /// [`MemoryEntry::is_matured`] deliberately reads `silent_until`, which is derived from the
    /// ingest clock. Maturation asks how long a belief has existed *in the system* and had a
    /// chance to be contradicted — using an attacker-supplied `occurred_at_ms` would let a planted
    /// turn be backdated and arrive pre-matured, which is single-exposure poisoning with the
    /// defence handed over in the payload. See `ingest.rs`'s `maturation_deadline`.
    ///
    /// Grouping is not a trust decision, which is why reading it *here* is safe: a caller who
    /// backdates turns can influence which candidates are ranked together and cannot influence
    /// whether any of them may be injected at all.
    pub occurred_at_ms: i64,

    // -- lifecycle ---------------------------------------------------------------------
    pub created_at: i64,
    pub last_accessed: i64,
    pub access_count: u32,
    pub confidence: f32,
    pub activation: f32,
    pub fidelity: Fidelity,
    /// Section 5.3 maturation. `Some(t)` means this entry is excluded from auto-injection
    /// until `clock.now >= t`. It remains reachable by explicit `recall`: the maturation bar
    /// is on *unprompted influence*, not on existence.
    pub silent_until: Option<i64>,
    pub supersedes: Vec<MemoryId>,
    pub superseded_by: Option<MemoryId>,
}

impl MemoryEntry {
    /// Section 4.3 exclusion (3). Separated from the other two because it is the one an
    /// implementation can drop while still passing every latency and precision test — the
    /// attack it defends against is temporally decoupled from its trigger.
    pub fn is_matured(&self, now_ms: i64) -> bool {
        self.silent_until.map_or(true, |t| t <= now_ms)
    }

    /// Section 4.3's three exclusions, together, as the auto-injection candidate predicate.
    ///
    /// **Three, not two.** The third is easy to lose.
    pub fn is_injection_candidate(&self, now_ms: i64) -> bool {
        self.fidelity > Fidelity::Tombstone      // (1)
            && self.superseded_by.is_none()      // (2)
            && self.is_matured(now_ms)           // (3)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry() -> MemoryEntry {
        MemoryEntry {
            id: memory_id("s-1", "t-1", 0),
            text: "the deploy job runs on Fridays".into(),
            payload_kind: PayloadKind::Episode,
            embedding_ref: None,
            source_turn_id: "t-1".into(),
            source_session_id: "s-1".into(),
            trust_class: TrustClass::UserAsserted,
            effective_trust: TrustClass::UserAsserted,
            derivation: Vec::new(),
            origin_event: 1,
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

    #[test]
    fn memory_ids_do_not_contain_a_timestamp() {
        // Clock probe test A shifts every supplied time by ten years and compares ids.
        let a = memory_id("s-1", "t-1", 0);
        let b = memory_id("s-1", "t-1", 0);
        assert_eq!(a, b);
        assert!(!a.chars().any(|c| c.is_ascii_digit() && a.contains("17")), "{a}");
    }

    #[test]
    fn an_unmatured_entry_is_not_an_injection_candidate() {
        let e = entry();
        assert!(!e.is_injection_candidate(1_000), "not at write time");
        assert!(
            !e.is_injection_candidate(1_000 + MATURATION_WINDOW_MS - 1),
            "not one millisecond early"
        );
        assert!(e.is_injection_candidate(1_000 + MATURATION_WINDOW_MS));
    }

    #[test]
    fn all_three_exclusions_bite_independently() {
        let now = 1_000 + MATURATION_WINDOW_MS;

        let mut tombstoned = entry();
        tombstoned.fidelity = Fidelity::Tombstone;
        assert!(!tombstoned.is_injection_candidate(now), "exclusion 1");

        let mut superseded = entry();
        superseded.superseded_by = Some("m-other".into());
        assert!(!superseded.is_injection_candidate(now), "exclusion 2");

        let mut silent = entry();
        silent.silent_until = Some(now + 1);
        assert!(!silent.is_injection_candidate(now), "exclusion 3");

        assert!(entry().is_injection_candidate(now), "and a live one passes");
    }
}
