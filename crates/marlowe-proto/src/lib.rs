//! The client <-> daemon wire.
//!
//! JSON lines over `127.0.0.1` TCP. A client's first line is [`Request::Hello`] with the token
//! from `daemon.json`; the daemon answers with [`Event::Welcome`] (or [`Event::Error`] and a
//! close). Every event is wrapped in an [`Envelope`] with a daemon-wide sequence number, and
//! every client sees the same stream; `Hello::since_seq` replays from the ring buffer.

#![forbid(unsafe_code)]

pub mod auth;
/// M4.6 wave 2 payload types (tasks, workflows, findings, notifications, plan file); reachable
/// as `marlowe_proto::background::X`.
pub mod background;
/// M4.6 payload types. Public as a module so a track's new type is reachable as
/// `marlowe_proto::coding::X` without editing the re-export list below.
pub mod coding;
mod context;
mod crew;
mod event;
mod frame;
mod ids;
mod memory;
pub mod paths;
mod phase;
mod request;
mod research;
mod user;
mod watch;
pub mod workspace;

pub use background::{
    Finding, FindingSeverity, FastState, InterruptedRun, NotifyState, PlanFileDecision, RuleSuggestion, SavedWorkflow, SentFile, TaskInfo,
    TaskKind, TaskStatus, Urgency, WorkflowAgentInfo, WorkflowAgentStatus, WorkflowInfo, WorkflowRef,
};
pub use coding::{
    AgentTypeInfo, CheckpointInfo, QuestionOption, CommandInfo, CostReport, ExperimentInfo, ExperimentStatus, ExportFormat,
    HeadlessOptions, HookInfo, JobInfo, McpServerInfo, ModelCost, PermissionMode, Question, RewindScope, RuleAction,
    RuleInfo, ScheduleInfo, SettingsLayer, SettingsScope, TodoItem, TodoStatus,
};
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
pub use phase::{PhaseEvent, PhaseKind, PhaseSpan, PhaseState, ReadStat, StageMs, TurnNumbers};
pub use request::{ApprovalDecision, Request, Secret, Slot, SlotParseError};
pub use research::{
    ChannelId, ChannelParseError, CostQuote, PlanDecision, PlanError, ResearchCounts, ResearchPlan, ResearchProgress,
    ResearchStage, StageState, SubQuestion, PLAN_MAX_BREADTH, PLAN_MAX_DEPTH, PLAN_MAX_QUERIES, PLAN_MAX_SOURCES,
    PLAN_MAX_SUBQUESTIONS,
};
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
mod tests_coding;
#[cfg(test)]
mod tests_crew;
#[cfg(test)]
mod tests_research;
#[cfg(test)]
mod tests_wave2;
