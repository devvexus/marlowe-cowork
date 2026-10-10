//! The belief store — a **materialized view over the journal**, never an authority.
//!
//! ARCHITECTURE section 2.3: *"Fully rebuildable from the log. That is what makes invariant 5
//! tractable — correcting a belief is an append plus a view refresh, not a destructive edit."*
//!
//! Three properties this module exists to hold:
//!
//! 1. **The rebuild is a versioned derivation.** [`DERIVATION_VERSION`] is recorded in the
//!    profile and checked here (ADR-009): a new derived field is a version bump plus a rebuild,
//!    never a journal migration.
//! 2. **The rebuild restores current state, not full fidelity** (invariant 9). Fidelity is a
//!    fold over typed events. An entry whose fidelity cannot be resolved from the log is a hard
//!    error, because the tempting default — assume `Record` — resurrects forgotten memories.
//! 3. **A snapshot is a shortcut through the same fold, not a second authority.** Boot loads a
//!    sealed [`BeliefSnapshot`] and applies only the events after its watermark with the very
//!    function the full derive uses; `tests/snapshot.rs` asserts the two states are equal.

use std::collections::BTreeMap;

use marlowe_contract::{Fidelity, PayloadKind, TrustClass};
use marlowe_journal::{EventKind, Journal, OperatorCapability, Seq, SigningKey};
use serde::{Deserialize, Serialize};

use crate::cue::lexical::LexicalIndex;
use crate::entry::{MemoryEntry, MemoryId};
use crate::error::MemoryError;
use crate::tags::Tags;

pub use marlowe_journal::DERIVATION_VERSION;

/// The payload of a `MemoryWritten` event.
///
/// Everything needed to rebuild the entry, and **the initial fidelity is explicit**: assuming
/// `Record` on replay is exactly the unobservable mismatch invariant 9 warns about.
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
    /// **Not optional, and not defaulted**: a default of 0 would put every pre-Session-H turn in
    /// one derived session. `DERIVATION_VERSION` was bumped instead.
    pub occurred_at_ms: i64,
    pub created_at: i64,
    pub silent_until: Option<i64>,
    pub fidelity: Fidelity,
    /// Kind, scope, speaker and source note.
    ///
    /// **Defaulted and omitted when empty, unlike `occurred_at_ms`**, because empty is a true
    /// value here rather than a wrong one: every eval write and every pre-M2 event is untagged.
    /// Untagged writes therefore serialize byte-for-byte as before.
    #[serde(default, skip_serializing_if = "Tags::is_empty")]
    pub tags: Tags,
}

impl MemoryWrittenPayload {
    /// The entry this event creates. The one constructor every write path and the replay share,
    /// so the live store and a rebuild cannot disagree about a field.
    pub fn to_entry(&self, seq: Seq) -> MemoryEntry {
        MemoryEntry {
            id: self.id.clone(),
            text: self.text.clone(),
            payload_kind: self.payload_kind,
            embedding_ref: None,
            source_turn_id: self.source_turn_id.clone(),
            source_session_id: self.source_session_id.clone(),
            trust_class: self.trust_class,
            effective_trust: self.effective_trust,
            derivation: self.derivation.clone(),
            origin_event: seq,
            occurred_at_ms: self.occurred_at_ms,
            created_at: self.created_at,
            last_accessed: self.created_at,
            access_count: 0,
            confidence: 1.0,
            activation: 1.0,
            // Explicit, from the event. Never defaulted — invariant 9.
            fidelity: self.fidelity,
            silent_until: self.silent_until,
            supersedes: Vec::new(),
            superseded_by: None,
            tags: self.tags.clone(),
        }
    }
}

/// The payload of a `MemoryWriteRejected` event.
///
/// A refusal is journaled, not merely returned: the poisoning suite's unsigned-write family
/// asserts a refusal was visible rather than inferred from absence.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryWriteRejectedPayload {
    pub turn_id: String,
    pub session_id: String,
    pub reason: String,
}

/// Beliefs, keyed by id, plus the lexical statistics derived from their text.
///
/// `BTreeMap`, not `HashMap`: iteration order reaches the wire through the injected set, and
/// hash order is randomised per process. A workspace test bans the type outright.
///
/// Every text change goes through [`Self::insert`] or [`Self::tombstone`], which is what keeps
/// [`Self::lexicon`] current.
#[derive(Debug, Default)]
pub struct BeliefStore {
    entries: BTreeMap<MemoryId, MemoryEntry>,
    lexicon: LexicalIndex,
}

