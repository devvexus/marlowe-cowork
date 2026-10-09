//! The belief store — a **materialized view over the journal**, never an authority.
//!
//! ARCHITECTURE section 2.3: *"Fully rebuildable from the log. That is what makes invariant 5
//! tractable — correcting a belief is an append plus a view refresh, not a destructive edit."*
//!
//! Two properties this module exists to hold:
//!
//! 1. **The rebuild is a versioned derivation.** [`DERIVATION_VERSION`] is recorded in the
//!    profile and checked here. This is the mechanism ADR-009 rests on: adding a derived
//!    per-entry field later is a version bump plus a rebuild, never a journal migration and
//!    never a contract major bump. It is testable, which a nullable column would not be.
//! 2. **The rebuild restores current state, not full fidelity** (invariant 9). Fidelity is a
//!    fold over typed events. An entry whose fidelity cannot be resolved from the log is a
//!    hard error, because the tempting default — assume `Record` — resurrects forgotten
//!    memories, and nothing downstream would observe it.

use std::collections::BTreeMap;

use marlowe_contract::{Fidelity, PayloadKind, TrustClass};
use marlowe_journal::{EventKind, Journal, OperatorCapability};
use serde::{Deserialize, Serialize};

use crate::entry::{MemoryEntry, MemoryId};
use crate::error::MemoryError;

pub use marlowe_journal::DERIVATION_VERSION;

/// The payload of a `MemoryWritten` event.
///
/// Everything needed to rebuild the entry, and **the initial fidelity is explicit**. It would
/// be smaller to omit it and assume `Record` on replay; that assumption is exactly the
/// unobservable mismatch invariant 9 warns about, so the field is carried on every write.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryWrittenPayload {
    pub id: MemoryId,
    pub text: String,
    pub payload_kind: PayloadKind,
    pub source_turn_id: String,
    pub source_session_id: String,
    pub trust_class: TrustClass,
    pub effective_trust: TrustClass,
    pub derivation: Vec<MemoryId>,
    /// The turn's own `occurred_at_ms` from §4.6. See [`MemoryEntry::occurred_at_ms`].
    ///
    /// **Not optional, and not defaulted.** A `#[serde(default)]` here would let a pre-Session-H
    /// journal replay into entries whose `occurred_at_ms` is silently 0 — every turn in the same
    /// derived session, pruning quietly degenerate, and nothing looking wrong. `DERIVATION_VERSION`
    /// is bumped instead, so an older profile fails to open with a version mismatch that names the
    /// problem. Load-time error over sensible default, per CLAUDE.md.
    pub occurred_at_ms: i64,
    pub created_at: i64,
    pub silent_until: Option<i64>,
    pub fidelity: Fidelity,
}

/// The payload of a `MemoryWriteRejected` event.
///
/// A refusal is journaled, not merely returned. The write did not happen, but *the attempt*
/// is part of what invariant 7 has to be able to reconstruct — and the poisoning suite's
/// unsigned-write family asserts a refusal was visible rather than inferred from absence.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryWriteRejectedPayload {
    pub turn_id: String,
    pub session_id: String,
    pub reason: String,
}

/// Beliefs, keyed by id.
///
/// `BTreeMap`, not `HashMap`. Iteration order reaches the wire through the injected set, and
/// hash order is randomised per process — two runs would produce two different `run.jsonl`
/// files and `marlowe-eval repro` would fail with two hashes and no explanation. A workspace
/// test bans the type outright.
#[derive(Debug, Default)]
pub struct BeliefStore {
    entries: BTreeMap<MemoryId, MemoryEntry>,
}

