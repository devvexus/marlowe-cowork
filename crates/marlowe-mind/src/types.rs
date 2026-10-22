//! Inputs and outputs of the memory service.

use std::path::PathBuf;
use std::time::Duration;

use marlowe_contract::TrustClass;
use marlowe_proto::{MemoryHit, MemoryKind, MemoryScope, MemoryTrust};

/// Who spoke a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TurnRole {
    /// Ingested at user trust, no maturation delay. Statements are injection candidates;
    /// questions and requests are stored as Episodes (recall only).
    User,
    /// Stored as Episode-kind memory; never auto-injected as a fact.
    Assistant,
}

/// One conversation turn to ingest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnIn {
    pub session: String,
    /// The session's workspace root. Turns are stored Global; the workspace is recorded per
    /// session and boosts that project's memories at injection.
    pub workspace: Option<String>,
    pub role: TurnRole,
    pub text: String,
    /// When the turn happened, Unix epoch ms.
    pub at_ms: i64,
}

/// A `/rewind` dropped turns of a session: what the session recorded from them is retracted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Retraction {
    pub session: String,
    /// The rewound turn indices.
    pub turns: Vec<u32>,
    /// The journal seq when the first rewound turn began ([`crate::Mind::journal_seq`]).
    pub after_seq: Option<u64>,
    /// For a turn stored before its seq was recorded: the instant it began, Unix epoch ms.
    pub from_ms: Option<i64>,
}

/// What kind of memory to write. `Decision` carries its reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    Fact,
    Preference,
    Decision { reason: String },
    Result,
    Note,
    Episode,
    Procedure,
}

impl Kind {
    /// The wire tag.
    pub fn tag(&self) -> MemoryKind {
        match self {
            Kind::Fact => MemoryKind::Fact,
            Kind::Preference => MemoryKind::Preference,
            Kind::Decision { .. } => MemoryKind::Decision,
            Kind::Result => MemoryKind::Result,
            Kind::Note => MemoryKind::Note,
            Kind::Episode => MemoryKind::Episode,
            Kind::Procedure => MemoryKind::Procedure,
        }
    }
}

/// Where a write came from. Trust is derived from this, never declared by the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Provenance {
    /// The user said or typed it: a user turn, the CLI, the Memory view.
    User { session: Option<String> },
    /// The conductor wrote it through a memory tool. `tainted`: the run had read untrusted content.
    Agent { session: String, turn: u32, tainted: bool },
    /// The harness computed it (exit codes, hashes, run results).
    Harness { what: String },
    /// External content: web, mail, MCP output.
    External { source: String },
}

impl Provenance {
    /// The write's own trust class, before lineage propagation.
    pub fn trust_class(&self) -> TrustClass {
        match self {
            Provenance::User { .. } => TrustClass::UserAsserted,
            Provenance::Agent { tainted: false, .. } => TrustClass::AgentInferred,
            Provenance::Agent { tainted: true, .. } => TrustClass::UntrustedContent,
            Provenance::Harness { .. } => TrustClass::AgentObserved,
            Provenance::External { .. } => TrustClass::UntrustedContent,
        }
    }

    /// The session it belongs to, if any.
    pub fn session(&self) -> Option<&str> {
        match self {
            Provenance::User { session } => session.as_deref(),
            Provenance::Agent { session, .. } => Some(session),
            Provenance::Harness { .. } | Provenance::External { .. } => None,
        }
    }
}

/// Maps a contract trust class to its wire form.
pub fn wire_trust(t: TrustClass) -> MemoryTrust {
    match t {
        TrustClass::UntrustedContent => MemoryTrust::Untrusted,
        TrustClass::AgentInferred => MemoryTrust::Inferred,
        TrustClass::AgentObserved => MemoryTrust::Observed,
        TrustClass::UserAsserted => MemoryTrust::User,
    }
}

/// An explicit memory write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryIn {
    pub text: String,
    pub kind: Kind,
    pub scope: MemoryScope,
    pub source: Provenance,
}

/// The receipt for a `remember` or `correct`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remembered {
    pub id: String,
    /// Effective trust after worst-case propagation.
    pub trust: MemoryTrust,
    /// When it becomes eligible for auto-injection; `None` means immediately.
    pub matures_at_ms: Option<i64>,
}

