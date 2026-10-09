//! BeliefStore snapshots on disk and boot.
//!
//! The snapshot is the core's sealed `BeliefSnapshot` (keyed MAC under the profile key). Boot
//! loads it and folds only the events after its watermark; anything off (missing, tampered,
//! another profile, another derivation) falls back to a full derive, loudly.
//! The snapshot also carries the journal signature at its watermark, so the chain is verified
//! from there on rather than from genesis.

use std::fs;
use std::path::{Path, PathBuf};

use marlowe_journal::{Journal, JournalError, Profile, SigningKey};
use marlowe_memory::{BeliefSnapshot, BeliefStore};

use crate::error::MindError;

/// Snapshot file inside the profile dir.
pub(crate) const SNAPSHOT_FILE: &str = "beliefs.snapshot";

/// Seals `store` at `journal_seq`, with the signature of that event as the boot checkpoint,
/// and writes it atomically (temp file + rename).
pub(crate) fn write_snapshot(
    profile_dir: &Path,
    key: &SigningKey,
    journal_seq: u64,
    journal_signature: &str,
    store: &BeliefStore,
) -> Result<PathBuf, MindError> {
    let mut snap = store.snapshot(journal_seq);
    snap.journal_signature = Some(journal_signature.to_string());
    let bytes = snap.seal(key)?;
    let path = profile_dir.join(SNAPSHOT_FILE);
    let tmp = profile_dir.join(format!("{SNAPSHOT_FILE}.tmp"));
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, &path)?;
    Ok(path)
}

/// How the store was booted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Boot {
    /// From a snapshot at `watermark` plus the `tail` journal events after it.
    Snapshot { watermark: u64, tail: u64 },
    /// Full `BeliefStore::derive`.
    Derived,
}

/// The sealed snapshot, if there is one this profile wrote. Anything else is logged and ignored.
pub(crate) fn read_snapshot(profile_dir: &Path, key: &SigningKey) -> Option<BeliefSnapshot> {
    let path = profile_dir.join(SNAPSHOT_FILE);
    let bytes = fs::read(&path).ok()?;
    match BeliefSnapshot::open(&bytes, key) {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("marlowe-mind: ignoring snapshot {}: {e}", path.display());
            None
        }
    }
}

/// Opens the journal, verifying only the chain after the snapshot's checkpoint when it has one
/// that matches; otherwise (or on a mismatch) verifies the whole chain.
pub(crate) fn open_journal(profile: &Profile, snapshot: Option<&BeliefSnapshot>) -> Result<Journal, MindError> {
    if let Some((seq, sig)) = snapshot.and_then(|s| s.journal_signature.as_deref().map(|sig| (s.journal_seq, sig))) {
        match Journal::open_from_checkpoint(profile, seq, sig) {
            Ok(j) => return Ok(j),
            Err(e @ JournalError::CheckpointMismatch { .. }) => eprintln!("marlowe-mind: {e}"),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(Journal::open(profile)?)
}

/// Snapshot plus tail when a valid snapshot exists, else a full derive.
#[cfg(test)]
pub(crate) fn boot(
    profile_dir: &Path,
    key: &SigningKey,
    journal: &Journal,
    derivation_version: u32,
) -> Result<(BeliefStore, Boot), MindError> {
    boot_with(read_snapshot(profile_dir, key), journal, derivation_version)
}

/// [`boot`] from an already-read snapshot.
pub(crate) fn boot_with(
    snapshot: Option<BeliefSnapshot>,
    journal: &Journal,
    derivation_version: u32,
) -> Result<(BeliefStore, Boot), MindError> {
    if let Some(snap) = snapshot {
        let watermark = snap.journal_seq;
        match BeliefStore::from_snapshot(snap, journal, derivation_version) {
            Ok(store) => {
                let tail = journal.last_seq() - watermark;
                return Ok((store, Boot::Snapshot { watermark, tail }));
            }
            Err(e) => eprintln!("marlowe-mind: ignoring snapshot at seq {watermark}: {e}"),
        }
    }
    Ok((BeliefStore::derive(journal, derivation_version)?, Boot::Derived))
}
