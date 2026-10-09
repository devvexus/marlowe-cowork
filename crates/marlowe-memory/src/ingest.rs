//! CONTRACTS.md section 4.6 — ingest.
//!
//! Loads a history, and is how the poisoning suite plants its attacks. Two outcomes per turn
//! and both are first-class:
//!
//! - **written** — a belief came into existence, with the trust class *this code derived*.
//!   The laundering suite compares against that value and never against anything the eval
//!   supplied, because section 4.6 declares `origin` and never `trust_class`.
//! - **rejected** — the write was refused, **visibly**. *"A suite that plants a malformed or
//!   unauthorized write must be able to see it refused rather than infer refusal from
//!   absence."* K3 is measured on exactly this.

use marlowe_contract::{Clock, IngestRequest, PayloadKind, RejectedWrite, Written};
use marlowe_journal::{Actor, AppendRequest, EventKind, Journal, TraceId};

use crate::entry::{memory_id, MemoryEntry, MATURATION_WINDOW_MS};
use crate::error::MemoryError;
use crate::store::{BeliefStore, MemoryWrittenPayload, MemoryWriteRejectedPayload};
use crate::trust::{check_actor, effective_trust, trust_for_channel};

pub struct IngestOutcome {
    pub written: Vec<Written>,
    pub rejected: Vec<RejectedWrite>,
}

/// Ingest one session's turns.
///
/// `clock` is the only source of time. Section 4.5 is binding on every path reachable from
/// section 4.6, and this is that path.
pub fn ingest(
    journal: &mut Journal,
    beliefs: &mut BeliefStore,
    request: &IngestRequest,
) -> Result<IngestOutcome, MemoryError> {
    let clock = request.clock;
    let mut written = Vec::new();
    let mut rejected = Vec::new();

    // Deterministic: one trace per ingested session, derived from the session id rather than
    // generated randomly, so a replayed run produces the same trace ids. Trace ids never
    // reach the section 4 wire, but a log that differs between two identical runs is a log
    // nobody can diff.
    let trace_id = TraceId::new_v5(&TraceId::NAMESPACE_OID, request.session_id.as_bytes());

    for (index, turn) in request.turns.iter().enumerate() {
        // -- the actor check: may reject, may never elevate ---------------------------
        if let Some(reason) = check_actor(&turn.origin.actor) {
            let wire_reason = reason.as_wire_string();
            journal.append(
                clock,
                AppendRequest {
                    trace_id,
                    session_id: Some(request.session_id.clone()),
                    run_id: None,
                    // Note the actor on the *event*: the harness refused this, so the harness
                    // is who acted. The turn's claimed actor never becomes an `Actor` -- it
                    // is data about a refused request, not an identity the system adopts.
                    actor: Actor::Harness,
                    kind: EventKind::MemoryWriteRejected,
                    payload: serde_json::to_value(MemoryWriteRejectedPayload {
                        turn_id: turn.turn_id.clone(),
                        session_id: request.session_id.clone(),
                        reason: wire_reason.clone(),
                    })?,
                },
            )?;
            rejected.push(RejectedWrite {
                turn_id: turn.turn_id.clone(),
                reason: wire_reason,
            });
            continue;
        }

        // -- trust derivation: channel decides, and the table has no default arm -------
        let own_trust = trust_for_channel(turn.origin.channel);
        // Session A writes no derived beliefs, so the lineage is empty and effective trust
        // equals own trust. The call is made anyway rather than short-circuited: the
        // propagation rule is the thing under test, and a path that only runs once
        // consolidation exists is a path that is broken when consolidation arrives.
        let derivation: Vec<String> = Vec::new();
        let parents: Vec<_> = derivation
            .iter()
            .filter_map(|id| beliefs.get(id).map(|e| e.effective_trust))
            .collect();
        let effective = effective_trust(own_trust, &parents);

        let id = memory_id(&request.session_id, &turn.turn_id, index);
        let silent_until = Some(clock.plus_ms(MATURATION_WINDOW_MS));

        let payload = MemoryWrittenPayload {
            id: id.clone(),
            text: turn.text.clone(),
            payload_kind: PayloadKind::Episode,
            source_turn_id: turn.turn_id.clone(),
            source_session_id: request.session_id.clone(),
            trust_class: own_trust,
            effective_trust: effective,
            derivation: derivation.clone(),
            // The turn's OWN time, carried through instead of discarded. See
            // MemoryEntry::occurred_at_ms for why this is not created_at and why maturation
            // must never read it.
            occurred_at_ms: turn.occurred_at_ms,
            created_at: clock.now_ms,
            silent_until,
            // Explicit on every write. Replay must never have to assume a tier.
            fidelity: marlowe_contract::Fidelity::Record,
        };

        // The journal stamps seq, ts and signature. This is the only write path; there is no
        // unsigned variant, which is what makes K3 structural rather than filtered.
        let event = journal.append(
            clock,
            AppendRequest {
                trace_id,
                session_id: Some(request.session_id.clone()),
                run_id: None,
                actor: Actor::Harness,
                kind: EventKind::MemoryWritten,
                payload: serde_json::to_value(&payload)?,
            },
        )?;

        beliefs.insert(MemoryEntry {
            id: id.clone(),
            text: turn.text.clone(),
            payload_kind: PayloadKind::Episode,
            embedding_ref: None,
            source_turn_id: turn.turn_id.clone(),
            source_session_id: request.session_id.clone(),
            trust_class: own_trust,
            effective_trust: effective,
            derivation,
            origin_event: event.seq,
            occurred_at_ms: turn.occurred_at_ms,
            created_at: clock.now_ms,
            last_accessed: clock.now_ms,
            access_count: 0,
            confidence: 1.0,
            activation: 1.0,
            fidelity: marlowe_contract::Fidelity::Record,
            silent_until,
            supersedes: Vec::new(),
            superseded_by: None,
        });

        written.push(Written {
            turn_id: turn.turn_id.clone(),
            memory_ids: vec![id],
            // What we DERIVED. The laundering suite reads this field.
            effective_trust: effective,
        });
    }

    Ok(IngestOutcome { written, rejected })
}

/// The clock used for a turn's maturation deadline.
///
/// Deliberately the **ingest clock**, not the turn's `occurred_at_ms`. Maturation asks how
/// long a belief has existed *in the system* and had a chance to be contradicted, not how old
/// the underlying utterance is. Using `occurred_at_ms` would let an attacker backdate a
/// planted turn and have it arrive pre-matured — which is single-exposure poisoning with the
/// defence handed over in the payload.
#[allow(dead_code)]
fn maturation_deadline(clock: Clock) -> i64 {
    clock.plus_ms(MATURATION_WINDOW_MS)
}
