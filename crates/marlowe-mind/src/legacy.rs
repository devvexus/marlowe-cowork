//! Reading the old harness profile for import. Never writes to it.
//!
//! The old profile is copied to a temp dir first (it may hold a lock and WAL files), its chain is
//! verified under its own key, and its memory events are turned into an ordered list of ops that
//! the actor applies in chunks. Ids are content digests, so a second import is a no-op.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use marlowe_contract::{Fidelity, PayloadKind, TrustClass};
use marlowe_journal::{EventKind, Journal, JournalError, OperatorCapability, Profile};
use marlowe_memory::memory_id;
use marlowe_memory::store::{FidelityDemotedPayload, MemoryWrittenPayload, SupersededPayload, TombstonedPayload};
use marlowe_memory::BeliefKind;
use serde::{Deserialize, Serialize};

use crate::error::MindError;
use crate::ImportReport;

/// Session every imported belief is filed under; ids are `m-legacy-<digest>-0`.
pub(crate) const SESSION: &str = "legacy";

/// Files copied out of the old profile.
const COPIED: [&str; 5] = ["journal.db", "journal.db-wal", "journal.db-shm", "profile.key", "profile.json"];
/// Marker written into the new profile when an import completes.
pub(crate) const MARKER_FILE: &str = "legacy-import.json";
/// Most errors listed in a report.
const MAX_ERRORS: usize = 50;

/// One old memory to write.
#[derive(Debug, Clone)]
pub(crate) struct LegacyWrite {
    /// Content digest; the new id is derived from it.
    pub digest: String,
    pub text: String,
    pub kind: BeliefKind,
    pub trust_class: TrustClass,
    pub effective_trust: TrustClass,
    /// New ids of the old parents.
    pub derived_from: Vec<String>,
    pub occurred_at_ms: i64,
}

