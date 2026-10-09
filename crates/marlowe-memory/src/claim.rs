//! CONTRACTS.md section 3.5 — `remember`, the model's write request.
//!
//! **This is the path that did not exist.** [`ingest`](crate::ingest) writes a session's *turns*;
//! nothing wrote a model-supplied *claim*, so `MemoryHost::remember` had no implementation to be
//! wired to and the daemon's `memory: None` was concealing an absence rather than a disconnection.
//!
//! Two things about it are load-bearing and are stated here rather than left to the reader:
//!
//! **1. `remember` is a REQUEST, not a write.** CONTRACTS §3.5 names this a naming hazard, because
//! the model-visible tool reads like a write and is not one. The harness assigns the id, resolves
//! the derivation, computes the trust class, signs and appends — or refuses. Invariant 2 depends on
//! there being no other path, and there is no unsigned variant here for the same reason `ingest`
//! has none.
//!
//! **2. The trust class is `min(AgentInferred, run_floor)` — ADR-038.** The floor arrives as a
//! parameter and is never re-derived. See [`ClaimWrite::run_floor`].
//!
//! A refusal is **journalled**, never inferred from absence. §4.6's rule for ingest applies with
//! equal force here: *"a suite that plants a malformed or unauthorized write must be able to see it
//! refused rather than infer refusal from absence."* A `remember` that silently did nothing would
//! be indistinguishable from one that was never attempted.

use marlowe_contract::{Clock, PayloadKind, TrustClass};
use marlowe_journal::{Actor, AppendRequest, EventKind, Journal, TraceId};

use crate::entry::{memory_id, MemoryEntry, MATURATION_WINDOW_MS};
use crate::error::MemoryError;
use crate::store::{BeliefStore, MemoryWriteRejectedPayload, MemoryWrittenPayload};
use crate::trust::effective_trust;

/// One `remember` request, with everything the harness needs and nothing the model supplied about
/// its own authority.
///
/// **There is no `trust_class` field and there must never be one.** §4.6 declares `origin` and
/// never `trust_class` for exactly this reason: a caller-supplied class is a self-service trust
/// escalation. The class is derived here from [`Self::run_floor`], which the *harness* latched.
pub struct ClaimWrite<'a> {
    pub session_id: &'a str,
    /// Recorded on the entry so a claim can be traced to the run that asked for it. It does **not**
    /// enter the memory id — `RunId` is generated per run, and an id containing one would differ
    /// between two otherwise identical replays.
    pub run_id: &'a str,
    pub text: &'a str,
    pub payload_kind: PayloadKind,
    /// Ids this claim is derived from. Each **must already exist**; an unknown parent is a
    /// rejection rather than an empty lineage, because silently dropping a parent would raise the
    /// derived belief's effective trust to whatever the claim asked for.
    pub derived_from: &'a [String],
    /// **The run's LATCHED trust floor. ADR-038.**
    ///
    /// Not the context view's floor: the view is trimmable, so a block evicted to stay inside
    /// budget would let a write happen at a class the run had already forfeited — the identical
    /// hole ADR-023 closed for tool arguments, except that a memory outlives the run that wrote it
    /// and therefore carries the mistake forward.
    pub run_floor: TrustClass,
}

/// Why a claim was refused. CONTRACTS §3.5's `Rejected`, narrowed to what this build can produce.
///
/// `QuotaExceeded` and `ContradictsPinned` are **deliberately absent rather than stubbed**: there
/// is no quota mechanism and no pinning mechanism in this build, and a variant that can never be
/// constructed is a promise the code does not keep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimRejected {
    /// A `derived_from` id that no `MemoryWritten` event created.
    UnknownParent(String),
    /// Empty or whitespace-only claim text. Refused rather than written, because an empty belief
    /// is retrievable, scores against every query, and asserts nothing.
    EmptyClaim,
    /// A `correct` whose replacement carries **less** authority than what it would supersede.
    ///
    /// Superseding evicts the target from auto-injection (§4.3 exclusion 2), so allowing this would
    /// let a tainted run delete a user-asserted belief through an ordinary tool call.
    WouldLowerAuthority {
        target: String,
        existing: TrustClass,
        correction: TrustClass,
    },
}

impl ClaimRejected {
    /// The wire form, which is also what the model reads. It says what was wrong **and** what an
    /// acceptable call looks like: a refusal the model cannot act on is worse than one it cannot
    /// read, because it acts on it confidently (see `ApprovalGate::is_interactive`).
    pub fn as_wire_string(&self) -> String {
        match self {
            Self::UnknownParent(id) => format!(
                "derived_from names {id}, which is not a memory this store has. Pass only ids \
                 returned by an earlier recall, or omit derived_from entirely."
            ),
            Self::EmptyClaim => {
                "the claim text is empty. Pass the fact to be remembered as `text`.".to_string()
            }
            Self::WouldLowerAuthority { target, existing, correction } => format!(
                "{target} was recorded as {existing:?} and this correction would only be \
                 {correction:?}, because of what this run has read. Superseding it would remove a \
                 better-sourced belief. Use `remember` to record the new fact alongside it instead."
            ),
        }
    }
}

