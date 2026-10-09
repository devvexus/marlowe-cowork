//! The memory service.
//!
//! Exclusively owns the journal Profile at `<home>\profile`; nothing else opens it. One actor
//! thread owns the Journal, BeliefStore and VectorStore. The Embedder and CrossEncoder load on
//! first need and unload after [`MindConfig::model_idle`]. A bounded, coalescing embed queue
//! embeds new beliefs in the background. Without models the service runs degraded (lexical
//! recall only, no auto-injection) and says so in [`Mind::stats`].
//!
//! Boot reads `<home>\profile\beliefs.snapshot` (written on shutdown, after a forget and every
//! [`MindConfig::snapshot_every`]), verifies and folds only the journal events after its
//! watermark, and loads belief vectors from `beliefs.vectors` so nothing is re-embedded.
//!
//! [`Mind`] is a cheap, cloneable, synchronous handle over the actor.

#![forbid(unsafe_code)]

mod actor;
mod error;
mod legacy;
mod models;
mod paths;
mod select;
mod store;
mod tags;
mod types;
mod vecfile;

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;

use crossbeam_channel::{bounded, unbounded, Sender};

use actor::{Actor, Cmd};

pub use error::MindError;
pub use marlowe_proto::{
    ImportReport, MemoryChange, MemoryHit as RecallHit, MemoryKind, MemoryScope as Scope,
    MemoryStatsReport as MindStats, MemoryTrust as Trust,
};
pub use paths::{
    default_legacy_profile, models_dir_candidates, profile_dir, resolve_models_dir, EMBEDDER_DIR,
    MODELS_ENV, RERANKER_DIR,
};
pub use types::{
    wire_trust, ConsolidateReport, Episode, ImportProgress, InjectQuery, Injection, Kind,
    LegacyStatus, MemoryIn, MindConfig, Provenance, RecallQuery, Remembered, SnapshotInfo,
    TurnIn, TurnRole,
};

/// Most tokens a single injection may use (the conductor's memory tier).
pub const MAX_INJECT_TOKENS: u32 = 7_000;

/// Provenance stamped on every belief imported from the old harness.
pub const LEGACY_PROVENANCE: &str = "imported from Marlowe_Harness";

/// Import ops sent to the actor per command, so turns interleave with a long import.
const IMPORT_CHUNK: usize = 100;

/// Handle to the memory service. Clones share one actor.
#[derive(Clone)]
pub struct Mind {
    inner: Arc<Inner>,
}

/// Shared handle state. Dropping the last handle stops the actor (after a final snapshot) and
/// joins it, so the profile lock is free when the drop returns.
struct Inner {
    home: PathBuf,
    cfg: MindConfig,
    tx: Mutex<Option<Sender<Cmd>>>,
    join: Mutex<Option<JoinHandle<()>>>,
    legacy_count: Mutex<Option<LegacyCount>>,
}

/// Cached `legacy_status` count, keyed by the old journal's size and mtime.
struct LegacyCount {
    path: PathBuf,
    fingerprint: Vec<(u64, Option<std::time::SystemTime>)>,
    entries: u64,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Drop for Inner {
    fn drop(&mut self) {
        lock(&self.tx).take();
        if let Some(handle) = lock(&self.join).take() {
            let _ = handle.join();
        }
    }
}

impl std::fmt::Debug for Mind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mind").field("home", &self.inner.home).finish_non_exhaustive()
    }
}

impl Mind {
    /// Opens (or creates) the Profile under `home`, boots from the latest snapshot plus the
    /// journal tail, and starts the actor. Models are not loaded yet.
    pub fn open(home: &Path) -> Result<Mind, MindError> {
        Mind::open_with(home, MindConfig::default())
    }

