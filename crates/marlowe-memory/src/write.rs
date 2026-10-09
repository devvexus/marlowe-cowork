//! The product write path: typed, scoped beliefs whose trust comes from their author.
//!
//! [`crate::ingest::ingest`] stays the §4.6 path the eval drives and [`crate::claim`] the
//! model's `remember` request; this is what the memory service uses for user turns, assistant
//! turns, explicit `remember`/`correct` from any surface, episode summaries and legacy import.
//! Same rules as both: trust derived here and never declared by the caller (except an import,
//! which replays classes a verified journal already derived), worst case over lineage, the ingest
//! clock for maturation, journal first then store, refusals journalled.

use marlowe_contract::{Clock, PayloadKind, TrustClass};
use marlowe_journal::{Actor, AppendRequest, EventKind, Journal, TraceId};

use crate::claim::ClaimRejected;
use crate::entry::{memory_id, MaturationPolicy, MemoryId};
use crate::error::MemoryError;
use crate::store::{BeliefStore, MemoryWriteRejectedPayload, MemoryWrittenPayload};
use crate::tags::Tags;
use crate::trust::effective_trust;

/// Who authored a belief. The trust class follows from this and nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Author {
    /// The user on an authenticated surface: a user turn, the CLI, the Memory view.
    User,
    /// The model, through a memory tool. `run_floor` is the run's **latched** floor (ADR-038),
    /// so a run that read untrusted content writes `UntrustedContent`.
    Agent { run_floor: TrustClass },
    /// The harness computed it: exit codes, hashes, run results.
    Harness,
    /// Third-party bytes: web, mail, MCP output.
    External,
    /// Replayed from another chain-verified journal. Keeps the classes that journal derived;
    /// the effective class can only be lowered (by `trust_class` or lineage), never raised.
    Imported { trust_class: TrustClass, effective_trust: TrustClass },
}

impl Author {
    /// Own class, before lineage.
    pub fn own_trust(self) -> TrustClass {
        match self {
            Author::User => TrustClass::UserAsserted,
            Author::Agent { run_floor } => TrustClass::AgentInferred.min(run_floor),
            Author::Harness => TrustClass::AgentObserved,
            Author::External => TrustClass::UntrustedContent,
            Author::Imported { trust_class, .. } => trust_class,
        }
    }

    /// Own class folded with the parents' effective classes, worst case.
    fn effective(self, parents: &[TrustClass]) -> TrustClass {
        let own = self.own_trust();
        let carried = match self {
            Author::Imported { effective_trust, .. } => effective_trust.min(own),
            Author::User | Author::Agent { .. } | Author::Harness | Author::External => own,
        };
        effective_trust(carried, parents)
    }
}

/// One belief to write.
#[derive(Debug, Clone)]
pub struct BeliefWrite<'a> {
    pub session_id: &'a str,
    /// The source turn, e.g. `"turn-12"`, `"episode"`, `"import-341"`. Enters the id.
    pub turn_id: &'a str,
    /// Recorded on the journal event only; never enters the id (it differs between replays).
    pub run_id: Option<&'a str>,
    pub text: &'a str,
    pub author: Author,
    /// Ids this belief is derived from. Each must exist; an unknown one is a refusal.
    pub derived_from: &'a [MemoryId],
    /// When the underlying thing happened. Grouping only; maturation never reads it.
    pub occurred_at_ms: i64,
    /// Kind (should be set), scope, speaker, source note, reason.
    pub tags: Tags,
    pub maturation: MaturationPolicy,
}

/// What a successful write produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BeliefReceipt {
    pub id: MemoryId,
    pub seq: u64,
    pub trust_class: TrustClass,
    /// After worst-case propagation. Reported, not assumed.
    pub effective_trust: TrustClass,
    /// When it may be auto-injected; `None` means now.
    pub silent_until: Option<i64>,
}

