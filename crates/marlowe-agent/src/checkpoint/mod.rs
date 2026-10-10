//! The loop side of checkpoints (track F2, design 3.4): a shadow git repository under
//! `<home>\checkpoints` snapshots the workspace before the first change of every turn, and
//! `/rewind` puts it back, byte for byte, without ever touching the user's own repository.
//!
//! - [`ShadowRepo`] is the engine (snapshots, rewind, diff, fork, prune); [`CheckpointStore`]
//!   hands out one per workspace.
//!   A turn that took a snapshot is closed with [`ShadowRepo::end_turn`], so a rewind can tell the
//!   session's own changes from edits made outside its turns and leave those alone.
//! - [`CheckpointMiddleware`] is the `pre_mutation` hook: the first call that will change a file
//!   in a turn takes the `pre` snapshot, and a file the tree cannot hold (ignored, or outside the
//!   workspace) gets a saved pre-image. A failing snapshot never blocks the edit; it warns once
//!   per session instead.
//!
//! The daemon wires these in (`marlowe-daemon/src/checkpoint`), together with the per-call turn
//! rows that make a killed session resumable.

mod git;
mod paths;
mod preimage;
mod shadow;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use marlowe_model::ToolCall;
use marlowe_proto::SessionId;

pub use preimage::{PreimageReport, MAX_PREIMAGE_BYTES};
pub use shadow::{
    RestoreReport, ShadowRepo, TurnCheckpoint, TurnState, MAX_DIFF_BYTES, MAX_SNAPSHOT_BYTES, MAX_SNAPSHOT_FILES,
    MAX_SNAPSHOT_FILE_BYTES, SIZE_WARN_KIB,
};

use crate::middleware::ToolMiddleware;
use crate::tools::{CallCtx, Mutation, ToolOutcome};

/// Checkpoints older than this are pruned.
pub const KEEP_DAYS: i64 = 30;

#[derive(Debug, thiserror::Error)]
pub enum CheckpointError {
    #[error("git {args} failed: {message}")]
    Git { args: String, message: String },
    #[error("could not run git: {0}")]
    Spawn(String),
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("checkpoint record: {0}")]
    Json(#[from] serde_json::Error),
    #[error("workspace: {0}")]
    Workspace(String),
    #[error("{0} has too many files to checkpoint; open Marlowe in a project directory")]
    TooLarge(String),
    #[error("interrupted")]
    Cancelled,
}

/// Shadow repositories under one root, one per workspace.
pub struct CheckpointStore {
    root: PathBuf,
    repos: Mutex<HashMap<PathBuf, Arc<ShadowRepo>>>,
}

