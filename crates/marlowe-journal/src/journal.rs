//! CONTRACTS.md section 1.2 — the append API.
//!
//! *"The ONLY write path. Stamps seq, ts, and signature. There is no unsigned variant."*

use marlowe_contract::Clock;
use rusqlite::{Connection, OptionalExtension};

use crate::error::JournalError;
use crate::event::{Actor, AppendRequest, EventKind, JournalEvent, Seq};
use crate::profile::Profile;
use crate::signature::{self, SignedFields, SigningKey, GENESIS};
use crate::store;

/// Proof that the caller is an operator or the audit path, not the loop.
///
/// ARCHITECTURE invariant 8: *"Replay is not model-reachable."* Replay is absent from the
/// tool registry at every exposure tier, and the journal lives outside the model's
/// filesystem scope. This token is the third lock: `replay` cannot be called without one,
/// and the only constructor names what it is for.
///
/// Without this, forgetting is cosmetic — section 5.4's worst-failure clause is *a memory
/// the user can no longer surface but the system silently acted on*, and a model that can
/// replay the log has exactly that.
pub struct OperatorCapability(());

impl OperatorCapability {
    /// Call sites are auditable by grepping for this function. There is no other way in.
    pub fn for_operator_or_audit() -> Self {
        Self(())
    }
}

/// One stored event without its signatures, as [`Journal::replay_meta`] returns it.
#[derive(Debug, Clone)]
pub struct ReplayedEvent {
    pub seq: Seq,
    /// Journal timestamp, UTC millis.
    pub ts: i64,
    pub session_id: Option<String>,
    pub kind: EventKind,
    pub payload: serde_json::Value,
}

pub struct Journal {
    conn: Connection,
    key: SigningKey,
    last_seq: Seq,
    last_signature: String,
}

impl Journal {
    /// Open the journal in an already-opened profile, verifying the whole chain.
    pub fn open(profile: &Profile) -> Result<Self, JournalError> {
        let conn = store::open(&profile.journal_path())?;
        let mut journal = Self {
            conn,
            key: profile.key().clone(),
            last_seq: 0,
            last_signature: GENESIS.to_string(),
        };
        journal.verify_chain()?;
        Ok(journal)
    }

    /// Open the journal trusting everything up to a checkpoint: the event at `seq` must carry
    /// `signature`, and only the events after it are verified. The checkpoint comes from a
    /// sealed snapshot this profile wrote, so boot cost follows the tail, not the history.
    /// [`JournalError::CheckpointMismatch`] means the caller should fall back to [`Self::open`].
    pub fn open_from_checkpoint(profile: &Profile, seq: Seq, signature: &str) -> Result<Self, JournalError> {
        let conn = store::open(&profile.journal_path())?;
        let stored: Option<String> = conn
            .query_row("SELECT signature FROM journal WHERE seq = ?1", [seq as i64], |r| r.get(0))
            .optional()?;
        if seq == 0 || stored.as_deref() != Some(signature) {
            return Err(JournalError::CheckpointMismatch { seq });
        }
        let mut journal = Self {
            conn,
            key: profile.key().clone(),
            last_seq: seq,
            last_signature: signature.to_string(),
        };
        journal.verify_from(seq, signature.to_string())?;
        Ok(journal)
    }

    pub fn last_seq(&self) -> Seq {
        self.last_seq
    }

    /// Signature of the last event (`genesis` when empty); a snapshot stores it as its checkpoint.
    pub fn last_signature(&self) -> &str {
        &self.last_signature
    }

