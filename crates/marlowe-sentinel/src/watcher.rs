//! The Sentinel's threads and the handle state behind [`crate::Sentinel`]: the engine thread
//! (file events, debounce, poll ticks; see [`crate::engine`]) and a probe thread for git, Ollama
//! and the GPU, which may block on a child process or a socket and so never runs on the engine.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use crossbeam_channel::{unbounded, Receiver, RecvTimeoutError, Sender};
use marlowe_proto::{GitInfo, NoticeSource, OllamaReport, QuietHours, Severity, WatchCandidate, WatchRoot, WatchState};

use crate::engine::{path_key, raise, Cmd, Engine};
use crate::gpu::Gpu;
use crate::notice::NoticeDraft;
use crate::ollama::{self, OllamaModel};
use crate::store::SentinelStore;
use crate::{discover, git, now_ms, SentinelConfig, SentinelError, Sink};

const QUIET_KEY: &str = "quiet_hours";
const OLLAMA_KEY: &str = "ollama_tags";
/// How long `add_root`/`remove_root` wait for the engine to finish the scan.
const REPLY_WAIT: Duration = Duration::from_secs(10);
/// `git_context` answers from cache for this long.
const GIT_CACHE: Duration = Duration::from_secs(10);
/// New Ollama models stay listed in the state this long.
const NEW_MODEL_WINDOW_MS: i64 = 24 * 3600 * 1000;
const OLLAMA_TIMEOUT: Duration = Duration::from_secs(3);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// An absolute, lexically normalised path (no trailing separator).
pub(crate) fn normalize(path: &Path) -> Result<PathBuf, SentinelError> {
    Ok(std::path::absolute(path)?.components().collect())
}

/// Path equality, case-insensitive on Windows.
pub(crate) fn same_path(a: &Path, b: &Path) -> bool {
    if cfg!(windows) {
        a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
    } else {
        a == b
    }
}

fn parse_quiet(s: &str) -> Option<QuietHours> {
    let (a, b) = s.split_once('-')?;
    let (start_min, end_min) = (a.trim().parse().ok()?, b.trim().parse().ok()?);
    (start_min < 1440 && end_min < 1440).then_some(QuietHours { start_min, end_min })
}

enum Probe {
    /// Roots changed: probe git now.
    Kick,
    Stop,
}

/// Shared state behind every [`crate::Sentinel`] clone.
pub(crate) struct Inner {
    store: Arc<SentinelStore>,
    cfg: SentinelConfig,
    cmd: Sender<Cmd>,
    probe: Sender<Probe>,
    snapshot: Arc<Mutex<WatchState>>,
    roots: Arc<Mutex<Vec<PathBuf>>>,
    quiet: Mutex<QuietHours>,
    git_cache: Mutex<HashMap<PathBuf, (Instant, Option<GitInfo>)>>,
    threads: Mutex<Vec<JoinHandle<()>>>,
    stopped: AtomicBool,
}