impl BeliefStore {
    /// Rebuild the belief store by replaying the log.
    ///
    /// This is the *only* constructor that reads history. There is no "load from a snapshot"
    /// path, so the derivation is exercised on every open rather than on a rare migration —
    /// a rebuild that is only run when it matters is a rebuild that is broken when it matters.
    pub fn derive(journal: &Journal, derivation_version: u32) -> Result<Self, MemoryError> {
        if derivation_version != DERIVATION_VERSION {
            return Err(MemoryError::DerivationVersionMismatch {
                found: derivation_version,
                expected: DERIVATION_VERSION,
            });
        }

        let cap = OperatorCapability::for_operator_or_audit();
        let mut entries: BTreeMap<MemoryId, MemoryEntry> = BTreeMap::new();

        for (seq, kind, payload) in journal.replay(&cap, None)? {
            match kind {
                EventKind::MemoryWritten => {
                    let p: MemoryWrittenPayload = serde_json::from_value(payload)
                        .map_err(|e| MemoryError::UndecodablePayload { seq, source: e })?;
                    entries.insert(
                        p.id.clone(),
                        MemoryEntry {
                            id: p.id,
                            text: p.text,
                            payload_kind: p.payload_kind,
                            embedding_ref: None,
                            source_turn_id: p.source_turn_id,
                            source_session_id: p.source_session_id,
                            trust_class: p.trust_class,
                            effective_trust: p.effective_trust,
                            derivation: p.derivation,
                            origin_event: seq,
                            occurred_at_ms: p.occurred_at_ms,
                            created_at: p.created_at,
                            last_accessed: p.created_at,
                            access_count: 0,
                            confidence: 1.0,
                            activation: 1.0,
                            // Explicit, from the event. Never defaulted -- see the module
                            // docstring and invariant 9.
                            fidelity: p.fidelity,
                            silent_until: p.silent_until,
                            supersedes: Vec::new(),
                            superseded_by: None,
                        },
                    );
                }

                // Forgetting is events, not computed state. Session A writes none of these
                // yet -- the contradiction detector is retrieval quality and waits for a
                // later session -- but the fold handles them now so that the rebuild is
                // complete rather than complete-so-far.
                EventKind::FidelityDemoted => {
                    let p: FidelityDemotedPayload = serde_json::from_value(payload)
                        .map_err(|e| MemoryError::UndecodablePayload { seq, source: e })?;
                    let target = entries
                        .get_mut(&p.id)
                        .ok_or_else(|| MemoryError::EventForUnknownMemory { seq, id: p.id.clone() })?;
                    target.fidelity = p.to;
                }
                EventKind::Superseded => {
                    let p: SupersededPayload = serde_json::from_value(payload)
                        .map_err(|e| MemoryError::UndecodablePayload { seq, source: e })?;
                    let target = entries
                        .get_mut(&p.id)
                        .ok_or_else(|| MemoryError::EventForUnknownMemory { seq, id: p.id.clone() })?;
                    target.superseded_by = Some(p.by.clone());
                    if let Some(winner) = entries.get_mut(&p.by) {
                        winner.supersedes.push(p.id);
                    }
                }
                EventKind::Tombstoned => {
                    let p: TombstonedPayload = serde_json::from_value(payload)
                        .map_err(|e| MemoryError::UndecodablePayload { seq, source: e })?;
                    let target = entries
                        .get_mut(&p.id)
                        .ok_or_else(|| MemoryError::EventForUnknownMemory { seq, id: p.id.clone() })?;
                    target.fidelity = Fidelity::Tombstone;
                    target.text.clear();
                }

                // Everything else is not a belief-store input. Ignored by kind rather than
                // by a catch-all on shape, so a new memory-affecting kind has to be handled
                // here on purpose.
                _ => {}
            }
        }

        Ok(Self { entries })
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    pub fn get(&self, id: &str) -> Option<&MemoryEntry> {
        self.entries.get(id)
    }
    pub fn insert(&mut self, entry: MemoryEntry) {
        self.entries.insert(entry.id.clone(), entry);
    }

    /// Fold a supersession edge into the live view.
    ///
    /// **This is deliberately the same two mutations `derive` performs for `EventKind::Superseded`
    /// — the loser's `superseded_by`, then the winner's `supersedes`.** The incremental path and
    /// the replay path have to agree, because §4.3's exclusion (2) reads the first field and the
    /// audit trail reads the second; a rebuild that disagreed with the live store would change
    /// what is retrievable with nothing observing it. `tests/ingest.rs` asserts the agreement
    /// against a real journal rather than leaving it to this comment.
    ///
    /// Silent on an unknown id, because the journal is the authority: `derive` raises
    /// `EventForUnknownMemory` on replay, which is where an unresolvable edge must be caught.
    /// Raising here as well would make an already-journaled event unreplayable in memory only.
    pub fn supersede(&mut self, id: &str, by: &str) {
        if let Some(loser) = self.entries.get_mut(id) {
            loser.superseded_by = Some(by.to_string());
        } else {
            return;
        }
        if let Some(winner) = self.entries.get_mut(by) {
            winner.supersedes.push(id.to_string());
        }
    }

    /// Apply a `Tombstoned` event to the in-memory view.
    ///
    /// **The text is cleared, not merely the tier demoted**, matching what `derive` does when it
    /// folds the same event on replay. Two paths producing different in-memory states from one
    /// journal entry is the two-sides-silently-disagree shape, and here it would be invisible until
    /// a restart changed what `recall` returned.
    ///
    /// Silent on an unknown id, for the reason [`Self::supersede`] gives: the journal is the
    /// authority, and `derive` raises `EventForUnknownMemory` where an unresolvable edge belongs.
    pub fn tombstone(&mut self, id: &str) {
        if let Some(e) = self.entries.get_mut(id) {
            e.fidelity = Fidelity::Tombstone;
            e.text.clear();
        }
    }

    /// Section 4.3 — the auto-injection candidate set, live-only.
    ///
    /// ADR-003 makes the live-only hot index a **requirement, not an optimization**: the
    /// measured curve has an unpartitioned index crossing the 120 ms budget at a tombstone
    /// fraction of ~0.45 and reaching 258 ms by 0.80, while the partitioned one is flat.
    /// Session A has no separate physical index yet, so this filter *is* the partition —
    /// and it applies all three exclusions, which is the part that must not drift.
    pub fn injection_candidates(&self, now_ms: i64) -> Vec<&MemoryEntry> {
        self.entries
            .values()
            .filter(|e| e.is_injection_candidate(now_ms))
            .collect()
    }

    /// Section 4.3 — reachable by explicit `recall` only: hot ∪ cold.
    ///
    /// Tombstones and unmatured entries live here and **never** compete for injection
    /// precision. Scoring a tombstone as a candidate would penalise the headline metric for
    /// working correctly.
    pub fn recall_candidates(&self) -> Vec<&MemoryEntry> {
        self.entries.values().collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FidelityDemotedPayload {
    pub id: MemoryId,
    pub to: Fidelity,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupersededPayload {
    pub id: MemoryId,
    pub by: MemoryId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TombstonedPayload {
    pub id: MemoryId,
}