impl BeliefStore {
    /// Rebuild the belief store by replaying the whole log.
    pub fn derive(journal: &Journal, derivation_version: u32) -> Result<Self, MemoryError> {
        check_version(derivation_version)?;
        let mut store = Self::default();
        store.apply_tail(journal, 0)?;
        Ok(store)
    }

    /// Rebuild from a snapshot plus the events after its watermark.
    ///
    /// Refuses (rather than guesses) when the snapshot is from another derivation, or claims a
    /// watermark the journal has not reached — a snapshot of some other profile, or of this one
    /// before a restore. The caller falls back to [`Self::derive`].
    ///
    /// The tail is read with a ranged, streaming replay, so boot cost follows the tail.
    pub fn from_snapshot(
        snapshot: BeliefSnapshot,
        journal: &Journal,
        derivation_version: u32,
    ) -> Result<Self, MemoryError> {
        check_version(derivation_version)?;
        if snapshot.format != SNAPSHOT_FORMAT {
            return Err(MemoryError::SnapshotRejected(format!(
                "format {} (this build reads {SNAPSHOT_FORMAT})",
                snapshot.format
            )));
        }
        if snapshot.derivation_version != DERIVATION_VERSION {
            return Err(MemoryError::SnapshotRejected(format!(
                "derived by version {}, this build derives {DERIVATION_VERSION}",
                snapshot.derivation_version
            )));
        }
        if snapshot.journal_seq > journal.last_seq() {
            return Err(MemoryError::SnapshotRejected(format!(
                "watermark {} is past the journal's last seq {}",
                snapshot.journal_seq,
                journal.last_seq()
            )));
        }
        let mut store = Self::default();
        for entry in snapshot.entries {
            store.insert(entry);
        }
        store.apply_tail(journal, snapshot.journal_seq)?;
        Ok(store)
    }

    /// Fold every event with `seq > after` into the store, one row at a time.
    fn apply_tail(&mut self, journal: &Journal, after: Seq) -> Result<(), MemoryError> {
        let cap = OperatorCapability::for_operator_or_audit();
        journal.replay_after(&cap, after, |seq, kind, payload| self.apply(seq, kind, payload))
    }

    /// One event of the fold. Payloads are decoded only for the kinds the store folds.
    fn apply(&mut self, seq: Seq, kind: EventKind, payload: &str) -> Result<(), MemoryError> {
        match kind {
            EventKind::MemoryWritten => {
                let p: MemoryWrittenPayload = decode(seq, payload)?;
                self.insert(p.to_entry(seq));
            }
            EventKind::FidelityDemoted => {
                let p: FidelityDemotedPayload = decode(seq, payload)?;
                self.known(seq, &p.id)?;
                if let Some(target) = self.entries.get_mut(&p.id) {
                    target.fidelity = p.to;
                }
            }
            EventKind::Superseded => {
                let p: SupersededPayload = decode(seq, payload)?;
                self.known(seq, &p.id)?;
                self.supersede(&p.id, &p.by);
            }
            EventKind::Tombstoned => {
                let p: TombstonedPayload = decode(seq, payload)?;
                self.known(seq, &p.id)?;
                self.tombstone(&p.id);
            }
            EventKind::Retracted => {
                let p: RetractedPayload = decode(seq, payload)?;
                self.retract(&p, seq);
            }
            // Not belief-store inputs. Ignored by kind rather than by a catch-all on shape, so a
            // new memory-affecting kind has to be handled here on purpose.
            _ => {}
        }
        Ok(())
    }

    fn known(&self, seq: Seq, id: &str) -> Result<(), MemoryError> {
        if self.entries.contains_key(id) {
            Ok(())
        } else {
            Err(MemoryError::EventForUnknownMemory { seq, id: id.to_string() })
        }
    }

