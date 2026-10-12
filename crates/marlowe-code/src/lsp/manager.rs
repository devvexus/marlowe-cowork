//! The live language servers of one tool host (design 3.3 L2): started lazily per (server,
//! root), at most [`MAX_LIVE`] at once (the least recently used one is stopped to make room),
//! stopped after [`IDLE_STOP`] unused by a reaper thread that ends with the manager. A server
//! that fails to start is not retried for this manager's life (its error is kept and reported).
//! `MARLOWE_LSP=off` (or `0`) disables every server, for hosts and test rigs that must never
//! start one.
//!
//! Trust (see [`super::servers`]): an untrusted caller never gets a trusted server. One running
//! for its root is stopped first ([`LspManager::distrust`]). A trusted caller takes whatever runs
//! (an untrusted server is only less capable); it is not restarted. A session that took in
//! untrusted content gets no server at all (`intel::Caller::tainted`), and an untrusted start is
//! refused where the root has config that names programs a server would run
//! ([`ServerSpec::untrusted_hazard`]).

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};
use std::time::Duration;

use crossbeam_channel::{RecvTimeoutError, Sender};

use super::client::LspClient;
use super::servers::{default_specs, ServerSpec};
use super::transport::{Launch, StdioLauncher};
use super::LspError;
use crate::index::{norm_key, Lang};

/// Most servers running at once per manager.
pub const MAX_LIVE: usize = 2;
/// A server unused this long is stopped.
pub const IDLE_STOP: Duration = Duration::from_secs(600);
/// How long the initialize handshake may take.
pub const START_TIMEOUT: Duration = Duration::from_secs(60);

type Key = (&'static str, String);

#[derive(Default)]
struct Slot {
    cell: OnceLock<Result<LspClient, String>>,
}

struct Inner {
    launcher: Arc<dyn Launch>,
    specs: Vec<ServerSpec>,
    slots: Mutex<HashMap<Key, Arc<Slot>>>,
    /// Which server serves a language under a root, trusted or not (the first that can run).
    chosen: Mutex<HashMap<(Lang, String, bool), Option<usize>>>,
    max_live: usize,
    idle: Duration,
    /// Dropped with the manager, which ends the reaper.
    _reaper: Sender<()>,
}

/// The language servers of one host.
#[derive(Clone)]
pub struct LspManager {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for LspManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LspManager").field("servers", &self.inner.specs.iter().map(|s| s.id).collect::<Vec<_>>()).finish()
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Whether `MARLOWE_LSP` turns servers off.
pub fn disabled_by_env() -> bool {
    std::env::var("MARLOWE_LSP").is_ok_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "off" | "false" | "no"))
}

impl LspManager {
    /// The installed servers ([`default_specs`]) over stdio; none when `MARLOWE_LSP` is off.
    pub fn new() -> Self {
        let specs = if disabled_by_env() { Vec::new() } else { default_specs() };
        Self::with(Arc::new(StdioLauncher), specs)
    }

    /// No servers at all (the L0/L1 fallbacks answer).
    pub fn disabled() -> Self {
        Self::with(Arc::new(StdioLauncher), Vec::new())
    }

    /// `specs` started by `launcher` (tests pass the fake).
    pub fn with(launcher: Arc<dyn Launch>, specs: Vec<ServerSpec>) -> Self {
        Self::with_limits(launcher, specs, MAX_LIVE, IDLE_STOP)
    }

    pub fn with_limits(launcher: Arc<dyn Launch>, specs: Vec<ServerSpec>, max_live: usize, idle: Duration) -> Self {
        let (tx, rx) = crossbeam_channel::bounded::<()>(0);
        let inner = Arc::new(Inner {
            launcher,
            specs,
            slots: Mutex::new(HashMap::new()),
            chosen: Mutex::new(HashMap::new()),
            max_live: max_live.max(1),
            idle,
            _reaper: tx,
        });
        let weak: Weak<Inner> = Arc::downgrade(&inner);
        let tick = (idle / 4).clamp(Duration::from_millis(50), Duration::from_secs(30));
        let _ = std::thread::Builder::new().name("lsp-reaper".into()).spawn(move || loop {
            match rx.recv_timeout(tick) {
                Err(RecvTimeoutError::Timeout) => match weak.upgrade() {
                    Some(inner) => LspManager { inner }.reap(),
                    None => return,
                },
                _ => return,
            }
        });
        Self { inner }
    }