impl Inner {
    pub fn start(home: &Path, roots: Vec<PathBuf>, sink: Sink, cfg: SentinelConfig) -> Result<Arc<Inner>, SentinelError> {
        let store = Arc::new(SentinelStore::open(&marlowe_proto::paths::cowork_db(home))?);
        let quiet = store.kv(QUIET_KEY)?.and_then(|s| parse_quiet(&s)).unwrap_or(cfg.quiet);
        let mut initial: Vec<(PathBuf, bool, i64)> = store.roots()?.into_iter().map(|r| (PathBuf::from(r.path), true, r.added_ms)).collect();
        for r in roots {
            let p = normalize(&r)?;
            if !initial.iter().any(|(q, _, _)| same_path(q, &p)) {
                initial.push((p, false, now_ms()));
            }
        }
        let snapshot = Arc::new(Mutex::new(WatchState {
            roots: initial.iter().map(|(p, _, at)| WatchRoot { path: path_key(p), added_ms: *at, runs: 0, git: None, problem: None }).collect(),
            quiet,
            ..WatchState::default()
        }));
        let shared_roots = Arc::new(Mutex::new(Vec::new()));
        let (cmd_tx, cmd_rx) = unbounded();
        let (probe_tx, probe_rx) = unbounded();
        let engine = Engine::new(cfg.clone(), store.clone(), sink.clone(), home.to_path_buf(), cmd_tx.clone(), shared_roots.clone(), snapshot.clone(), quiet);
        let mut threads = Vec::new();
        threads.push(std::thread::Builder::new().name("marlowe-sentinel".into()).spawn(move || engine.run(cmd_rx, initial))?);
        let prober = Prober { cfg: cfg.clone(), store: store.clone(), sink, cmd: cmd_tx.clone(), roots: shared_roots.clone() };
        match std::thread::Builder::new().name("marlowe-sentinel-probe".into()).spawn(move || prober.run(probe_rx)) {
            Ok(h) => threads.push(h),
            Err(e) => {
                let _ = cmd_tx.send(Cmd::Shutdown);
                return Err(e.into());
            }
        }
        Ok(Arc::new(Inner {
            store,
            cfg,
            cmd: cmd_tx,
            probe: probe_tx,
            snapshot,
            roots: shared_roots,
            quiet: Mutex::new(quiet),
            git_cache: Mutex::new(HashMap::new()),
            threads: Mutex::new(threads),
            stopped: AtomicBool::new(false),
        }))
    }

    pub fn store(&self) -> &SentinelStore {
        &self.store
    }

    pub fn state(&self) -> WatchState {
        lock(&self.snapshot).clone()
    }

    /// Sends a command to the engine and waits (bounded) for it to finish.
    fn ask(&self, make: impl FnOnce(Sender<()>) -> Cmd) -> Result<(), SentinelError> {
        if self.stopped.load(Ordering::SeqCst) {
            return Err(SentinelError::Stopped);
        }
        let (tx, rx) = crossbeam_channel::bounded(1);
        self.cmd.send(make(tx)).map_err(|_| SentinelError::Stopped)?;
        match rx.recv_timeout(REPLY_WAIT) {
            Ok(()) | Err(RecvTimeoutError::Timeout) => Ok(()),
            Err(RecvTimeoutError::Disconnected) => Err(SentinelError::Stopped),
        }
    }

    pub fn add_root(&self, path: &Path) -> Result<WatchState, SentinelError> {
        let p = normalize(path)?;
        if !p.is_dir() {
            return Err(SentinelError::NotADirectory(path_key(&p)));
        }
        // Keep the spelling already watched, if any.
        let p = lock(&self.roots).iter().find(|r| same_path(r, &p)).cloned().unwrap_or(p);
        let at = now_ms();
        self.store.add_root(&path_key(&p), at)?;
        self.ask(|done| Cmd::AddRoot { path: p, persisted: true, added_ms: at, done: Some(done) })?;
        let _ = self.probe.send(Probe::Kick);
        Ok(self.state())
    }

    pub fn remove_root(&self, path: &Path) -> Result<WatchState, SentinelError> {
        let p = normalize(path)?;
        let stored = self.store.roots()?.into_iter().map(|r| PathBuf::from(r.path)).find(|r| same_path(r, &p));
        let live = lock(&self.roots).iter().find(|r| same_path(r, &p)).cloned();
        let Some(p) = live.clone().or(stored.clone()) else { return Err(SentinelError::NotWatched(path_key(&p))) };
        if let Some(s) = &stored {
            self.store.remove_root(&path_key(s))?;
        }
        self.ask(|done| Cmd::RemoveRoot { path: p, done })?;
        let _ = self.probe.send(Probe::Kick);
        Ok(self.state())
    }

    pub fn candidates(&self) -> Vec<WatchCandidate> {
        let Some(base) = discover::projects_dir() else { return Vec::new() };
        let mut watched = lock(&self.roots).clone();
        watched.extend(self.store.roots().unwrap_or_default().into_iter().map(|r| PathBuf::from(r.path)));
        discover::discover(&base, SystemTime::now(), discover::DEFAULT_DEPTH, discover::RECENT, &watched)
    }

    pub fn quiet_hours(&self) -> QuietHours {
        *lock(&self.quiet)
    }