impl CheckpointStore {
    /// `root` is `<home>\checkpoints`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into(), repos: Mutex::new(HashMap::new()) }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn cached(&self, workspace: &Path) -> Option<Arc<ShadowRepo>> {
        let key = paths::canonical(workspace);
        self.repos.lock().unwrap_or_else(|p| p.into_inner()).get(&key).cloned()
    }

    /// The shadow repository for `workspace`, created on first use.
    pub fn repo(&self, workspace: &Path) -> Result<Arc<ShadowRepo>, CheckpointError> {
        if let Some(r) = self.cached(workspace) {
            return Ok(r);
        }
        // Held across the open so two sessions starting together initialise one repository.
        let mut map = self.repos.lock().unwrap_or_else(|p| p.into_inner());
        let key = paths::canonical(workspace);
        if let Some(r) = map.get(&key) {
            return Ok(r.clone());
        }
        let repo = Arc::new(ShadowRepo::open(&self.root, workspace)?);
        map.insert(key, repo.clone());
        Ok(repo)
    }

    /// The repository for `workspace` if checkpoints were ever taken there.
    pub fn existing(&self, workspace: &Path) -> Result<Option<Arc<ShadowRepo>>, CheckpointError> {
        if let Some(r) = self.cached(workspace) {
            return Ok(Some(r));
        }
        match ShadowRepo::open_existing(&self.root, workspace)? {
            Some(r) => Ok(Some(self.repo_from(workspace, r))),
            None => Ok(None),
        }
    }

    fn repo_from(&self, workspace: &Path, repo: ShadowRepo) -> Arc<ShadowRepo> {
        let mut map = self.repos.lock().unwrap_or_else(|p| p.into_inner());
        map.entry(paths::canonical(workspace)).or_insert_with(|| Arc::new(repo)).clone()
    }

    /// Prunes every workspace's repository: sessions idle for more than [`KEEP_DAYS`] lose their
    /// checkpoints. Returns how many sessions were pruned (an emptied repository of a vanished workspace counts one). Slow (git gc): run it on a thread.
    pub fn prune_all(&self) -> usize {
        self.prune_all_older_than(KEEP_DAYS * 24 * 3600 * 1000)
    }

    fn prune_all_older_than(&self, max_age_ms: i64) -> usize {
        let Ok(rd) = std::fs::read_dir(&self.root) else { return 0 };
        let mut total = 0;
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().is_none_or(|x| x != "git") {
                continue;
            }
            let Some(ws) = std::fs::read_to_string(p.join("workspace.txt")).ok().map(PathBuf::from) else { continue };
            if !ws.is_dir() {
                // The project was deleted, moved or renamed: its checkpoints age out all the
                // same, and the repository goes with the last of them.
                if let Some(repo) = ShadowRepo::orphan(&self.root, p.clone(), ws) {
                    total += repo.prune_orphan(max_age_ms).map(usize::from).unwrap_or(0);
                }
            } else if let Ok(Some(repo)) = self.existing(&ws) {
                total += repo.prune(max_age_ms).map(|v| v.len()).unwrap_or(0);
            }
        }
        total
    }
}

/// Told when a checkpoint could not be taken or the repository is large.
pub type Warn = Arc<dyn Fn(&SessionId, String) + Send + Sync>;

/// `pre_mutation`: snapshots the workspace before a turn's first change and saves pre-images of
/// files the tree cannot hold. Cheap after the first call of a turn (a set lookup, plus an ignore
/// check for new paths).
pub struct CheckpointMiddleware {
    store: Arc<CheckpointStore>,
    warn: Warn,
    warned: Mutex<HashSet<String>>,
}

impl CheckpointMiddleware {
    pub fn new(store: Arc<CheckpointStore>, warn: Warn) -> Self {
        Self { store, warn, warned: Mutex::new(HashSet::new()) }
    }

    fn warn_once(&self, session: &SessionId, key: &str, message: String) {
        let first = self.warned.lock().unwrap_or_else(|p| p.into_inner()).insert(format!("{}:{key}", session.as_str()));
        if first {
            (self.warn)(session, message);
        }
    }
}

impl ToolMiddleware for CheckpointMiddleware {
    fn name(&self) -> &str {
        "checkpoints"
    }

    fn pre_mutation(&self, _call: &ToolCall, cx: &CallCtx, mutation: &Mutation) -> Result<(), ToolOutcome> {
        let Some(workspace) = &cx.workspace else { return Ok(()) };
        let result = self.store.repo(workspace).and_then(|repo| {
            let state = repo.begin_turn_cancellable(&cx.session, cx.turn, mutation, Some(&cx.cancel))?;
            if state.snapshotted && repo.size_kib() > SIZE_WARN_KIB {
                self.warn_once(
                    &cx.session,
                    "size",
                    format!("checkpoints for {} take more than 2 GB; sessions idle for {KEEP_DAYS} days are pruned", workspace.display()),
                );
            }
            for w in state.warnings {
                self.warn_once(&cx.session, &w, format!("checkpoint: {w}"));
            }
            Ok(())
        });
        if let Err(CheckpointError::Cancelled) = result {
            // Interrupted: the edit must not run without its snapshot; the next turn retries.
            return Err(ToolOutcome::error("interrupted"));
        }
        if let Err(e) = result {
            // The edit still runs: a missing undo is worse than none being offered, but a
            // refused edit because git is absent would be worse than both.
            self.warn_once(
                &cx.session,
                "failed",
                format!("checkpoints are unavailable ({e}); changes this turn cannot be rewound"),
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