/// What the caller gets back. CONTRACTS §3.5's `WriteReceipt`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteReceipt {
    pub id: String,
    pub seq: u64,
    /// **May be lower than `AgentInferred` — worst case wins.** Reported rather than assumed so a
    /// caller can see that a run which read a page wrote at `UntrustedContent`.
    pub effective_trust: TrustClass,
    /// When this belief becomes eligible for auto-injection. §4.3 exclusion (3).
    pub silent_until: i64,
}

/// Write one model-authored claim, or refuse it.
///
/// Mirrors [`crate::ingest::ingest`]'s structure deliberately — same trust call, same maturation
/// stamp, same journal-then-store order, same rejection-is-journalled rule. Two write paths that
/// diverged in any of those would be two different memory systems sharing a store.
pub fn remember_claim(
    journal: &mut Journal,
    beliefs: &mut BeliefStore,
    clock: Clock,
    write: &ClaimWrite<'_>,
) -> Result<Result<WriteReceipt, ClaimRejected>, MemoryError> {
    // Deterministic, and derived from the session rather than generated. A replayed run must
    // produce the same trace id or its log cannot be diffed against the original.
    let trace_id = TraceId::new_v5(&TraceId::NAMESPACE_OID, write.session_id.as_bytes());

    // ── refusals, journalled ────────────────────────────────────────────────────────
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
                run_id: Some(write.run_id.to_string()),
                // The HARNESS refused this, so the harness is who acted. The model's claim never
                // becomes an `Actor` — it is data about a refused request, not an identity.
                actor: Actor::Harness,
                kind: EventKind::MemoryWriteRejected,
                payload: serde_json::to_value(MemoryWriteRejectedPayload {
                    turn_id: format!("claim:{}", write.run_id),
                    session_id: write.session_id.to_string(),
                    reason: reason.as_wire_string(),
                })?,
            },
        )?;
        return Ok(Err(reason));
    }

    // ── trust: ADR-038, then §3.3's propagation unchanged ───────────────────────────
    //
    // The `min` is applied HERE rather than by the loop. `trust.rs` owns §3.3, and a caller that
    // handed down a finished class would be a second implementation of the rule this crate exists
    // to be the only implementation of.
    let own_trust = TrustClass::AgentInferred.min(write.run_floor);
    let parents: Vec<TrustClass> = write
        .derived_from
        .iter()
        .filter_map(|id| beliefs.get(id).map(|e| e.effective_trust))
        .collect();
    let effective = effective_trust(own_trust, &parents);

    // ── identity ────────────────────────────────────────────────────────────────────
    //
    // No clock and no run id, so two identical replays produce identical ids —
    // `entry::memory_ids_do_not_contain_a_timestamp` states the rule and clock probe test A is
    // what enforces it. The index counts this session's existing claims, which is a function of
    // store state and therefore reproduces.
    let prefix = format!("m-{}-claim-", write.session_id);
    let index = beliefs.recall_candidates().iter().filter(|e| e.id.starts_with(&prefix)).count();
    let id = memory_id(write.session_id, "claim", index);

    // **The ingest clock, never the claim's own idea of when it happened.** Maturation asks how
    // long a belief has existed *in the system* and had a chance to be contradicted. A claim that
    // could name its own age would arrive pre-matured, which is single-exposure poisoning with the
    // defence handed over in the payload.
    let silent_until = clock.plus_ms(MATURATION_WINDOW_MS);

    let payload = MemoryWrittenPayload {
        id: id.clone(),
        text: write.text.to_string(),
        payload_kind: write.payload_kind,
        source_turn_id: format!("claim:{}", write.run_id),
        source_session_id: write.session_id.to_string(),
        trust_class: own_trust,
        effective_trust: effective,
        derivation: write.derived_from.to_vec(),
        occurred_at_ms: clock.now_ms,
        created_at: clock.now_ms,
        silent_until: Some(silent_until),
        fidelity: marlowe_contract::Fidelity::Record,
    };

    // The journal stamps seq, ts and signature. This is the only write path; there is no unsigned
    // variant, which is what makes K3 structural rather than filtered.
    let event = journal.append(
        clock,
        AppendRequest {
            trace_id,
            session_id: Some(write.session_id.to_string()),
            run_id: Some(write.run_id.to_string()),
            actor: Actor::Harness,
            kind: EventKind::MemoryWritten,
            payload: serde_json::to_value(&payload)?,
        },
    )?;

    beliefs.insert(MemoryEntry {
        id: id.clone(),
        text: write.text.to_string(),
        payload_kind: write.payload_kind,
        embedding_ref: None,
        source_turn_id: format!("claim:{}", write.run_id),
        source_session_id: write.session_id.to_string(),
        trust_class: own_trust,
        effective_trust: effective,
        derivation: write.derived_from.to_vec(),
        origin_event: event.seq,
        occurred_at_ms: clock.now_ms,
        created_at: clock.now_ms,
        last_accessed: clock.now_ms,
        access_count: 0,
        confidence: 1.0,
        activation: 1.0,
        fidelity: marlowe_contract::Fidelity::Record,
        silent_until: Some(silent_until),
        supersedes: Vec::new(),
        superseded_by: None,
    });

    Ok(Ok(WriteReceipt { id, seq: event.seq, effective_trust: effective, silent_until }))
}