/// Write one belief, or journal why not.
pub fn write_belief(
    journal: &mut Journal,
    beliefs: &mut BeliefStore,
    clock: Clock,
    write: &BeliefWrite<'_>,
) -> Result<Result<BeliefReceipt, ClaimRejected>, MemoryError> {
    let trace_id = TraceId::new_v5(&TraceId::NAMESPACE_OID, write.session_id.as_bytes());

    let rejection = if write.text.trim().is_empty() {
        Some(ClaimRejected::EmptyClaim)
    } else {
        write
            .derived_from
            .iter()
            .find(|id| beliefs.get(id).is_none())
            .map(|id| ClaimRejected::UnknownParent(id.clone()))
    };
    if let Some(reason) = rejection {
        journal.append(
            clock,
            AppendRequest {
                trace_id,
                session_id: Some(write.session_id.to_string()),
                run_id: write.run_id.map(str::to_string),
                actor: Actor::Harness,
                kind: EventKind::MemoryWriteRejected,
                payload: serde_json::to_value(MemoryWriteRejectedPayload {
                    turn_id: write.turn_id.to_string(),
                    session_id: write.session_id.to_string(),
                    reason: reason.as_wire_string(),
                })?,
            },
        )?;
        return Ok(Err(reason));
    }

    let own = write.author.own_trust();
    let effective = write.author.effective(&parent_trust(beliefs, write.derived_from));
    let silent_until = write.maturation.silent_until(clock, effective);
    let id = next_id(beliefs, write.session_id, write.turn_id);

    let payload = MemoryWrittenPayload {
        id: id.clone(),
        text: write.text.to_string(),
        payload_kind: write.tags.kind.map_or(PayloadKind::Fact, |k| k.payload_kind()),
        source_turn_id: write.turn_id.to_string(),
        source_session_id: write.session_id.to_string(),
        trust_class: own,
        effective_trust: effective,
        derivation: write.derived_from.to_vec(),
        occurred_at_ms: write.occurred_at_ms,
        created_at: clock.now_ms,
        silent_until,
        fidelity: marlowe_contract::Fidelity::Record,
        tags: write.tags.clone(),
    };
    let event = journal.append(
        clock,
        AppendRequest {
            trace_id,
            session_id: Some(write.session_id.to_string()),
            run_id: write.run_id.map(str::to_string),
            actor: Actor::Harness,
            kind: EventKind::MemoryWritten,
            payload: serde_json::to_value(&payload)?,
        },
    )?;
    beliefs.insert(payload.to_entry(event.seq));

    Ok(Ok(BeliefReceipt {
        id,
        seq: event.seq,
        trust_class: own,
        effective_trust: effective,
        silent_until,
    }))
}

/// Supersede `target` with a new belief.
///
/// Refused when the correction would carry **less** authority than the target
/// ([`ClaimRejected::WouldLowerAuthority`]): superseding evicts the target from injection, so a
/// tainted run must not be able to replace what the user said. The user can correct anything.
pub fn correct_belief(
    journal: &mut Journal,
    beliefs: &mut BeliefStore,
    clock: Clock,
    write: &BeliefWrite<'_>,
    target: &str,
) -> Result<Result<BeliefReceipt, ClaimRejected>, MemoryError> {
    let Some(existing) = beliefs.get(target) else {
        return Ok(Err(ClaimRejected::UnknownParent(target.to_string())));
    };
    let existing_trust = existing.effective_trust;
    let correction = write.author.effective(&parent_trust(beliefs, write.derived_from));
    if correction < existing_trust {
        return Ok(Err(ClaimRejected::WouldLowerAuthority {
            target: target.to_string(),
            existing: existing_trust,
            correction,
        }));
    }

    let receipt = match write_belief(journal, beliefs, clock, write)? {
        Ok(r) => r,
        Err(rejected) => return Ok(Err(rejected)),
    };
    journal.append(
        clock,
        AppendRequest {
            trace_id: TraceId::new_v5(&TraceId::NAMESPACE_OID, write.session_id.as_bytes()),
            session_id: Some(write.session_id.to_string()),
            run_id: write.run_id.map(str::to_string),
            actor: Actor::Harness,
            kind: EventKind::Superseded,
            payload: serde_json::to_value(crate::store::SupersededPayload {
                id: target.to_string(),
                by: receipt.id.clone(),
            })?,
        },
    )?;
    beliefs.supersede(target, &receipt.id);
    Ok(Ok(receipt))
}

fn parent_trust(beliefs: &BeliefStore, ids: &[MemoryId]) -> Vec<TrustClass> {
    ids.iter().filter_map(|id| beliefs.get(id).map(|e| e.effective_trust)).collect()
}

/// `m-{session}-{turn}-{n}` with the first `n` not already taken. A function of store state,
/// never of the clock, so identical replays get identical ids.
fn next_id(beliefs: &BeliefStore, session: &str, turn: &str) -> MemoryId {
    let mut index = 0usize;
    loop {
        let id = memory_id(session, turn, index);
        if beliefs.get(&id).is_none() {
            return id;
        }
        index += 1;
    }
}
