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
mod capture;
pub mod download;
mod episode;
mod error;
mod legacy;
mod live;
mod models;
mod paths;
mod profile;
mod select;
mod store;
mod tags;
mod types;
mod vecfile;
mod workspaces;

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;

use crossbeam_channel::{bounded, unbounded, Sender};

use actor::{Actor, Cmd};

pub use error::MindError;
pub use marlowe_proto::{
    ImportReport, MemoryChange, MemoryHit as RecallHit, MemoryKind, MemoryScope as Scope,
    MemoryStatsReport as MindStats, MemoryTrust as Trust, ModelsState,
};
pub use paths::{
    default_legacy_profile, has_models, install_dir, install_models, models_dir_candidates, profile_dir,
    remember_models_dir, resolve_models_dir, EMBEDDER_DIR, MODELS_ENV, MODELS_POINTER, RERANKER_DIR,
};
pub use profile::{Profile, ProfileQuery, DEFAULT_PROFILE_TOKENS};
pub use actor::SINK_GRACE;
pub use live::{LiveEvent, LiveKind, LiveSink, MindEvent, MindSink, PROGRESS_EVERY};
pub use marlowe_memory::ep::{judge_gpu, GpuLoad, GpuVerdict};
pub use types::{
    wire_trust, ConsolidateReport, Episode, GpuProbe, ImportProgress, InjectQuery, Injection, Kind,
    LegacyStatus, MemoryIn, MindConfig, Provenance, RecallQuery, Remembered, Retraction, SnapshotInfo,
    TurnIn, TurnRole,
};

/// Most tokens a single injection may use (the conductor's memory tier).
pub const MAX_INJECT_TOKENS: u32 = 7_000;

/// Provenance stamped on every belief imported from the old harness.
pub const LEGACY_PROVENANCE: &str = "imported from Marlowe_Harness";

/// Provenance stamped on every conversation turn imported from the old harness.
pub const LEGACY_CONVERSATION_PROVENANCE: &str = "imported from Marlowe_Harness (conversation)";

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

    /// Ingests one turn. User turns: user trust, no maturation; a statement is a Note (an
    /// injection candidate), a question or request an Episode (recall only). Assistant turns:
    /// Episode kind, never injected. Turns are Global; the workspace is kept per session and
    /// boosts that project's memories at injection.
    pub fn ingest_turn(&self, turn: TurnIn) -> Result<(), MindError> {
        self.call(|r| Cmd::Ingest(turn, r))?
    }

    /// Writes one explicit memory. Trust comes from `memory.source`.
    pub fn remember(&self, memory: MemoryIn) -> Result<Remembered, MindError> {
        self.call(|r| Cmd::Remember(memory, Vec::new(), r))?
    }

    /// [`Mind::remember`] for a memory derived from others (a research finding resting on
    /// recalled memories): their ids become its lineage, and its trust is the worst over them.
    /// Ids no longer in the store are dropped.
    pub fn remember_derived(&self, memory: MemoryIn, derived_from: Vec<String>) -> Result<Remembered, MindError> {
        self.call(|r| Cmd::Remember(memory, derived_from, r))?
    }

    /// Selects memories for a turn: Profile scope, dense cue, cross-encoder rerank, gate and
    /// declared operating point. Never fails the turn: errors become `abstained`.
    pub fn inject(&self, query: InjectQuery) -> Injection {
        self.call(|r| Cmd::Inject(query, None, r))
            .unwrap_or_else(|e| Injection { abstained: Some(e.to_string()), ..Injection::default() })
    }

    /// [`Mind::inject`], streaming each recall stage to `live` the moment it starts, advances and
    /// ends. Selects exactly what `inject` would; the sink only observes.
    pub fn inject_live(&self, query: InjectQuery, live: LiveSink) -> Injection {
        self.call(|r| Cmd::Inject(query, Some(live), r))
            .unwrap_or_else(|e| Injection { abstained: Some(e.to_string()), ..Injection::default() })
    }

    /// Sets who hears the write tail (ingest, embed, index) from now on.
    pub fn set_event_sink(&self, sink: MindSink) {
        let _ = self.call(|r| Cmd::SetSink(sink, r));
    }

    /// What was deliberately recorded about the user (Preference, Decision, Fact, Procedure from
    /// remember, corrections or the harness; never turns, imports, episodes or untrusted
    /// writes), rendered one line each for the context tier. Deterministic; needs no models.
    pub fn profile(&self, query: ProfileQuery) -> Result<Profile, MindError> {
        self.call(|r| Cmd::Profile(query, r))
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

    /// Writes the session's episode from its ingested turns, with no model: the opening request,
    /// when and where it ran, its most salient lines and keywords (see `episode`). Supersedes the
    /// session's earlier episode, so a session evicted twice keeps one. `Ok(None)` when the
    /// session has no ingested turns. `tainted`: the session read untrusted content.
    pub fn write_session_episode(&self, session: &str, tainted: bool) -> Result<Option<String>, MindError> {
        self.call(|r| Cmd::WriteSessionEpisode(session.to_string(), tainted, r))?
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

    /// Looks for the memory models again (after they were installed) and loads them on first
    /// need, so recall leaves keyword-only mode without a restart.
    pub fn reload_models(&self) -> Result<(), MindError> {
        self.call(Cmd::ReloadModels)
    }

    /// Writes a BeliefStore snapshot with its journal-seq watermark.
    pub fn snapshot(&self) -> Result<SnapshotInfo, MindError> {
        self.call(Cmd::Snapshot)?
    }

    /// Retracts what `retraction.session` recorded from its rewound turns (`/rewind`): the beliefs
    /// stay in the journal, behind a `Retracted` event, and are never injected, recalled, listed
    /// in the profile or consolidated again. Returns how many beliefs it retracted.
    pub fn retract(&self, retraction: Retraction) -> Result<usize, MindError> {
        self.call(|r| Cmd::Retract(retraction, r))?
    }

    /// The journal's last seq: a turn records it when it begins, so a rewind can tell what the
    /// turn went on to write ([`Retraction::after_seq`]).
    pub fn journal_seq(&self) -> Result<u64, MindError> {
        self.call(Cmd::JournalSeq)
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
        let already_imported = legacy::marker_is_current(&profile_dir(&self.inner.home));
        Ok(Some(LegacyStatus { path, approx_entries: entries, already_imported }))
    }

    /// Imports the old harness profile. Copies it to a temp dir, verifies the chain with the old
    /// key, replays MemoryWritten/Superseded/Tombstoned as new beliefs carrying
    /// [`LEGACY_PROVENANCE`], the original `occurred_at` and trust class. Idempotent by content
    /// digest. Never writes to `old_profile_dir`. Memories forgotten in the old profile are not
    /// copied; `tombstoned` counts them.
    ///
    /// Also imports the old conversation from its checkpoints (see `legacy`), carrying
    /// [`LEGACY_CONVERSATION_PROVENANCE`]: user turns at user trust (statements as Notes,
    /// requests as Episodes), assistant prose as Episodes. Machine payloads are skipped.
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
            report.conversation_user_turns += r.user_turns;
            report.conversation_user_requests += r.user_requests;
            report.conversation_assistant_turns += r.assistant_turns;
            on_progress(ImportProgress { read: report.read * done / total, imported: report.imported });
        }
        report.conversation_turns_imported = report.conversation_user_turns + report.conversation_assistant_turns;
        on_progress(ImportProgress { read: report.read, imported: report.imported });
        let marker = legacy::Marker {
            version: legacy::MARKER_VERSION,
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
mod tests_live;
#[cfg(test)]
mod tests_profile;
#[cfg(test)]
mod tests_service;