/// A request for memories to inject before a turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InjectQuery {
    pub session: String,
    /// Memories from this workspace are preferred after rank 1; only explicit Project memories
    /// of other projects are filtered out.
    pub workspace: Option<String>,
    pub text: String,
    /// Capped at [`crate::MAX_INJECT_TOKENS`].
    pub budget_tokens: u32,
    /// Ids already in context (the profile); they never compete for a slot.
    pub exclude: Vec<String>,
}

/// What was injected, or why nothing was.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Injection {
    /// Context blocks, one per injected memory. Only retrieved memories, never invented.
    pub blocks: Vec<String>,
    /// The memories behind `blocks`, in the same order.
    pub recalled: Vec<MemoryHit>,
    /// The lowest effective trust among `recalled`; `None` when nothing was injected. When it is
    /// `Untrusted`, untrusted text has entered the context and the run must count as tainted.
    pub floor: Option<MemoryTrust>,
    /// Why nothing was injected (gate abstained, degraded mode, error); `None` when `blocks` is non-empty.
    pub abstained: Option<String>,
    pub latency_ms: u64,
    /// Measured time per recall stage, in pipeline order (also when it abstained part-way).
    pub stages: Vec<marlowe_proto::StageMs>,
}

/// An explicit search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecallQuery {
    pub text: String,
    /// `None` searches everything; `Project(p)` searches `p` plus global memories.
    pub scope: Option<MemoryScope>,
    pub limit: usize,
}

/// A session summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Episode {
    pub id: String,
    pub session: String,
    pub workspace: Option<String>,
    pub summary: String,
    pub at_ms: i64,
}

/// What `consolidate_now` did, per session.
#[derive(Debug, Clone, Default)]
pub struct ConsolidateReport {
    pub sessions: Vec<marlowe_memory::consolidate::ConsolidationReport>,
    /// Total beliefs superseded across sessions.
    pub suppressed: usize,
}

/// Progress of a legacy import.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ImportProgress {
    pub read: u64,
    pub imported: u64,
}

/// Whether an old harness profile is there to import.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyStatus {
    pub path: PathBuf,
    /// Approximate memory events in the old journal.
    pub approx_entries: u64,
    /// An import from this profile has already completed.
    pub already_imported: bool,
}

/// A written BeliefStore snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotInfo {
    pub path: PathBuf,
    /// Journal seq the snapshot covers; boot derives only events after it.
    pub journal_seq: u64,
    pub beliefs: u64,
}

/// Asks whether the GPU may be borrowed now; the daemon builds it over the Sentinel. Called at
/// every cross-encoder load, and every 30 s while one is GPU-resident.
#[derive(Clone)]
pub struct GpuProbe(std::sync::Arc<dyn Fn() -> marlowe_memory::ep::GpuVerdict + Send + Sync>);

impl GpuProbe {
    pub fn new(f: impl Fn() -> marlowe_memory::ep::GpuVerdict + Send + Sync + 'static) -> Self {
        Self(std::sync::Arc::new(f))
    }

    pub fn verdict(&self) -> marlowe_memory::ep::GpuVerdict {
        (self.0)()
    }
}

impl std::fmt::Debug for GpuProbe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GpuProbe")
    }
}

/// Service tunables. `Default` is what the daemon runs with; tests shrink them.
#[derive(Debug, Clone)]
pub struct MindConfig {
    /// Overrides model dir resolution (see [`crate::resolve_models_dir`]).
    pub models_dir: Option<PathBuf>,
    /// A model unloads after this long unused.
    pub model_idle: Duration,
    /// Beliefs queued for embedding before the queue coalesces into a full rebuild pass.
    pub embed_queue: usize,
    /// How often a snapshot is written while running.
    pub snapshot_every: Duration,
    /// GPU idleness for the cross-encoder's provider choice. `None` keeps it on CPU.
    pub gpu: Option<GpuProbe>,
    /// Hears the write tail (ingest, embed, index) as it happens; the daemon turns it into phases.
    pub on_event: Option<crate::live::MindSink>,
    /// Hold the first embed run (the backfill of beliefs with no vector) until `on_event` is set,
    /// at most [`crate::SINK_GRACE`], so the daemon sees its model load and progress from the start.
    pub await_sink: bool,
}

impl Default for MindConfig {
    fn default() -> Self {
        Self {
            models_dir: None,
            model_idle: Duration::from_secs(10 * 60),
            embed_queue: 1024,
            snapshot_every: Duration::from_secs(15 * 60),
            gpu: None,
            on_event: None,
            await_sink: false,
        }
    }
}