    /// The current state as a snapshot covering every event up to `journal_seq`.
    ///
    /// The caller must pass the seq of the last event already applied to this store
    /// (normally `journal.last_seq()` on the thread that owns both).
    pub fn snapshot(&self, journal_seq: Seq) -> BeliefSnapshot {
        BeliefSnapshot {
            format: SNAPSHOT_FORMAT,
            derivation_version: DERIVATION_VERSION,
            journal_seq,
            journal_signature: None,
            entries: self.entries.values().cloned().collect(),
        }
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
    /// Every entry, in id order — live, superseded, unmatured and tombstoned alike.
    pub fn entries(&self) -> impl Iterator<Item = &MemoryEntry> {
        self.entries.values()
    }
    /// Cached per-entry term statistics for [`crate::cue::lexical::score_indexed`].
    pub fn lexicon(&self) -> &LexicalIndex {
        &self.lexicon
    }
    pub fn insert(&mut self, entry: MemoryEntry) {
        self.lexicon.upsert(&entry.id, &entry.text);
        self.entries.insert(entry.id.clone(), entry);
    }

    /// Fold a supersession edge into the live view: the loser's `superseded_by`, then the
    /// winner's `supersedes`. The replay path calls this same function.
    ///
    /// Silent on an unknown id, because the journal is the authority: replay raises
    /// `EventForUnknownMemory` where an unresolvable edge belongs.
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

    /// Apply a `Tombstoned` event: tier to `Tombstone` **and the text cleared**, on the live path
    /// and on replay alike (they share this function). Silent on an unknown id, as above.
    pub fn tombstone(&mut self, id: &str) {
        if let Some(e) = self.entries.get_mut(id) {
            e.fidelity = Fidelity::Tombstone;
            e.text.clear();
            self.lexicon.upsert(id, "");
        }
    }

    /// Apply a `Retracted` event (a `/rewind` dropped turns): every live entry the session wrote
    /// from the rewound turns is marked superseded by `retracted@<seq>`, which every retrieval
    /// path already excludes (injection, recall, profile, consolidation). The text stays: the
    /// journal is the audit spine and nothing was forgotten, only un-said. Returns how many
    /// entries were retracted. The live path and replay share this function.
    ///
    /// A belief a rewound turn had superseded (`correct`) is live again. A `forget` in a rewound
    /// turn cannot be undone: a tombstone's text was cleared.
    pub fn retract(&mut self, p: &RetractedPayload, seq: Seq) -> usize {
        let marker = format!("{RETRACTED_BY_PREFIX}{seq}");
        // Journal order when the rewound turn recorded where it began; the wall clock only for
        // sessions saved before that was recorded.
        let rewound = |e: &MemoryEntry| {
            e.source_session_id == p.session
                && match (p.after_seq, p.from_ms) {
                    (Some(after), _) => e.origin_event > after,
                    (None, Some(from)) => e.occurred_at_ms >= from,
                    (None, None) => false,
                }
        };
        let hit: Vec<String> = self
            .entries
            .values()
            .filter(|e| rewound(e) && e.superseded_by.is_none() && e.fidelity > Fidelity::Tombstone)
            .map(|e| e.id.clone())
            .collect();
        // A correction made inside the rewound turns had superseded an older belief: that belief
        // is the live one again, as it was before the turn. (Unless it was itself said in the
        // rewound turns.)
        let mut revived = Vec::new();
        for id in &hit {
            if let Some(e) = self.entries.get(id) {
                for loser in &e.supersedes {
                    if self.entries.get(loser).is_some_and(|l| !rewound(l)) {
                        revived.push((loser.clone(), id.clone()));
                    }
                }
            }
        }
        for id in &hit {
            if let Some(e) = self.entries.get_mut(id) {
                e.superseded_by = Some(marker.clone());
            }
        }
        for (loser, winner) in revived {
            if let Some(e) = self.entries.get_mut(&loser) {
                if e.superseded_by.as_deref() == Some(winner.as_str()) {
                    e.superseded_by = None;
                }
            }
        }
        hit.len()
    }

    /// Section 4.3 — the auto-injection candidate set, live-only.
    ///
    /// ADR-003 makes the live-only hot index a **requirement, not an optimization**. This filter
    /// *is* the partition, and it applies every exclusion in
    /// [`MemoryEntry::is_injection_candidate`] — the part that must not drift.
    pub fn injection_candidates(&self, now_ms: i64) -> Vec<&MemoryEntry> {
        self.entries
            .values()
            .filter(|e| e.is_injection_candidate(now_ms))
            .collect()
    }

    /// Section 4.3 — reachable by explicit `recall` only: hot ∪ cold.
    ///
    /// Tombstones and unmatured entries live here and **never** compete for injection precision.
    pub fn recall_candidates(&self) -> Vec<&MemoryEntry> {
        self.entries.values().collect()
    }
}

fn check_version(found: u32) -> Result<(), MemoryError> {
    if found != DERIVATION_VERSION {
        return Err(MemoryError::DerivationVersionMismatch { found, expected: DERIVATION_VERSION });
    }
    Ok(())
}

fn decode<T: serde::de::DeserializeOwned>(seq: Seq, payload: &str) -> Result<T, MemoryError> {
    serde_json::from_str(payload).map_err(|e| MemoryError::UndecodablePayload { seq, source: e })
}

/// Bumped when [`BeliefSnapshot`]'s shape changes. An old snapshot is then refused and boot
/// derives from the journal — a snapshot is a cache, never migrated.
pub const SNAPSHOT_FORMAT: u32 = 1;

const SNAPSHOT_MAGIC: &str = "marlowe-belief-snapshot";
const SNAPSHOT_KEY_CONTEXT: &str = "marlowe belief snapshot v1";

/// The belief store at a journal watermark.
///
/// Lives outside the signed journal, so on disk it is always [`sealed`](Self::seal) with a key
/// derived from the profile's signing key: a snapshot nobody but this profile could have written
/// is the only kind boot will trust.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BeliefSnapshot {
    pub format: u32,
    pub derivation_version: u32,
    /// Every event with `seq <= journal_seq` is reflected in `entries`.
    pub journal_seq: Seq,
    /// Signature of the journal event at `journal_seq`, when the writer recorded it: a
    /// checkpoint that lets boot verify only the chain after it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub journal_signature: Option<String>,
    pub entries: Vec<MemoryEntry>,
}

