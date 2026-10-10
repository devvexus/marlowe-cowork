//! The client <-> daemon wire.
//!
//! JSON lines over `127.0.0.1` TCP. A client's first line is [`Request::Hello`] with the token
//! from `daemon.json`; the daemon answers with [`Event::Welcome`] (or [`Event::Error`] and a
//! close). Every event is wrapped in an [`Envelope`] with a daemon-wide sequence number, and
//! every client sees the same stream; `Hello::since_seq` replays from the ring buffer.

#![forbid(unsafe_code)]

pub mod auth;
mod context;
mod crew;
mod event;
mod frame;
mod ids;
mod memory;
pub mod paths;
mod request;
mod user;
mod watch;

pub use context::{
    ctx_label, planned_changes, policy_label, server_change, ArchSummary, ChangeWhen, ContextPlan, ContextPolicy, Engine, EngineSettings, Fit, GpuBudget,
    KvType, LayerRole, ModelContext, PlanModel, PlanRow, PlannedChange, PolicySource, RunnerSlot, MAX_AGENTS, MIN_CTX,
    SERVER_RESTART_TOOL,
};
pub use crew::{
    Citation, Confidence, CrewBudget, CrewRunInfo, CrewRunStatus, CrewSpent, CrewStep, DiffSummary, Dispatch, Playbook,
    PlaybookParseError, RoleInfo, RunId, SandboxInfo, SandboxKind,
};
pub use event::{
    Envelope, Event, LibraryModel, ProviderStatus, SessionSummary, SlotAssignment, StatusReport,
    StoredTurn, TurnStop,
};
pub use frame::{write_line, FrameError, LineReader, MAX_LINE_BYTES};
pub use ids::{ApprovalId, SessionId};
pub use memory::{ImportReport, MemoryChange, MemoryHit, MemoryKind, MemoryScope, MemoryStatsReport, MemoryTrust};
pub use paths::DaemonInfo;
pub use request::{ApprovalDecision, Request, Secret, Slot, SlotParseError};
pub use user::{clean_user_field, AddressForm, AddressParseError, USER_FIELD_MAX};
pub use watch::{
    BriefItem, BriefItemKind, GitInfo, GpuDevice, GpuReport, Notice, NoticeSource, OllamaReport, QuietHours,
    RunInfo, RunStatus, Severity, VoiceStatus, WatchCandidate, WatchRoot, WatchState, ENVELOPE_HZ,
};

/// Wire protocol version. Bumped on any incompatible change.
pub const PROTO_VERSION: u32 = 1;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_crew;