    /// Whether any server is configured.
    pub fn enabled(&self) -> bool {
        !self.inner.specs.is_empty()
    }

    /// The server for files of `lang` under `root`: the first configured one that can run there
    /// (else the first configured one, whose start then says what is missing).
    pub fn spec_for(&self, lang: Lang, root: &Path, trusted: bool) -> Option<&ServerSpec> {
        let key = (lang, norm_key(root), trusted);
        let mut chosen = lock(&self.inner.chosen);
        let i = *chosen.entry(key).or_insert_with(|| {
            let serving: Vec<usize> = (0..self.inner.specs.len()).filter(|&i| self.inner.specs[i].serves(lang)).collect();
            serving
                .iter()
                .copied()
                .find(|&i| self.inner.specs[i].launch_command(root, trusted).is_some())
                .or(serving.first().copied())
        });
        i.map(|i| &self.inner.specs[i])
    }

    fn key(spec: &ServerSpec, root: &Path) -> Key {
        (spec.id, norm_key(root))
    }

    /// A live server for `lang` under `root`, whichever configured server it is.
    fn live_for(&self, slots: &HashMap<Key, Arc<Slot>>, lang: Lang, root: &Path) -> Option<LspClient> {
        let r = norm_key(root);
        self.inner.specs.iter().filter(|s| s.serves(lang)).find_map(|s| match slots.get(&(s.id, r.clone()))?.cell.get() {
            Some(Ok(c)) if c.alive() => Some(c.clone()),
            _ => None,
        })
    }

    /// The running server for `path` under `root`, if it is up.
    pub fn running(&self, path: &Path, root: &Path) -> Option<LspClient> {
        let lang = Lang::of(path)?;
        self.live_for(&lock(&self.inner.slots), lang, root)
    }

    /// Stops every trusted server for `root` (before an untrusted caller uses the root).
    pub fn distrust(&self, root: &Path) {
        Self::distrust_locked(&mut lock(&self.inner.slots), root);
    }

    fn distrust_locked(slots: &mut HashMap<Key, Arc<Slot>>, root: &Path) {
        let r = norm_key(root);
        let trusted: Vec<Key> = slots
            .iter()
            .filter(|(k, s)| k.1 == r && matches!(s.cell.get(), Some(Ok(c)) if c.trusted()))
            .map(|(k, _)| k.clone())
            .collect();
        for k in trusted {
            if let Some(Ok(c)) = slots.remove(&k).and_then(|s| s.cell.get().cloned()) {
                stop_later(c);
            }
        }
    }

    /// The server for `path` under `root`, started if it is not running. An untrusted caller
    /// (`trusted` false) never gets a trusted server: one running is stopped first.
    pub fn client(&self, path: &Path, root: &Path, trusted: bool) -> Result<LspClient, LspError> {
        let lang = Lang::of(path).ok_or_else(|| LspError::NoServer(path.display().to_string()))?;
        // Twice at most: a trusted start that raced an untrusted caller is stopped and redone.
        for _ in 0..2 {
            let (slot, spec) = {
                let mut slots = lock(&self.inner.slots);
                if !trusted {
                    Self::distrust_locked(&mut slots, root);
                }
                if let Some(c) = self.live_for(&slots, lang, root) {
                    c.touch();
                    return Ok(c);
                }
                let spec = self.spec_for(lang, root, trusted).ok_or_else(|| LspError::NoServer(lang.name().to_string()))?.clone();
                if !trusted {
                    if let Some(what) = spec.untrusted_hazard(root) {
                        return Err(LspError::Start(format!(
                            "{} is not started for this workspace: {what} can name a program it would run",
                            spec.id
                        )));
                    }
                }
                let key = Self::key(&spec, root);
                // A server that died is started again on demand.
                if let Some(s) = slots.get(&key) {
                    if matches!(s.cell.get(), Some(Ok(c)) if !c.alive()) {
                        slots.remove(&key);
                    }
                }
                if !slots.contains_key(&key) {
                    self.make_room(&mut slots);
                }
                (slots.entry(key).or_default().clone(), spec)
            };
            let started = slot.cell.get_or_init(|| {
                LspClient::start(self.inner.launcher.as_ref(), &spec, root, trusted, START_TIMEOUT).map_err(|e| e.to_string())
            });
            match started {
                Ok(c) if !trusted && c.trusted() => continue,
                Ok(c) => {
                    c.touch();
                    return Ok(c.clone());
                }
                Err(e) => return Err(LspError::Start(e.clone())),
            }
        }
        Err(LspError::Start("a trusted server kept starting for an untrusted caller".into()))
    }