    /// [`Mind::open`] with explicit tunables.
    pub fn open_with(home: &Path, cfg: MindConfig) -> Result<Mind, MindError> {
        let (tx, rx) = unbounded();
        let (ready_tx, ready_rx) = bounded(1);
        let (actor_home, actor_cfg) = (home.to_path_buf(), cfg.clone());
        let join = std::thread::Builder::new().name("marlowe-mind".into()).spawn(move || {
            match Actor::open(&actor_home, actor_cfg) {
                Ok(actor) => {
                    let _ = ready_tx.send(Ok(()));
                    actor.run(rx);
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                }
            }
        })?;
        let started = ready_rx.recv().unwrap_or(Err(MindError::Closed));
        if let Err(e) = started {
            let _ = join.join();
            return Err(e);
        }
        Ok(Mind {
            inner: Arc::new(Inner {
                home: home.to_path_buf(),
                cfg,
                tx: Mutex::new(Some(tx)),
                join: Mutex::new(Some(join)),
                legacy_count: Mutex::new(None),
            }),
        })
    }

    /// The cowork home this service was opened on.
    pub fn home(&self) -> &Path {
        &self.inner.home
    }

    /// The tunables it runs with.
    pub fn config(&self) -> &MindConfig {
        &self.inner.cfg
    }

    fn call<T>(&self, make: impl FnOnce(Sender<T>) -> Cmd) -> Result<T, MindError> {
        let tx = lock(&self.inner.tx).clone().ok_or(MindError::Closed)?;
        let (reply_tx, reply_rx) = bounded(1);
        tx.send(make(reply_tx)).map_err(|_| MindError::Closed)?;
        reply_rx.recv().map_err(|_| MindError::Closed)
    }

    /// Ingests one turn. User turns: user trust, no maturation. Assistant turns: Episode kind.
    pub fn ingest_turn(&self, turn: TurnIn) -> Result<(), MindError> {
        self.call(|r| Cmd::Ingest(turn, r))?
    }

    /// Writes one explicit memory. Trust comes from `memory.source`.
    pub fn remember(&self, memory: MemoryIn) -> Result<Remembered, MindError> {
        self.call(|r| Cmd::Remember(memory, r))?
    }

    /// Selects memories for a turn: Profile scope, dense cue, cross-encoder rerank, gate and
    /// declared operating point. Never fails the turn: errors become `abstained`.
    pub fn inject(&self, query: InjectQuery) -> Injection {
        self.call(|r| Cmd::Inject(query, r))
            .unwrap_or_else(|e| Injection { abstained: Some(e.to_string()), ..Injection::default() })
    }

    /// Explicit search. Works in degraded mode (lexical only); sees unmatured beliefs too.
    pub fn recall(&self, query: RecallQuery) -> Result<Vec<RecallHit>, MindError> {
        self.call(|r| Cmd::Recall(query, r))?
    }

    /// Supersedes `id` with `new_text`. Refused if the correction carries less trust than `id`.
    pub fn correct(&self, id: &str, new_text: &str, source: Provenance) -> Result<Remembered, MindError> {
        self.call(|r| Cmd::Correct(id.to_string(), new_text.to_string(), source, r))?
    }

    /// Tombstones `id`. The event stays in the journal; the text leaves the store. Refused
    /// when `source` carries less trust than the memory (the same rule as [`Mind::correct`]).
    pub fn forget(&self, id: &str, source: Provenance) -> Result<(), MindError> {
        self.call(|r| Cmd::Forget(id.to_string(), source, r))?
    }

    /// One memory by id (live or superseded); `None` if unknown or forgotten.
    pub fn get(&self, id: &str) -> Result<Option<RecallHit>, MindError> {
        self.call(|r| Cmd::Get(id.to_string(), r))
    }

    /// Episodes whose `at_ms` falls in `range`, oldest first.
    pub fn episodes(&self, range: Range<i64>) -> Result<Vec<Episode>, MindError> {
        self.call(|r| Cmd::Episodes(range, r))?
    }

    /// Writes a session summary as an Episode memory; returns its id. `tainted`: the session
    /// read untrusted content, so the episode is untrusted too.
    pub fn write_episode(&self, session: &str, summary: &str, at_ms: i64, tainted: bool) -> Result<String, MindError> {
        self.call(|r| Cmd::WriteEpisode(session.to_string(), summary.to_string(), at_ms, tainted, r))?
    }