    /// **The only write path.** Stamps `seq`, `ts` and `signature`.
    ///
    /// `clock` is the caller-supplied clock and the only source of time here. Section 4.5
    /// forbids reading a system clock on any path reachable from the three interfaces, and
    /// ingest reaches this function.
    pub fn append(
        &mut self,
        clock: Clock,
        req: AppendRequest,
    ) -> Result<JournalEvent, JournalError> {
        // Section 1.1, enforced at append: the trust ledger's grant events are appendable
        // only by the permission component. Because the model can never be an Actor at all,
        // self-promotion is unrepresentable rather than merely rejected -- this check exists
        // for the remaining case, a harness component reaching for the wrong actor.
        if req.kind.requires_permission_actor() && req.actor != Actor::Permission {
            return Err(JournalError::ActorMayNotEmit {
                kind: req.kind,
                actor: req.actor.canonical(),
            });
        }

        let seq = self.last_seq + 1;
        let ts = clock.now_ms;
        // Serialized once, signed and stored as the same bytes.
        let payload_json = serde_json::to_string(&req.payload)?;
        let actor_canonical = req.actor.canonical();

        let fields = SignedFields {
            seq,
            ts,
            trace_id: &req.trace_id,
            session_id: req.session_id.as_deref(),
            run_id: req.run_id.as_deref(),
            actor_canonical: &actor_canonical,
            kind: req.kind,
            payload_json: &payload_json,
            prev_signature: &self.last_signature,
        };
        let sig = signature::sign(&self.key, &fields);

        self.conn.execute(
            "INSERT INTO journal
                (seq, ts, trace_id, session_id, run_id, actor, kind, payload,
                 prev_signature, signature)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            rusqlite::params![
                seq as i64,
                ts,
                req.trace_id.to_string(),
                req.session_id.as_deref(),
                req.run_id.as_deref(),
                req.actor.canonical(),
                req.kind.as_str(),
                payload_json,
                self.last_signature,
                sig,
            ],
        )?;

        let event = JournalEvent {
            seq,
            ts,
            trace_id: req.trace_id,
            session_id: req.session_id,
            run_id: req.run_id,
            actor: req.actor,
            kind: req.kind,
            payload: req.payload,
            signature: sig.clone(),
            prev_signature: std::mem::replace(&mut self.last_signature, sig),
        };
        self.last_seq = seq;
        Ok(event)
    }

    /// Walk the log, recompute every signature, and reject the first break.
    ///
    /// Called on open. It is O(n) and that is accepted: the alternative is trusting a log
    /// whose integrity is the basis of invariant 7. When the log grows enough for this to
    /// hurt, the answer is a checkpointed verification, not a skipped one.
    pub fn verify_chain(&mut self) -> Result<(), JournalError> {
        self.verify_from(0, GENESIS.to_string())
    }

    /// Verify every event after `after`, chaining from `prev` (the signature at `after`).
    fn verify_from(&mut self, after: Seq, prev: String) -> Result<(), JournalError> {
        let mut stmt = self.conn.prepare(
            "SELECT seq, ts, trace_id, session_id, run_id, actor, kind, payload,
                    prev_signature, signature
             FROM journal WHERE seq > ?1 ORDER BY seq ASC",
        )?;

        let mut expected_seq: Seq = after + 1;
        let mut prev = prev;
        let mut last_seq: Seq = after;

        let mut rows = stmt.query([after as i64])?;
        while let Some(row) = rows.next()? {
            let seq: i64 = row.get(0)?;
            let seq = seq as Seq;
            if seq != expected_seq {
                return Err(JournalError::SequenceGap {
                    expected: expected_seq,
                    found: seq,
                });
            }

            let stored_prev: String = row.get(8)?;
            let stored_sig: String = row.get(9)?;
            if stored_prev != prev {
                return Err(JournalError::ChainBroken { seq });
            }

            let trace_raw: String = row.get(2)?;
            let trace_id = trace_raw
                .parse::<uuid::Uuid>()
                .map_err(|_| JournalError::ChainBroken { seq })?;
            let session_id: Option<String> = row.get(3)?;
            let run_id: Option<String> = row.get(4)?;
            let actor_raw: String = row.get(5)?;
            let kind_raw: String = row.get(6)?;
            let payload_json: String = row.get(7)?;
            let ts: i64 = row.get(1)?;

            let kind: EventKind = serde_json::from_value(serde_json::Value::String(kind_raw))
                .map_err(|_| JournalError::ChainBroken { seq })?;

            // The stored canonical actor string is fed straight back in. No parse, no second
            // encoder, so a rewritten actor column fails the signature and an intact one
            // never produces a phantom break.
            let fields = SignedFields {
                seq,
                ts,
                trace_id: &trace_id,
                session_id: session_id.as_deref(),
                run_id: run_id.as_deref(),
                actor_canonical: &actor_raw,
                kind,
                payload_json: &payload_json,
                prev_signature: &stored_prev,
            };
            if !signature::verify(&self.key, &fields, &stored_sig) {
                return Err(JournalError::ChainBroken { seq });
            }

            prev = stored_sig;
            last_seq = seq;
            expected_seq += 1;
        }

        self.last_seq = last_seq;
        self.last_signature = prev;
        Ok(())
    }