/// CONTRACTS §3.5's `correct`: supersede a belief with a new one.
///
/// **Built on [`remember_claim`] rather than beside it**, so a correction is written under exactly
/// the same trust rule (ADR-038) as any other claim. A second write path here would be a second
/// answer to "what class does a model-authored belief carry".
///
/// # A correction may not lower a belief's authority, and this is a NEW rule
///
/// Superseding removes the target from the auto-injection candidate set (§4.3 exclusion 2). So a
/// correction written at a *lower* effective trust than its target would let untrusted content
/// evict a trusted belief — the same shape as the consolidation-merge finding in
/// `runs/m2-session-d/OPEN-QUESTION-0-AUDIT.md`, except reachable deliberately through a tool.
///
/// This refuses it. **That is a rule nobody has ratified**, and it is flagged as such: it is the
/// conservative direction, it costs a legitimate case (a tainted run correcting an older
/// inference), and the cost is recoverable — `remember` still works, only the supersession is
/// refused.
pub fn correct_claim(
    journal: &mut Journal,
    beliefs: &mut BeliefStore,
    clock: Clock,
    write: &ClaimWrite<'_>,
    target: &str,
) -> Result<Result<WriteReceipt, ClaimRejected>, MemoryError> {
    let Some(existing) = beliefs.get(target) else {
        return Ok(Err(ClaimRejected::UnknownParent(target.to_string())));
    };
    let existing_trust = existing.effective_trust;

    // Computed the way `remember_claim` will compute it, so the check and the write cannot
    // disagree about what the correction is worth.
    let own = TrustClass::AgentInferred.min(write.run_floor);
    let parents: Vec<TrustClass> = write
        .derived_from
        .iter()
        .filter_map(|id| beliefs.get(id).map(|e| e.effective_trust))
        .collect();
    let correction = effective_trust(own, &parents);
    if correction < existing_trust {
        return Ok(Err(ClaimRejected::WouldLowerAuthority {
            target: target.to_string(),
            existing: existing_trust,
            correction,
        }));
    }

    let receipt = match remember_claim(journal, beliefs, clock, write)? {
        Ok(r) => r,
        Err(rejected) => return Ok(Err(rejected)),
    };

    let trace_id = TraceId::new_v5(&TraceId::NAMESPACE_OID, write.session_id.as_bytes());
    journal.append(
        clock,
        AppendRequest {
            trace_id,
            session_id: Some(write.session_id.to_string()),
            run_id: Some(write.run_id.to_string()),
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

/// CONTRACTS §3.5's `forget`, in its `Tombstone` mode.
///
/// **`RedactDestructive` is deliberately not offered.** ADR-009's crypto-shredding path does not
/// exist, and a `mode` argument with one working arm is a promise the code does not keep.
///
/// The *event* stays in the log forever, so what happened remains auditable even though the text is
/// gone — and `derive` folds that event the same way, so a restart reproduces exactly this state
/// rather than a second interpretation of it.
pub fn forget_claim(
    journal: &mut Journal,
    beliefs: &mut BeliefStore,
    clock: Clock,
    session_id: &str,
    run_id: &str,
    target: &str,
) -> Result<Result<String, ClaimRejected>, MemoryError> {
    if beliefs.get(target).is_none() {
        return Ok(Err(ClaimRejected::UnknownParent(target.to_string())));
    }
    let trace_id = TraceId::new_v5(&TraceId::NAMESPACE_OID, session_id.as_bytes());
    journal.append(
        clock,
        AppendRequest {
            trace_id,
            session_id: Some(session_id.to_string()),
            run_id: Some(run_id.to_string()),
            // **`Actor::Harness`, not the model.** Forgetting is the harness acting on a request,
            // exactly as `remember` is. The model is not in the `Actor` enum and must not be.
            actor: Actor::Harness,
            kind: EventKind::Tombstoned,
            payload: serde_json::to_value(crate::store::TombstonedPayload {
                id: target.to_string(),
            })?,
        },
    )?;
    beliefs.tombstone(target);
    Ok(Ok(target.to_string()))
}
