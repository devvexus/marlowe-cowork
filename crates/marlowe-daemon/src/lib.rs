//! The daemon. Concrete providers and tools are injected through [`DaemonDeps`], so this crate
//! depends on no provider or executor crate.
//!
//! Threads: an acceptor; per client a reader (requests) and a writer (bounded queue; a client
//! that falls behind is dropped, never the daemon); a [`bus`] that numbers broadcast events and
//! keeps a replay ring; a store actor owning the sessions/turns tables in `cowork.db`; one runner
//! per active session owning its `Conversation` and steer inbox.
//!
//! Reply convention: replies to one client carry `seq: 0`. A `SessionOpened` with `seq: 0` means
//! "the session you asked for" (OpenSession, NewSession, or an Ask that created it); the same
//! event with `seq > 0` tells every client the session exists or was retitled.

#![forbid(unsafe_code)]

pub mod brief;
mod bg_ops;
mod blob_ops;
mod boot;
mod brief_ops;
mod bus;
mod checkpoint;
mod client;
mod close_ops;
mod conductor_ops;
mod context_ops;
mod crew;
mod crew_ops;
mod daemon;
mod exp_ops;
mod f3_ops;
mod job_ops;
mod library;
mod live;
mod mcp_ops;
mod mind;
mod mind_ops;
mod mode_ops;
mod notify_ops;
mod phases;
mod pr_ops;
pub mod probe;
mod schedule_ops;
mod session;
mod session_ops;
mod stack;
mod store;
mod task_ops;
mod teams_ops;
mod tasks;
pub mod user;
mod watch_ops;
mod web_ops;
mod workflow_ops;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::Receiver;
use marlowe_agent::{ToolHost, UserInfo};
use marlowe_brains::{BrainsError, ProviderFactory};
use marlowe_model::ModelInfo;
use marlowe_sentinel::{Sentinel, SentinelEvent};
use marlowe_voice::{Voice, VoiceEvent};

pub use close_ops::{resume_prompt, CloseHooks, Unloader};
pub use daemon::{Daemon, DaemonControl};
pub use notify_ops::{NotifyHooks, Opener, ShellOpener, Speaker};
pub use mind::{ComposedTools, MindMemory, MindTools, RecallSink, MEMORY_TOOLS};

/// What the daemon knows when it builds a session's tool host.
#[derive(Debug, Clone, Default)]
pub struct HostRequest {
    /// The session's workspace root (`None`: no workspace).
    pub workspace: Option<PathBuf>,
    /// The conductor's model when the session started, so the host fixes its tool set for the
    /// session (`BuiltinTools::with_model_exposure`). `None` when no model is assigned yet.
    pub model: Option<ModelInfo>,
    /// The session the host is built for (its background jobs belong to it; track B). `None`
    /// for hosts no session owns (a PR's dry run, a subagent's root).
    pub session: Option<marlowe_proto::SessionId>,
}

/// Builds the tool host for a session.
pub type ToolHostFactory = Arc<dyn Fn(&HostRequest) -> Arc<dyn ToolHost> + Send + Sync>;

/// Everything the binary injects.
pub struct DaemonDeps {
    /// Provider constructors, registered by the binary.
    pub providers: ProviderFactory,
    /// Tool hosts per workspace.
    pub tools: ToolHostFactory,
    /// Name and address form for the system prompt's user block, as of start (the binary reads
    /// [`user::load`], else env). `SetUser` replaces it live.
    pub user: UserInfo,
    /// The memory service. `None`: no memory (tests, or a profile that failed to open).
    pub mind: Option<marlowe_mind::Mind>,
    /// The watchers. `None`: no Sentinel (tests); watch requests answer with an error.
    pub sentinel: Option<SentinelLink>,
    /// Spoken voice. `None`: silent (tests); voice requests answer with an error.
    pub voice: Option<VoiceLink>,
    /// The crew's tool factory and tunables. `None`: no crew; `dispatch` is not exposed, the
    /// crew persona block is not appended, and crew requests (but `ListRoles`) answer with an
    /// error. The daemon starts the supervisor itself with its models, run approvals, memory and
    /// the Sentinel's GPU state.
    pub crew: Option<marlowe_crew::CrewSetup>,
}

/// A started [`Sentinel`] and the receiving end of the sink it was started with. The daemon
/// forwards its events to the bus and shuts it down on exit.
pub struct SentinelLink {
    pub sentinel: Sentinel,
    pub events: Receiver<SentinelEvent>,
}

/// A started [`Voice`] and the receiving end of its event channel.
pub struct VoiceLink {
    pub voice: Voice,
    pub events: Receiver<VoiceEvent>,
}

/// Tunables. `Default` is what the binary runs with; tests shrink them.
#[derive(Debug, Clone)]
pub struct DaemonConfig {
    /// Events queued per client before that client is dropped as too slow. Raised to
    /// `replay_ring + 2` so a full replay always fits.
    pub client_queue: usize,
    /// Broadcast events kept for replay.
    pub replay_ring: usize,
    /// An unanswered approval is denied after this long.
    pub approval_timeout: Duration,
    /// A connection must send `Hello` within this long.
    pub hello_timeout: Duration,
    /// A blocked socket write fails after this long and drops the client.
    pub write_timeout: Duration,
    /// How long shutdown waits for running turns to stop and persist.
    pub drain_timeout: Duration,
    /// An idle session's runner exits after this long (writing an episode when memory is on);
    /// the next ask reloads it from the store.
    pub session_idle: Duration,
    /// Offer the old harness profile to each client after `Welcome` (needs a memory service).
    pub legacy_offer: bool,
    /// Let the brief's polish call a local model (Ollama, GGUF). Off by default: it would load a
    /// model onto the GPU the user's jobs use. Even when on, it waits for an idle GPU and no live
    /// run. `MARLOWE_BRIEF_LOCAL_POLISH=1` turns it on for `marlowe daemon`.
    pub local_polish: bool,
    /// How long after the last window disconnects without saying goodbye the daemon waits for
    /// one to reconnect before it stops everything (a closed window).
    pub close_grace: Duration,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            client_queue: 16_384,
            replay_ring: 8_192,
            approval_timeout: Duration::from_secs(600),
            hello_timeout: Duration::from_secs(10),
            write_timeout: Duration::from_secs(15),
            drain_timeout: Duration::from_secs(10),
            session_idle: Duration::from_secs(30 * 60),
            legacy_offer: true,
            local_polish: false,
            close_grace: Duration::from_secs(2),
        }
    }
}

/// A daemon failure.
#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Brains(#[from] BrainsError),
    #[error("database: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("a daemon is already serving {0}")]
    AlreadyRunning(String),
}

/// Runs the daemon for the profile home `home` until `Shutdown`. Binds 127.0.0.1:0, writes
/// `daemon.json`, serves clients, and removes `daemon.json` on exit.
pub fn run(deps: DaemonDeps, home: &Path) -> Result<(), DaemonError> {
    let local_polish = std::env::var("MARLOWE_BRIEF_LOCAL_POLISH").is_ok_and(|v| v.trim() == "1");
    Daemon::start(deps, home, DaemonConfig { local_polish, ..DaemonConfig::default() })?.wait()
}

/// Unix epoch milliseconds.
pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Locks a mutex, ignoring poison: a panicked holder must not take the daemon down.
pub(crate) fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}
