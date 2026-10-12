//! The Sentinel: strictly read-only watchers over the user's experiments and repos.
//!
//! Watch kinds, auto-detected inside each root: run directories (`heartbeat.json`, `*.log`,
//! `state.json`), tailed logs, metric/result JSON, `trainer_state.json` and `*.jsonl`/`*.csv`
//! metric logs, git repositories, the Ollama store (`GET /api/tags` only) and the GPU (NVML).
//!
//! Read-only means: files are opened only to read (std's default Windows share mode is
//! read|write|delete, so a writer is never blocked) and closed at once; nothing is ever written,
//! locked or created inside a watched directory; processes are only queried for liveness, never
//! signalled. File events come from `notify` with an in-crate debounce, with a polling fallback
//! no faster than [`SentinelConfig::poll_interval`]; tails read only appended bytes, capped per tick.
//! The one long-lived handle is the OS directory watch on each root (`ReadDirectoryChangesW`,
//! opened list-only with full sharing), which never blocks a reader or writer of its files.
//! Git runs read-only commands with `GIT_OPTIONAL_LOCKS=0`; Ollama is asked for `/api/tags` only.
//!
//! State (roots, per-file offsets, last-seen metrics, notices, per-client seen marks) lives in
//! `cowork.db`. Roots are added only by explicit request; [`discover`] returns candidates only.
//!
//! [`Sentinel`] is a cheap, cloneable handle; its threads report through a [`Sink`].

#![deny(unsafe_code)]

pub mod discover;
pub mod ci;
mod engine;
pub mod experiments;
pub mod git;
pub mod gpu;
pub mod logs;
pub mod metrics;
pub mod notice;
pub mod ollama;
pub mod probe;
pub mod process;
pub mod run;
pub mod store;
pub mod trainer;
pub mod vram;
mod watcher;

use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::Sender;

pub use marlowe_proto::{
    GitInfo, GpuReport, Notice, NoticeSource, OllamaReport, QuietHours, RunInfo, RunStatus, Severity,
    WatchCandidate, WatchRoot, WatchState,
};
pub use notice::NoticeDraft;
pub use store::SentinelStore;

/// Unix epoch ms now.
pub(crate) fn now_ms() -> i64 {
    discover::ms(std::time::SystemTime::now())
}

/// What the Sentinel reports.
#[derive(Debug, Clone, PartialEq)]
pub enum SentinelEvent {
    /// A new notice, or a duplicate that escalated in severity. Recorded before it is sent.
    Notice(Notice),
    /// The watch state changed (a run changed status, a root was added, GPU/Ollama refreshed).
    State(WatchState),
}

/// Where the Sentinel's threads send events. A full or closed sink drops events, never blocks.
pub type Sink = Sender<SentinelEvent>;

/// Tunables. `Default` is what the daemon runs with; tests shrink them.
#[derive(Debug, Clone)]
pub struct SentinelConfig {
    /// File events within this window coalesce into one rescan of the touched files.
    pub fs_debounce: Duration,
    /// Polling fallback (and liveness re-check) interval. Never below 15 s in production.
    pub poll_interval: Duration,
    pub git_interval: Duration,
    /// Each git command is killed after this long.
    pub git_timeout: Duration,
    /// `None` disables the Ollama watcher.
    pub ollama_addr: Option<SocketAddr>,
    pub ollama_interval: Duration,
    /// False disables NVML entirely.
    pub gpu: bool,
    pub gpu_interval: Duration,
    /// Most bytes read from all tails in one tick.
    pub max_read_per_tick: u64,
    /// Metric/result JSON larger than this is ignored.
    pub max_metric_json: u64,
    /// A heartbeat is stale after max(this, 5x its usual interval).
    pub stall_floor: Duration,
    /// Evals without improvement before a plateau notice.
    pub plateau_evals: usize,
    /// A live run whose process is gone must stay gone this long before it counts as ended
    /// (an orchestrator handing over to the next stage swaps pids in between).
    pub end_grace: Duration,
    pub quiet: QuietHours,
}

impl Default for SentinelConfig {
    fn default() -> Self {
        Self {
            fs_debounce: Duration::from_millis(750),
            poll_interval: Duration::from_secs(15),
            git_interval: Duration::from_secs(60),
            git_timeout: Duration::from_secs(5),
            ollama_addr: Some(SocketAddr::from((Ipv4Addr::LOCALHOST, 11434))),
            ollama_interval: Duration::from_secs(120),
            gpu: true,
            gpu_interval: Duration::from_secs(15),
            max_read_per_tick: 256 * 1024,
            max_metric_json: 1024 * 1024,
            stall_floor: Duration::from_secs(10 * 60),
            plateau_evals: 5,
            end_grace: Duration::from_secs(45),
            quiet: QuietHours::default(),
        }
    }
}

