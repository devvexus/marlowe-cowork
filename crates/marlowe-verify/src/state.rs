//! Per-host state of the verify tools: the services it started, where logs and screenshots go,
//! the browser driver, and the project profile (cached for a moment, because approvals and
//! descriptions ask for it repeatedly).

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use marlowe_os::workspace::Workspace;

use crate::browser::{Browser, ShotStore};
use crate::profile::{self, ProjectProfile};
use crate::run::LogStore;
use crate::service::ServiceTable;

/// Judged plans kept (one per distinct call).
const JUDGED_MAX: usize = 256;

/// Everything a host keeps for `project`, `service`, `http_check` and `browser`.
#[derive(Default)]
pub struct VerifyState {
    pub services: ServiceTable,
    pub logs: LogStore,
    pub shots: ShotStore,
    browser: Mutex<Option<Arc<dyn Browser>>>,
    /// What each call was judged to run when its approval was decided (see [`Self::judge`]).
    judged: Mutex<HashMap<String, String>>,
}

impl std::fmt::Debug for VerifyState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VerifyState").field("services", &self.services).finish_non_exhaustive()
    }
}

impl VerifyState {
    /// The browser driver for `browser` calls.
    pub fn with_browser(self, browser: Arc<dyn Browser>) -> Self {
        *self.browser.lock().unwrap_or_else(|p| p.into_inner()) = Some(browser);
        self
    }

    /// Logs and screenshots under `home\verify\`, image blobs under `home\blobs\`.
    pub fn with_home(self, home: &Path) -> Self {
        self.logs.set_dir(home.join("verify").join("logs"));
        self.shots.set_dirs(home.join("verify").join("shots"), Some(home.join("blobs")));
        self
    }

    pub fn browser(&self) -> Option<Arc<dyn Browser>> {
        self.browser.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// The commands of `root` as the manifests say right now (never cached: an approval must
    /// describe what will run, and a file may have changed a moment ago).
    pub fn profile(&self, ws: &dyn Workspace, root: &Path) -> Arc<ProjectProfile> {
        Arc::new(profile::detect_commands(ws, root))
    }

    /// Records the fingerprint of the plan `key` (a tool and its arguments) was judged with.
    ///
    /// The loop decides every call's approval before any call of the batch runs, and free calls
    /// then run in parallel. A `write` that rewrites `.marlowe/project.toml` in the same batch
    /// would otherwise change what a `project` call runs after it was judged. The call compares
    /// its own plan with this ([`Self::judged_matches`]) and refuses to run on a difference.
    pub fn judge(&self, key: String, fingerprint: String) {
        let mut g = self.judged.lock().unwrap_or_else(|p| p.into_inner());
        if g.len() >= JUDGED_MAX {
            g.clear();
        }
        g.insert(key, fingerprint);
    }

    /// Whether `fingerprint` is what `key` was judged with (true when it was never judged: a
    /// call the loop did not gate is not one a stale judgement could have approved).
    pub fn judged_matches(&self, key: &str, fingerprint: &str) -> bool {
        self.judged.lock().unwrap_or_else(|p| p.into_inner()).get(key).is_none_or(|f| f == fingerprint)
    }
}