    /// Counts, resident models, degraded reason, journal seq.
    pub fn stats(&self) -> Result<MindStats, MindError> {
        self.call(Cmd::Stats)?
    }

    /// Runs `marlowe_memory::consolidate` over every session now (scheduling is M5).
    pub fn consolidate_now(&self) -> Result<ConsolidateReport, MindError> {
        self.call(Cmd::Consolidate)?
    }

    /// Writes a BeliefStore snapshot with its journal-seq watermark.
    pub fn snapshot(&self) -> Result<SnapshotInfo, MindError> {
        self.call(Cmd::Snapshot)?
    }

    /// Whether an old profile exists at `dir` (default: [`default_legacy_profile`]) and was
    /// already imported. `None` when there is none. Reads a temp copy of the old journal.
    pub fn legacy_status(&self, dir: Option<&Path>) -> Result<Option<LegacyStatus>, MindError> {
        let path = match dir.map(Path::to_path_buf).or_else(default_legacy_profile) {
            Some(p) if p.join("journal.db").is_file() => p,
            _ => return Ok(None),
        };
        let fingerprint = legacy::fingerprint(&path);
        let cached = lock(&self.inner.legacy_count)
            .as_ref()
            .filter(|c| c.path == path && c.fingerprint == fingerprint)
            .map(|c| c.entries);
        let entries = match cached {
            Some(n) => n,
            None => {
                let n = legacy::count(&path)?;
                *lock(&self.inner.legacy_count) = Some(LegacyCount { path: path.clone(), fingerprint, entries: n });
                n
            }
        };
        let already_imported = profile_dir(&self.inner.home).join(legacy::MARKER_FILE).is_file();
        Ok(Some(LegacyStatus { path, approx_entries: entries, already_imported }))
    }

    /// Imports the old harness profile. Copies it to a temp dir, verifies the chain with the old
    /// key, replays MemoryWritten/Superseded/Tombstoned as new beliefs carrying
    /// [`LEGACY_PROVENANCE`], the original `occurred_at` and trust class. Idempotent by content
    /// digest. Never writes to `old_profile_dir`. Memories forgotten in the old profile are not
    /// copied; `tombstoned` counts them.
    pub fn import_legacy(
        &self,
        old_profile_dir: &Path,
        on_progress: &mut dyn FnMut(ImportProgress),
    ) -> Result<ImportReport, MindError> {
        let plan = legacy::plan(old_profile_dir)?;
        let mut report = plan.report;
        let total = plan.ops.len().max(1) as u64;
        let mut done = 0u64;
        let mut ops = plan.ops.into_iter();
        loop {
            let chunk: Vec<_> = ops.by_ref().take(IMPORT_CHUNK).collect();
            if chunk.is_empty() {
                break;
            }
            done += chunk.len() as u64;
            let r = self.call(|reply| Cmd::ImportChunk(chunk, reply))??;
            report.imported += r.imported;
            report.skipped_duplicates += r.skipped_duplicates;
            report.superseded += r.superseded;
            on_progress(ImportProgress { read: report.read * done / total, imported: report.imported });
        }
        on_progress(ImportProgress { read: report.read, imported: report.imported });
        let marker = legacy::Marker {
            source: old_profile_dir.display().to_string(),
            completed_at_ms: actor::now_ms(),
            report: report.clone(),
        };
        std::fs::write(profile_dir(&self.inner.home).join(legacy::MARKER_FILE), serde_json::to_vec_pretty(&marker)?)?;
        Ok(report)
    }

    /// Writes a final snapshot and stops the actor. Later calls return [`MindError::Closed`].
    pub fn shutdown(&self) -> Result<(), MindError> {
        let result = self.call(Cmd::Shutdown);
        lock(&self.inner.tx).take();
        if let Some(handle) = lock(&self.inner.join).take() {
            let _ = handle.join();
        }
        result?
    }

    #[cfg(test)]
    fn boot_info(&self) -> Result<store::Boot, MindError> {
        self.call(Cmd::BootInfo)
    }
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_service;