/// A Sentinel failure.
#[derive(Debug, thiserror::Error)]
pub enum SentinelError {
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("database: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("watch: {0}")]
    Notify(#[from] notify::Error),
    #[error("not a directory: {0}")]
    NotADirectory(String),
    #[error("{0} is not watched")]
    NotWatched(String),
    #[error("ollama: {0}")]
    Ollama(String),
    #[error("the Sentinel has stopped")]
    Stopped,
}

/// Handle over the Sentinel's threads. Clones share them; [`Sentinel::shutdown`] stops them.
#[derive(Clone)]
pub struct Sentinel {
    inner: Arc<watcher::Inner>,
}

impl Sentinel {
    /// Opens the Sentinel tables in `<home>\cowork.db` and starts watching the persisted roots
    /// plus `roots` (watched for this process only, not persisted: tests and `--watch`).
    pub fn start(home: &Path, roots: Vec<PathBuf>, sink: Sink) -> Result<Sentinel, SentinelError> {
        Self::start_with(home, roots, sink, SentinelConfig::default())
    }

    /// [`Sentinel::start`] with explicit tunables.
    pub fn start_with(home: &Path, roots: Vec<PathBuf>, sink: Sink, cfg: SentinelConfig) -> Result<Sentinel, SentinelError> {
        Ok(Sentinel { inner: watcher::Inner::start(home, roots, sink, cfg)? })
    }

    /// The current watch state (roots, runs, GPU, Ollama, quiet hours). Cached; never blocks on I/O.
    pub fn state(&self) -> WatchState {
        self.inner.state()
    }

    /// Adds and persists a watch root (explicit consent only). Returns the new state.
    pub fn add_root(&self, path: &Path) -> Result<WatchState, SentinelError> {
        self.inner.add_root(path)
    }

    /// Removes a persisted root and stops watching it. Returns the new state.
    pub fn remove_root(&self, path: &Path) -> Result<WatchState, SentinelError> {
        self.inner.remove_root(path)
    }

    /// Runs discovery over [`discover::projects_dir`]. Blocking (a bounded directory walk): call
    /// it off the request thread. Already-watched roots are left out.
    pub fn candidates(&self) -> Vec<WatchCandidate> {
        self.inner.candidates()
    }

    /// Notices, ranked (severity, then newest), with `seen` for `client`.
    pub fn notices(&self, client: &str, limit: usize, unseen_only: bool) -> Result<Vec<Notice>, SentinelError> {
        self.inner.store().notices(client, limit, unseen_only)
    }

    /// Records a notice raised by the daemon itself (a finished job, a monitor, a `notify` call,
    /// a message). `None`: it duplicated one already recorded at the same severity. This only
    /// records: the daemon broadcasts, speaks and toasts it.
    pub fn raise(&self, draft: NoticeDraft) -> Result<Option<Notice>, SentinelError> {
        let at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        self.inner.store().record(&draft, at_ms)
    }

    /// Notices raised or refreshed at or after `since_ms`, ranked. Feeds the brief.
    pub fn notices_since(&self, since_ms: i64) -> Result<Vec<Notice>, SentinelError> {
        self.inner.store().notices_since(since_ms)
    }

    /// A result file beside the sibling it is judged against (stored metrics first, else a
    /// read-only read). Blocking (a directory listing); call it off the request thread.
    pub fn result_comparison(&self, file: &Path) -> Option<metrics::ResultComparison> {
        let store = self.inner.store();
        let cached = |p: &Path| store.metrics(&p.to_string_lossy()).ok().flatten();
        metrics::load_comparison(file, self.inner.max_metric_json(), &cached)
    }

    /// Marks a notice seen for `client`. False when no such notice exists.
    pub fn mark_seen(&self, client: &str, id: &str) -> Result<bool, SentinelError> {
        self.inner.store().mark_seen(client, id)
    }

    pub fn quiet_hours(&self) -> QuietHours {
        self.inner.quiet_hours()
    }

    /// Persists new quiet hours.
    pub fn set_quiet_hours(&self, quiet: QuietHours) -> Result<(), SentinelError> {
        self.inner.set_quiet_hours(quiet)
    }

    /// Branch, dirty count and last commit for the repo containing `workspace`; cached for a
    /// short while so the agent's environment block can ask every turn.
    pub fn git_context(&self, workspace: &Path) -> Option<GitInfo> {
        self.inner.git_context(workspace)
    }

    /// Stops the watcher threads (idempotent). The sink's receiver then sees a disconnect once
    /// every clone of the sink is dropped.
    pub fn shutdown(&self) {
        self.inner.shutdown()
    }
}