impl BeliefSnapshot {
    /// Serialize and authenticate: `magic\n<blake3 keyed mac hex>\n<json>`.
    pub fn seal(&self, key: &SigningKey) -> Result<Vec<u8>, MemoryError> {
        let body = serde_json::to_vec(self)?;
        let mac = blake3::keyed_hash(&snapshot_key(key), &body);
        let mut out = format!("{SNAPSHOT_MAGIC}\n{}\n", mac.to_hex()).into_bytes();
        out.extend_from_slice(&body);
        Ok(out)
    }

    /// Verify and decode a sealed snapshot. Any mismatch is a refusal, never a partial load.
    pub fn open(bytes: &[u8], key: &SigningKey) -> Result<Self, MemoryError> {
        let reject = |why: &str| MemoryError::SnapshotRejected(why.to_string());
        let mut parts = bytes.splitn(3, |b| *b == b'\n');
        let magic = parts.next().ok_or_else(|| reject("empty file"))?;
        if magic != SNAPSHOT_MAGIC.as_bytes() {
            return Err(reject("not a belief snapshot"));
        }
        let mac_hex = parts.next().ok_or_else(|| reject("truncated before the mac"))?;
        let body = parts.next().ok_or_else(|| reject("truncated before the body"))?;
        let mac_hex = std::str::from_utf8(mac_hex).map_err(|_| reject("mac is not text"))?;
        let claimed = blake3::Hash::from_hex(mac_hex).map_err(|_| reject("mac is not hex"))?;
        // blake3::Hash equality is constant-time.
        if blake3::keyed_hash(&snapshot_key(key), body) != claimed {
            return Err(reject("mac does not verify under this profile's key"));
        }
        Ok(serde_json::from_slice(body)?)
    }
}

fn snapshot_key(key: &SigningKey) -> [u8; 32] {
    // SigningKey exposes its bytes only as hex, deliberately; decode locally.
    let hex = key.to_hex();
    let mut bytes = [0u8; 32];
    for (i, pair) in hex.as_bytes().chunks(2).enumerate() {
        let pair = std::str::from_utf8(pair).unwrap_or("00");
        bytes[i] = u8::from_str_radix(pair, 16).unwrap_or(0);
    }
    blake3::derive_key(SNAPSHOT_KEY_CONTEXT, &bytes)
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

/// Journals a `Retracted` event for `p` and folds it into `beliefs` (journal first, as every
/// write is). Returns how many entries it retracted. The harness acts here, on the user's
/// `/rewind`; the model is never an actor.
pub fn append_retraction(
    journal: &mut Journal,
    beliefs: &mut BeliefStore,
    clock: marlowe_contract::Clock,
    p: &RetractedPayload,
) -> Result<usize, MemoryError> {
    use marlowe_journal::{Actor, AppendRequest, TraceId};
    let event = journal.append(
        clock,
        AppendRequest {
            trace_id: TraceId::new_v5(&TraceId::NAMESPACE_OID, p.session.as_bytes()),
            session_id: Some(p.session.clone()),
            run_id: None,
            actor: Actor::Harness,
            kind: EventKind::Retracted,
            payload: serde_json::to_value(p)?,
        },
    )?;
    Ok(beliefs.retract(p, event.seq))
}

/// `superseded_by` of an entry a `/rewind` retracted is this prefix plus the event's seq.
pub const RETRACTED_BY_PREFIX: &str = "retracted@";

/// The payload of a `Retracted` event: a session's turns from `turns[0]` on were rewound.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetractedPayload {
    pub session: String,
    /// The rewound turn indices, ascending.
    pub turns: Vec<u32>,
    /// The journal's last seq when the first rewound turn began: entries the session wrote after
    /// it are retracted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_seq: Option<Seq>,
    /// For a turn stored before `after_seq` was recorded: entries that occurred at or after this
    /// instant (Unix epoch ms).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_ms: Option<i64>,
}