/// One step of an import, applied by the actor.
#[derive(Debug, Clone)]
pub(crate) enum ImportOp {
    /// Skipped when a belief with this digest id already exists.
    Write(Box<LegacyWrite>),
    Supersede { loser: String, winner: String },
    /// Forgotten in the old profile after an earlier import brought it over.
    Tombstone { id: String },
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ChunkResult {
    pub imported: u64,
    pub skipped_duplicates: u64,
    pub superseded: u64,
}

pub(crate) struct Plan {
    pub ops: Vec<ImportOp>,
    /// The report's planner-side counts: `read`, `tombstoned`, `errors`.
    pub report: ImportReport,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Marker {
    pub source: String,
    pub completed_at_ms: i64,
    pub report: ImportReport,
}

/// A throwaway copy of the old profile, removed on drop.
struct TempCopy(PathBuf);

impl TempCopy {
    fn new(old: &Path) -> Result<Self, MindError> {
        if !old.join("journal.db").is_file() {
            return Err(MindError::NoLegacyProfile(old.to_path_buf()));
        }
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("marlowe-legacy-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&dir)?;
        let copy = TempCopy(dir);
        for name in COPIED {
            let src = old.join(name);
            if src.is_file() {
                fs::copy(&src, copy.0.join(name))?;
            } else if matches!(name, "profile.key" | "profile.json") {
                return Err(MindError::LegacyChain(format!("{} is missing", src.display())));
            }
        }
        Ok(copy)
    }
}

impl Drop for TempCopy {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Copies, opens and verifies the old journal; returns its events.
fn read_events(old: &Path) -> Result<Vec<(u64, EventKind, serde_json::Value)>, MindError> {
    let copy = TempCopy::new(old)?;
    let events = {
        let profile = Profile::open(&copy.0).map_err(|e| MindError::LegacyChain(e.to_string()))?;
        let journal = Journal::open(&profile).map_err(|e| match e {
            JournalError::ChainBroken { .. } | JournalError::SequenceGap { .. } => MindError::LegacyChain(e.to_string()),
            other => MindError::Journal(other),
        })?;
        journal.replay(&OperatorCapability::for_operator_or_audit(), None)?
    };
    drop(copy);
    Ok(events)
}

/// Approximate number of memories in the old profile (its `MemoryWritten` events).
pub(crate) fn count(old: &Path) -> Result<u64, MindError> {
    Ok(read_events(old)?.iter().filter(|(_, k, _)| *k == EventKind::MemoryWritten).count() as u64)
}

/// A file's size and mtime, to tell whether a cached count is stale.
pub(crate) fn fingerprint(old: &Path) -> Vec<(u64, Option<std::time::SystemTime>)> {
    ["journal.db", "journal.db-wal"]
        .iter()
        .map(|n| match fs::metadata(old.join(n)) {
            Ok(m) => (m.len(), m.modified().ok()),
            Err(_) => (0, None),
        })
        .collect()
}

/// Content digest of an old memory: its old id and text.
pub(crate) fn digest(old_id: &str, text: &str) -> String {
    let mut h = blake3::Hasher::new();
    h.update(b"marlowe-legacy-v1\0");
    h.update(old_id.as_bytes());
    h.update(b"\0");
    h.update(text.as_bytes());
    h.finalize().to_hex()[..24].to_string()
}

/// Old conversation turns were stored as episodes; here they are notes, so they read as what
/// was said rather than as session summaries.
fn imported_kind(kind: PayloadKind) -> BeliefKind {
    match kind {
        PayloadKind::Episode => BeliefKind::Note,
        other => BeliefKind::from_payload_kind(other),
    }
}

pub(crate) fn plan(old: &Path) -> Result<Plan, MindError> {
    let events = read_events(old)?;
    let mut report = ImportReport::default();
    let mut errors = Vec::new();
    let mut written: Vec<MemoryWrittenPayload> = Vec::new();
    let mut supersedes: Vec<(String, String)> = Vec::new();
    let mut tombstoned: BTreeSet<String> = BTreeSet::new();
    let mut demoted: BTreeMap<String, Fidelity> = BTreeMap::new();

    for (seq, kind, payload) in events {
        let decoded = match kind {
            EventKind::MemoryWritten => serde_json::from_value::<MemoryWrittenPayload>(payload).map(|p| written.push(p)),
            EventKind::Superseded => {
                serde_json::from_value::<SupersededPayload>(payload).map(|p| supersedes.push((p.id, p.by)))
            }
            EventKind::Tombstoned => {
                serde_json::from_value::<TombstonedPayload>(payload).map(|p| {
                    tombstoned.insert(p.id);
                })
            }
            EventKind::FidelityDemoted => serde_json::from_value::<FidelityDemotedPayload>(payload).map(|p| {
                if p.to == Fidelity::Tombstone {
                    tombstoned.insert(p.id);
                } else {
                    demoted.insert(p.id, p.to);
                }
            }),
            _ => continue,
        };
        report.read += 1;
        if let Err(e) = decoded {
            errors.push(format!("seq {seq}: {e}"));
        }
    }

    let digests: BTreeMap<String, String> = written.iter().map(|p| (p.id.clone(), digest(&p.id, &p.text))).collect();
    let ids: BTreeMap<String, String> =
        digests.iter().map(|(old, d)| (old.clone(), memory_id(SESSION, d, 0))).collect();
    let mut ops = Vec::new();
    for p in written {
        if tombstoned.contains(&p.id) {
            // Forgotten there: its text is never copied here.
            report.tombstoned += 1;
            ops.push(ImportOp::Tombstone { id: ids[&p.id].clone() });
            continue;
        }
        if demoted.contains_key(&p.id) {
            errors.push(format!("{} was demoted in the old profile; imported at full fidelity", p.id));
        }
        let derived_from = p.derivation.iter().filter_map(|d| ids.get(d).cloned()).collect();
        ops.push(ImportOp::Write(Box::new(LegacyWrite {
            digest: digests[&p.id].clone(),
            text: p.text,
            kind: p.tags.kind.unwrap_or_else(|| imported_kind(p.payload_kind)),
            trust_class: p.trust_class,
            effective_trust: p.effective_trust,
            derived_from,
            occurred_at_ms: p.occurred_at_ms,
        })));
    }
    for (loser, winner) in supersedes {
        if tombstoned.contains(&loser) {
            continue;
        }
        match (ids.get(&loser), ids.get(&winner)) {
            (Some(l), Some(w)) => ops.push(ImportOp::Supersede { loser: l.clone(), winner: w.clone() }),
            _ => errors.push(format!("supersession {loser} -> {winner} names a memory the old journal never wrote")),
        }
    }
    if errors.len() > MAX_ERRORS {
        let extra = errors.len() - MAX_ERRORS;
        errors.truncate(MAX_ERRORS);
        errors.push(format!("... and {extra} more"));
    }
    report.errors = errors;
    Ok(Plan { ops, report })
}
