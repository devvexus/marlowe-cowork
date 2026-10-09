//! Daemon -> client events.

use marlowe_model::{Effort, ModelInfo, ModelRef, Part, ProviderKind, Role, Usage};
use serde::{Deserialize, Serialize};

use crate::ids::{ApprovalId, SessionId};
use crate::memory::{ImportReport, MemoryChange, MemoryHit, MemoryStatsReport};
use crate::request::{ApprovalDecision, Slot};

/// Every event on the wire. Broadcast events carry a daemon-wide, strictly increasing `seq` and
/// are replayable; replies addressed to one client (Welcome, SessionList, Transcript, Catalog,
/// Status, a request's Error) carry `seq: 0` and are never replayed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    pub seq: u64,
    /// Unix epoch milliseconds when the daemon emitted it.
    pub at_ms: i64,
    pub event: Event,
}

/// Why a turn ended.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStop {
    /// The model answered with no further tool calls.
    #[default]
    Done,
    /// Interrupted by the user.
    Interrupted,
    /// Hit the step limit.
    StepLimit,
    /// Hit the token or cost budget.
    Budget,
    /// Failed; an `Error` event carries the message.
    Error,
}

/// One row of a session list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: SessionId,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    pub created_ms: i64,
    pub updated_ms: i64,
    pub turns: u32,
}

/// One stored message of a transcript.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredTurn {
    pub idx: u32,
    pub role: Role,
    pub content: Vec<Part>,
    pub at_ms: i64,
}

/// A model in the library.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LibraryModel {
    pub model: ModelRef,
    pub info: ModelInfo,
    pub added_ms: i64,
}

/// A slot's resolved assignment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlotAssignment {
    pub slot: Slot,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelRef>,
    pub effort: Effort,
    /// True when the slot is unset and falls back to the default (first added) model.
    #[serde(default)]
    pub inherited: bool,
}

/// What the daemon knows about a provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderStatus {
    pub kind: ProviderKind,
    /// An adapter exists in this build.
    pub available_now: bool,
    /// A credential (or nothing needed) is in place.
    pub configured: bool,
    /// Found running or found a key in the environment.
    pub detected: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Daemon health, for `marlowe status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusReport {
    pub daemon_version: String,
    pub pid: u32,
    pub uptime_ms: u64,
    pub clients: u32,
    pub active_sessions: Vec<SessionId>,
    pub seq: u64,
    pub profile_home: String,
}

/// A daemon event. Tagged by `ev`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "ev", rename_all = "snake_case")]
pub enum Event {
    /// Handshake accepted. `seq` is the latest seq at the time of the handshake.
    Welcome { daemon_version: String, proto_version: u32, profile_home: String, seq: u64 },
    SessionOpened {
        session: SessionId,
        title: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace: Option<String>,
    },
    SessionList { sessions: Vec<SessionSummary> },
    /// A session's stored transcript (replay on open).
    Transcript { session: SessionId, turns: Vec<StoredTurn> },
    /// A turn began. `user` is the text that started it, so every client can show it.
    TurnStarted { session: SessionId, turn: u32, model: ModelRef, user: String },
    TextDelta { session: SessionId, turn: u32, text: String },
    ReasoningDelta { session: SessionId, turn: u32, text: String, tokens: u64 },
    /// Text already streamed in this model call was reasoning; move it to the thinking line.
    TextRetracted { session: SessionId, turn: u32, tokens: u64 },
    ToolStarted { session: SessionId, turn: u32, call_id: String, tool: String, summary: String },
    ToolFinished { session: SessionId, turn: u32, call_id: String, ok: bool, summary: String },
    TurnFinished { session: SessionId, turn: u32, usage: Usage, stop: TurnStop },
    ApprovalRequested {
        approval: ApprovalId,
        session: SessionId,
        tool: String,
        summary: String,
        detail: String,
    },
    ApprovalResolved { approval: ApprovalId, decision: ApprovalDecision },
    LibraryState {
        models: Vec<LibraryModel>,
        slots: Vec<SlotAssignment>,
        providers: Vec<ProviderStatus>,
    },
    Catalog { provider: ProviderKind, models: Vec<ModelInfo> },
    Status(StatusReport),
    Error {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<SessionId>,
        message: String,
    },
    /// The daemon is exiting.
    ShuttingDown,
    /// Reply to `MemorySearch`.
    MemoryResults { hits: Vec<MemoryHit> },
    /// A memory was written, corrected or forgotten (broadcast).
    MemoryChanged { id: String, change: MemoryChange },
    /// Reply to `MemoryStats`.
    MemoryStats(MemoryStatsReport),
    /// Memories were auto-injected into a turn; drives the dim `recall · N memories` line.
    Recalled { session: SessionId, turn: u32, count: u32, summary: String },
    /// Legacy import progress (broadcast).
    ImportProgress { read: u64, imported: u64 },
    /// Legacy import finished (broadcast).
    ImportDone { report: ImportReport },
    /// An old harness profile exists. Sent after `Welcome` when nothing was imported yet, and in
    /// reply to `LegacyStatus`.
    LegacyAvailable {
        path: String,
        approx_entries: u64,
        #[serde(default)]
        already_imported: bool,
    },
    /// Reply to `MemoryProbe`: what auto-injection selected (or why it abstained) and how long
    /// the Mind took.
    MemoryProbed {
        recalled: Vec<MemoryHit>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        abstained: Option<String>,
        latency_ms: u64,
    },
}

impl Event {
    /// The session this event belongs to, if any.
    pub fn session(&self) -> Option<&SessionId> {
        match self {
            Event::SessionOpened { session, .. }
            | Event::Transcript { session, .. }
            | Event::TurnStarted { session, .. }
            | Event::TextDelta { session, .. }
            | Event::ReasoningDelta { session, .. }
            | Event::TextRetracted { session, .. }
            | Event::ToolStarted { session, .. }
            | Event::ToolFinished { session, .. }
            | Event::TurnFinished { session, .. }
            | Event::ApprovalRequested { session, .. }
            | Event::Recalled { session, .. } => Some(session),
            Event::Error { session, .. } => session.as_ref(),
            _ => None,
        }
    }
}