    pub fn set_quiet_hours(&self, quiet: QuietHours) -> Result<(), SentinelError> {
        self.store.set_kv(QUIET_KEY, &format!("{}-{}", quiet.start_min, quiet.end_min))?;
        *lock(&self.quiet) = quiet;
        let _ = self.cmd.send(Cmd::Quiet(quiet));
        Ok(())
    }

    pub fn git_context(&self, workspace: &Path) -> Option<GitInfo> {
        let repo = git::repo_root(workspace)?;
        if let Some((at, info)) = lock(&self.git_cache).get(&repo) {
            if at.elapsed() < GIT_CACHE {
                return info.clone();
            }
        }
        let info = git::probe(&repo, None, self.cfg.git_timeout);
        lock(&self.git_cache).insert(repo, (Instant::now(), info.clone()));
        info
    }

    pub fn shutdown(&self) {
        if self.stopped.swap(true, Ordering::SeqCst) {
            return;
        }
        let _ = self.cmd.send(Cmd::Shutdown);
        let _ = self.probe.send(Probe::Stop);
        let threads = std::mem::take(&mut *lock(&self.threads));
        for t in threads {
            let _ = t.join();
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// The probe thread: git every `git_interval`, Ollama tags every `ollama_interval`, NVML every
/// `gpu_interval`. Results go to the engine; notices are recorded here.
struct Prober {
    cfg: SentinelConfig,
    store: Arc<SentinelStore>,
    sink: Sink,
    cmd: Sender<Cmd>,
    roots: Arc<Mutex<Vec<PathBuf>>>,
}

impl Prober {
    fn run(self, rx: Receiver<Probe>) {
        let gpu = if self.cfg.gpu { Gpu::open() } else { Gpu::disabled("GPU watching is off") };
        let mut tags: Option<Vec<OllamaModel>> = self.store.kv(OLLAMA_KEY).ok().flatten().map(|s| decode_tags(&s));
        let mut recent: Vec<(String, i64)> = Vec::new();
        let now = Instant::now();
        let (mut next_git, mut next_gpu, mut next_ollama) = (now, now, now);
        let mut gpu_sent = false;
        loop {
            let now = Instant::now();
            if now >= next_git {
                self.probe_git();
                next_git = Instant::now() + self.cfg.git_interval;
            }
            if self.cfg.gpu && now >= next_gpu && (gpu.is_available() || !gpu_sent) {
                let _ = self.cmd.send(Cmd::Gpu(gpu.sample()));
                gpu_sent = true;
                next_gpu = Instant::now() + self.cfg.gpu_interval;
            } else if !self.cfg.gpu && !gpu_sent {
                let _ = self.cmd.send(Cmd::Gpu(gpu.sample()));
                gpu_sent = true;
            }
            if let Some(addr) = self.cfg.ollama_addr {
                if now >= next_ollama {
                    self.probe_ollama(addr, &mut tags, &mut recent);
                    next_ollama = Instant::now() + self.cfg.ollama_interval;
                }
            }
            let mut wake = next_git;
            if self.cfg.gpu && gpu.is_available() {
                wake = wake.min(next_gpu);
            }
            if self.cfg.ollama_addr.is_some() {
                wake = wake.min(next_ollama);
            }
            match rx.recv_timeout(wake.saturating_duration_since(Instant::now())) {
                Ok(Probe::Kick) => next_git = Instant::now(),
                Ok(Probe::Stop) | Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
    }

    /// Probes each root's repository once per pass (roots sharing a repo share the probe).
    fn probe_git(&self) {
        let roots = lock(&self.roots).clone();
        let mut repos: HashMap<PathBuf, Option<GitInfo>> = HashMap::new();
        for root in roots {
            let info = match git::repo_root(&root) {
                Some(repo) => repos.entry(repo.clone()).or_insert_with(|| self.probe_repo(&repo)).clone(),
                None => None,
            };
            let _ = self.cmd.send(Cmd::Git(root, info));
        }
    }

    fn probe_repo(&self, repo: &Path) -> Option<GitInfo> {
        let key = format!("git_head:{}", path_key(repo));
        let last = self.store.kv(&key).ok().flatten();
        let info = git::probe(repo, last.as_deref(), self.cfg.git_timeout)?;
        if let Some(head) = &info.head {
            if last.as_deref() != Some(head.as_str()) {
                if last.is_some() && info.new_commits > 0 {
                    let n = info.new_commits;
                    let name = repo.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| path_key(repo));
                    let title = format!("{name}: {n} new commit{} on {}", if n == 1 { "" } else { "s" }, info.branch);
                    let detail = info.subject.as_deref().map(|s| format!("latest: {s}")).unwrap_or_default();
                    raise(&self.store, &self.sink, NoticeDraft::new(NoticeSource::Git, Severity::Info, "new_commits", title, detail, path_key(repo)));
                }
                let _ = self.store.set_kv(&key, head);
            }
        }
        Some(info)
    }

    fn probe_ollama(&self, addr: std::net::SocketAddr, tags: &mut Option<Vec<OllamaModel>>, recent: &mut Vec<(String, i64)>) {
        let now = now_ms();
        recent.retain(|(_, at)| now - at <= NEW_MODEL_WINDOW_MS);
        let report = match ollama::tags(addr, OLLAMA_TIMEOUT) {
            Ok(list) => {
                // The first sighting is the baseline: nothing is "new" yet.
                let changed = tags.as_deref().map(|before| ollama::changed(before, &list)).unwrap_or_default();
                for name in &changed {
                    let updated = tags.as_deref().is_some_and(|b| b.iter().any(|m| &m.name == name));
                    let m = list.iter().find(|m| &m.name == name);
                    let size = m.map(|m| format!("{:.1} GB", m.size as f64 / 1e9)).unwrap_or_default();
                    let sev = if name.starts_with("marlowe") { Severity::Good } else { Severity::Info };
                    let title = if updated { format!("Model updated: {name}") } else { format!("New model: {name}") };
                    raise(&self.store, &self.sink, NoticeDraft::new(NoticeSource::Ollama, sev, "new_model", title, size, name.clone()));
                    recent.retain(|(n, _)| n != name);
                    recent.push((name.clone(), now));
                }
                if tags.as_ref() != Some(&list) {
                    let _ = self.store.set_kv(OLLAMA_KEY, &encode_tags(&list));
                }
                let report = OllamaReport { reachable: true, models: list.len() as u32, new_models: recent.iter().map(|(n, _)| n.clone()).collect() };
                *tags = Some(list);
                report
            }
            Err(_) => OllamaReport { reachable: false, models: 0, new_models: recent.iter().map(|(n, _)| n.clone()).collect() },
        };
        let _ = self.cmd.send(Cmd::Ollama(report));
    }
}

fn encode_tags(list: &[OllamaModel]) -> String {
    let rows: Vec<(&str, &str, &str, u64)> = list.iter().map(|m| (m.name.as_str(), m.digest.as_str(), m.modified_at.as_str(), m.size)).collect();
    serde_json::to_string(&rows).unwrap_or_else(|_| "[]".into())
}

fn decode_tags(s: &str) -> Vec<OllamaModel> {
    serde_json::from_str::<Vec<(String, String, String, u64)>>(s)
        .unwrap_or_default()
        .into_iter()
        .map(|(name, digest, modified_at, size)| OllamaModel { name, digest, modified_at, size })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quiet_and_tags_round_trip() {
        assert_eq!(parse_quiet("1380-420"), Some(QuietHours { start_min: 1380, end_min: 420 }));
        assert_eq!(parse_quiet("2000-1"), None);
        let list = vec![OllamaModel { name: "a".into(), digest: "d".into(), modified_at: "t".into(), size: 3 }];
        assert_eq!(decode_tags(&encode_tags(&list)), list);
        assert!(same_path(Path::new("C:\\A\\b"), Path::new(if cfg!(windows) { "c:\\a\\B" } else { "C:\\A\\b" })));
        let n = normalize(Path::new("x/./y/")).unwrap();
        assert!(n.is_absolute() && n.ends_with("x/y"));
    }
}