    /// The server for an edited file: the running one, or a new one when the root has the
    /// server's project marker (`Cargo.toml`, `pyproject.toml`, `package.json`, ...). An edit in
    /// a stray folder never starts a server. Untrusted, a trusted server is stopped first.
    pub fn for_edit(&self, path: &Path, root: &Path, trusted: bool) -> Option<LspClient> {
        if !trusted {
            self.distrust(root);
        }
        if let Some(c) = self.running(path, root) {
            return Some(c);
        }
        let spec = self.spec_for(Lang::of(path)?, root, trusted)?;
        if !spec.wanted_in(root) {
            return None;
        }
        self.client(path, root, trusted).ok()
    }

    /// Stops the least recently used live servers until there is room for one more.
    fn make_room(&self, slots: &mut HashMap<Key, Arc<Slot>>) {
        loop {
            let live: Vec<(Key, LspClient)> = slots
                .iter()
                .filter_map(|(k, s)| match s.cell.get() {
                    Some(Ok(c)) => Some((k.clone(), c.clone())),
                    _ => None,
                })
                .collect();
            if live.len() < self.inner.max_live {
                return;
            }
            let Some((key, victim)) = live.into_iter().max_by_key(|(_, c)| c.idle_for()) else { return };
            slots.remove(&key);
            stop_later(victim);
        }
    }

    /// Stops servers idle longer than the idle limit, and forgets dead ones.
    pub fn reap(&self) {
        let mut slots = lock(&self.inner.slots);
        let stale: Vec<Key> = slots
            .iter()
            .filter(|(_, s)| matches!(s.cell.get(), Some(Ok(c)) if !c.alive() || c.idle_for() >= self.inner.idle))
            .map(|(k, _)| k.clone())
            .collect();
        for k in stale {
            if let Some(Ok(c)) = slots.remove(&k).and_then(|s| s.cell.get().cloned()) {
                stop_later(c);
            }
        }
    }

    /// The servers running now.
    pub fn live(&self) -> Vec<LspClient> {
        lock(&self.inner.slots)
            .values()
            .filter_map(|s| match s.cell.get() {
                Some(Ok(c)) if c.alive() => Some(c.clone()),
                _ => None,
            })
            .collect()
    }

    /// The servers running now for `root` (the manager is shared across workspaces).
    pub fn live_under(&self, root: &Path) -> Vec<LspClient> {
        let r = norm_key(root);
        lock(&self.inner.slots)
            .iter()
            .filter(|(k, _)| k.1 == r)
            .filter_map(|(_, s)| match s.cell.get() {
                Some(Ok(c)) if c.alive() => Some(c.clone()),
                _ => None,
            })
            .collect()
    }

    /// Stops every server and waits for them.
    pub fn shutdown_all(&self) {
        let all: Vec<LspClient> = lock(&self.inner.slots)
            .drain()
            .filter_map(|(_, s)| s.cell.get().and_then(|r| r.as_ref().ok()).cloned())
            .collect();
        for c in all {
            c.shutdown();
        }
    }
}

impl Default for LspManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Shuts a server down off the caller's thread (the polite stop can take a couple of seconds).
fn stop_later(c: LspClient) {
    let _ = std::thread::Builder::new().name("lsp-stop".into()).spawn(move || c.shutdown());
}
