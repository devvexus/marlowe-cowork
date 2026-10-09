//! Which workspace each session ran in, and the injection admission rules that read it.
//!
//! User turns are stored Global, so a statement made in one project still reaches the others.
//! The session's workspace is kept here instead (`<profile>\workspaces.json`, written on change)
//! and turns it into a soft boost: at injection, memories from sessions in the same workspace
//! are preferred for the slots after rank 1. Not trust-bearing and not in the journal; losing
//! the file loses only the boost.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use marlowe_memory::tags::same_root;
use marlowe_memory::{BeliefKind, BeliefScope, MemoryEntry, Utterance};

/// Sidecar file inside the profile dir.
pub(crate) const WORKSPACES_FILE: &str = "workspaces.json";

/// Session id -> workspace root.
#[derive(Debug, Default)]
pub(crate) struct Workspaces {
    path: PathBuf,
    by_session: BTreeMap<String, String>,
}

impl Workspaces {
    /// Loads the map; a missing or unreadable file is an empty map.
    pub fn load(profile_dir: &Path) -> Workspaces {
        let path = profile_dir.join(WORKSPACES_FILE);
        let by_session = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                eprintln!("marlowe-mind: ignoring {}: {e}", path.display());
                BTreeMap::new()
            }),
            Err(_) => BTreeMap::new(),
        };
        Workspaces { path, by_session }
    }

    /// Records `session`'s workspace; writes the file (temp + rename) only when it changed.
    pub fn record(&mut self, session: &str, workspace: Option<&str>) {
        let Some(ws) = workspace.map(str::trim).filter(|w| !w.is_empty()) else { return };
        if self.by_session.get(session).is_some_and(|w| w == ws) {
            return;
        }
        self.by_session.insert(session.to_string(), ws.to_string());
        let tmp = self.path.with_extension("json.tmp");
        let written = serde_json::to_vec_pretty(&self.by_session)
            .map_err(std::io::Error::other)
            .and_then(|b| fs::write(&tmp, b))
            .and_then(|()| fs::rename(&tmp, &self.path));
        if let Err(e) = written {
            eprintln!("marlowe-mind: could not save {}: {e}", self.path.display());
        }
    }

    pub fn of(&self, session: &str) -> Option<&str> {
        self.by_session.get(session).map(String::as_str)
    }

    /// Whether `e` belongs to `workspace`: its own Project scope names it, or its session ran there.
    pub fn same_project(&self, e: &MemoryEntry, workspace: &str) -> bool {
        match e.scope() {
            BeliefScope::Project(root) if same_root(root, workspace) => true,
            _ => self.of(&e.source_session_id).is_some_and(|w| same_root(w, workspace)),
        }
    }
}

/// A user question or request: Episode kind on a user turn. Recallable, never injected.
pub(crate) fn is_user_request(e: &MemoryEntry) -> bool {
    e.kind() == BeliefKind::Episode && e.tags.utterance == Some(Utterance::User)
}

/// The product injection pool, on top of the core's exclusions (tombstoned, superseded,
/// unmatured, assistant prose): user requests are out; an explicit Project memory is admitted
/// only in its own workspace; conversation turns are admitted everywhere, including turns an
/// earlier build tagged with a Project scope.
pub(crate) fn admits(e: &MemoryEntry, workspace: Option<&str>) -> bool {
    if is_user_request(e) {
        return false;
    }
    match e.scope() {
        BeliefScope::Global => true,
        BeliefScope::Project(_) if e.tags.utterance.is_some() => true,
        scope @ BeliefScope::Project(_) => scope.admits(workspace),
    }
}