    /// Operator and audit only. See [`OperatorCapability`].
    pub fn replay(
        &self,
        cap: &OperatorCapability,
        kind: Option<EventKind>,
    ) -> Result<Vec<(Seq, EventKind, serde_json::Value)>, JournalError> {
        let mut out = Vec::new();
        self.replay_after(cap, 0, |seq, this_kind, payload_raw| {
            if kind.is_none_or(|want| want == this_kind) {
                out.push((seq, this_kind, serde_json::from_str(payload_raw)?));
            }
            Ok::<(), JournalError>(())
        })?;
        Ok(out)
    }

    /// [`Self::replay`] with each event's `ts` and `session_id`. Operator and audit only.
    pub fn replay_meta(
        &self,
        _cap: &OperatorCapability,
        kind: Option<EventKind>,
    ) -> Result<Vec<ReplayedEvent>, JournalError> {
        let mut stmt = self.conn.prepare("SELECT seq, ts, session_id, kind, payload FROM journal ORDER BY seq ASC")?;
        let mut rows = stmt.query([])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            let kind_raw: String = row.get(3)?;
            let this_kind: EventKind = serde_json::from_value(serde_json::Value::String(kind_raw))?;
            if kind.is_some_and(|want| want != this_kind) {
                continue;
            }
            let payload_raw: String = row.get(4)?;
            out.push(ReplayedEvent {
                seq: row.get::<_, i64>(0)? as Seq,
                ts: row.get(1)?,
                session_id: row.get(2)?,
                kind: this_kind,
                payload: serde_json::from_str(&payload_raw)?,
            });
        }
        Ok(out)
    }

    /// Streams every event with `seq > after`, in order, to `f` with its raw payload JSON.
    /// Ranged in SQL and never collected, so a snapshot's tail costs only the tail.
    /// Operator and audit only.
    pub fn replay_after<E: From<JournalError>>(
        &self,
        _cap: &OperatorCapability,
        after: Seq,
        mut f: impl FnMut(Seq, EventKind, &str) -> Result<(), E>,
    ) -> Result<(), E> {
        let mut stmt = self
            .conn
            .prepare("SELECT seq, kind, payload FROM journal WHERE seq > ?1 ORDER BY seq ASC")
            .map_err(JournalError::from)?;
        let mut rows = stmt.query([after as i64]).map_err(JournalError::from)?;
        while let Some(row) = rows.next().map_err(JournalError::from)? {
            let seq: i64 = row.get(0).map_err(JournalError::from)?;
            let kind_raw: String = row.get(1).map_err(JournalError::from)?;
            let payload_raw: String = row.get(2).map_err(JournalError::from)?;
            let kind: EventKind = serde_json::from_value(serde_json::Value::String(kind_raw)).map_err(JournalError::from)?;
            f(seq as Seq, kind, &payload_raw)?;
        }
        Ok(())
    }
}
