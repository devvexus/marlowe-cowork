//! `<root>\state.json`: the selected backend, and per backend the current and previous tag and the
//! release-upgrade bookkeeping. One file for the machine: engines are installed once, not per
//! profile. Writes go through [`update`], which serialises them inside the process and replaces
//! the file atomically.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::backend::{Backend, DEFAULT};

const FILE: &str = "state.json";

static WRITE: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendState {
    /// The tag the engine launches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<String>,
    /// The version kept for rollback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<String>,
    /// "Don't ask again" for the release upgrade prompt.
    #[serde(default)]
    pub dont_ask: bool,
    /// When the release upgrade check last ran (epoch seconds): at most one a day.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_release_check: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected: Option<String>,
    /// GitHub's API said the limit is used up until this epoch second; nothing asks before then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_until: Option<u64>,
    #[serde(default)]
    pub backends: BTreeMap<String, BackendState>,
}

impl State {
    /// The selected backend; the default when none was chosen or the saved id is unknown.
    pub fn selected(&self) -> Backend {
        self.selected.as_deref().and_then(Backend::parse).unwrap_or(DEFAULT)
    }

    pub fn of(&self, backend: Backend) -> BackendState {
        self.backends.get(backend.id()).cloned().unwrap_or_default()
    }

    pub fn entry(&mut self, backend: Backend) -> &mut BackendState {
        self.backends.entry(backend.id().to_string()).or_default()
    }
}

fn path(root: &Path) -> PathBuf {
    root.join(FILE)
}

/// The saved state; a missing or unreadable file is the empty state.
pub fn load(root: &Path) -> State {
    std::fs::read_to_string(path(root)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

/// Loads, changes and saves the state under the process-wide write lock.
pub fn update<R>(root: &Path, f: impl FnOnce(&mut State) -> R) -> std::io::Result<R> {
    let _guard = WRITE.lock().unwrap_or_else(|e| e.into_inner());
    let mut s = load(root);
    let out = f(&mut s);
    std::fs::create_dir_all(root)?;
    let json = serde_json::to_string_pretty(&s).map_err(std::io::Error::other)?;
    let tmp = path(root).with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, path(root))?;
    Ok(out)
}

/// The selected backend now.
pub fn selected(root: &Path) -> Backend {
    load(root).selected()
}

/// Makes `backend` the one that serves models; true when that changed the selection.
pub fn select(root: &Path, backend: Backend) -> std::io::Result<bool> {
    update(root, |s| {
        let changed = s.selected() != backend;
        s.selected = Some(backend.id().to_string());
        changed
    })
}

/// "Don't ask again" for the release upgrade prompt of `backend`.
pub fn set_dont_ask(root: &Path, backend: Backend, on: bool) -> std::io::Result<()> {
    update(root, |s| s.entry(backend).dont_ask = on)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::scratch;

    #[test]
    fn nothing_selected_means_stock_llama_cpp_and_a_choice_is_kept() {
        let root = scratch("state");
        assert_eq!(selected(&root), Backend::LlamaCpp);
        assert!(select(&root, Backend::MsLlamaCpp).unwrap());
        assert!(!select(&root, Backend::MsLlamaCpp).unwrap(), "selecting what is selected changes nothing");
        assert_eq!(selected(&root), Backend::MsLlamaCpp);
        set_dont_ask(&root, Backend::LlamaCpp, true).unwrap();
        let s = load(&root);
        assert!(s.of(Backend::LlamaCpp).dont_ask && !s.of(Backend::MsLlamaCpp).dont_ask);
        assert_eq!(s.selected(), Backend::MsLlamaCpp, "a later update keeps the earlier fields");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_corrupt_file_reads_as_the_empty_state_and_an_unknown_id_as_the_default() {
        let root = scratch("corrupt");
        std::fs::write(root.join(FILE), "{ not json").unwrap();
        assert_eq!(load(&root), State::default());
        update(&root, |s| s.selected = Some("ollama".into())).unwrap();
        assert_eq!(selected(&root), Backend::LlamaCpp);
        let _ = std::fs::remove_dir_all(&root);
    }
}
