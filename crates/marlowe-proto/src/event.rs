//! Daemon -> client events.

use marlowe_model::{Effort, ModelInfo, ModelRef, Part, ProviderKind, Role, Usage};
use serde::{Deserialize, Serialize};

use crate::background::{
    FastState, Finding, InterruptedRun, NotifyState, RuleSuggestion, SavedWorkflow, SentFile, TaskInfo, Urgency, WorkflowInfo,
};
use crate::context::{ContextPlan, ContextPolicy, ModelContext, PlannedChange};
use crate::crew::{CrewRunInfo, CrewStep, Dispatch, RoleInfo, RunId};
use crate::ids::{ApprovalId, SessionId};
use crate::memory::{ImportReport, MemoryChange, MemoryHit, MemoryStatsReport};
use crate::phase::{PhaseEvent, PhaseSpan, StageMs, TurnNumbers};
use crate::project::{ProjectGateOption, ProjectPatch, ProjectSummary};
use crate::request::{ApprovalDecision, Slot};
use crate::watch::{BriefItem, Notice, VoiceStatus, WatchCandidate, WatchState};

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
    /// The measured phases of the turn this message began, for replay on open.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub phases: Vec<PhaseSpan>,
    /// The turn's measured totals; on the turn's first message, with `phases`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub numbers: Option<TurnNumbers>,
}

/// A line the daemon announced into a session outside any turn, kept with the conversation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredNote {
    pub at_ms: i64,
    pub title: String,
    pub body: String,
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
    /// Lines the daemon said into a session outside any turn (a held report), stored with it; sent
    /// just before the `Transcript` they belong to (reply).
    SessionNotes { session: SessionId, notes: Vec<StoredNote> },
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
        /// Set when a crew run asks; `session` is then the run's parent session.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run: Option<RunId>,
        /// The rule `ApprovalDecision::AllowPattern` would add (tool plus normalised prefix or
        /// path glob, never a bare tool), when E's rules engine proposes one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        proposed_rule: Option<String>,
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
    /// A recall finished for a turn (also when it found nothing); drives the dim
    /// `recall · N memories · 84ms  embed 12 · ...` line.
    Recalled {
        session: SessionId,
        turn: u32,
        count: u32,
        summary: String,
        /// Whole recall, ms (0 from an older daemon).
        #[serde(default)]
        total_ms: u64,
        /// Per-stage measured time, in pipeline order.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        stages: Vec<StageMs>,
        /// The gate declined to inject anything.
        #[serde(default)]
        abstained: bool,
    },
    /// A step started, advanced or ended (see [`PhaseEvent`]). `Progress` is ephemeral.
    Phase(PhaseEvent),
    /// Every measured span of a turn, published just before `TurnFinished`.
    TurnTimings {
        session: SessionId,
        turn: u32,
        spans: Vec<PhaseSpan>,
        #[serde(flatten)]
        numbers: TurnNumbers,
    },
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
    /// Reply to `MemoryProfile`: the memories in the profile block, in block order, its estimated
    /// tokens, and how many members the budget left out.
    MemoryProfile { hits: Vec<MemoryHit>, tokens: u32, omitted: u32 },
    /// The Wake brief: sent to a client after `Welcome` when the user has been away (or on a
    /// first session today), and in reply to `Request::Brief`. `text` is written; `spoken` is the
    /// ear-friendly version. `session` is where it was stored as the first assistant message.
    Brief {
        text: String,
        spoken: String,
        #[serde(default)]
        items: Vec<BriefItem>,
        #[serde(default)]
        offers: Vec<WatchCandidate>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<SessionId>,
    },
    /// A new or escalated notice (broadcast).
    Notice(Notice),
    /// Reply to `ListNotices`.
    Notices { list: Vec<Notice> },
    /// What the Sentinel watches (reply to `WatchList`/`WatchAdd`/`WatchRemove`; also broadcast on
    /// change).
    WatchState(WatchState),
    /// Reply to `WatchCandidates`.
    WatchCandidates { candidates: Vec<WatchCandidate> },
    /// Voice state; while speaking, broadcast per envelope chunk (~1 s of frames).
    VoiceState(VoiceStatus),
    /// Who the user is. Sent after `Welcome` and broadcast after `SetUser`. `set` is false when
    /// nothing has named the user yet (no `user.json`, no env), so a client may offer onboarding.
    User {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        address: Option<String>,
        #[serde(default)]
        set: bool,
    },
    /// A crew run was created or changed state, step, spend or action (broadcast).
    RunUpdated(CrewRunInfo),
    /// A run's step streamed something (broadcast). `text_delta` is assistant text; `action` is
    /// the one-line current action.
    RunStep {
        run: RunId,
        step: u32,
        action: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text_delta: Option<String>,
    },
    /// A run finished with a result (broadcast). Failed and cancelled runs only send `RunUpdated`.
    RunFinished { run: RunId, dispatch: Dispatch },
    /// Reply to `ListRuns`.
    Runs { list: Vec<CrewRunInfo> },
    /// Reply to `WatchRun`: the run as it stands and its stored steps.
    RunTranscript { run: CrewRunInfo, steps: Vec<CrewStep> },
    /// Reply to `ListRoles`.
    Roles { list: Vec<RoleInfo> },
    /// A project's state changed (broadcast, coalesced to about 100 ms). The patches take a copy
    /// of the state at revision `base` to revision `rev`; a client whose copy is not at `base`
    /// asks for the full state instead of applying them.
    ProjectUpdated { project: String, base: u64, rev: u64, patches: Vec<ProjectPatch> },
    /// A project's whole state at revision `rev` (reply to a state request, and the answer to a
    /// revision gap).
    ProjectState { project: String, rev: u64, state: serde_json::Value },
    /// Reply to `ListProjects`, newest first.
    ProjectList { projects: Vec<ProjectSummary> },
    /// A project parked on a gate and waits for the user (broadcast). `kind` is `plan` or
    /// `checkin`; `summary` is one line for a notice.
    ProjectGateRaised {
        project: String,
        gate: String,
        kind: String,
        summary: String,
        options: Vec<ProjectGateOption>,
    },
    /// A project's gate was answered and the run was re-queued (broadcast).
    ProjectGateAnswered { project: String, gate: String, choice: String },
    /// One model call finished: `tokens` (prompt + output) is what the context holds now, out of
    /// the agent's `window`. Drives the Model box's context meter.
    ContextUsage { session: SessionId, turn: u32, tokens: u64, window: u32 },
    /// Reply to `ContextPlan`.
    ContextPlan(Box<ContextPlan>),
    /// Reply to `ContextPolicies` (and broadcast after a policy is saved).
    ContextPolicies { models: Vec<ModelContext> },
    /// A policy was saved (broadcast). `pending_restart`: some changes wait for the server.
    ContextPolicySaved { model: ModelRef, policy: ContextPolicy, changes: Vec<PlannedChange>, pending_restart: bool },
    /// The outcome of `ApplyOllamaServer` (broadcast): restarted, refused, or failed, with what
    /// to do by hand when it did not happen.
    ServerApplied { ok: bool, message: String },

    // ---- M4.6 (types in `coding.rs`).
    /// A session's permission mode changed (broadcast).
    ModeChanged { session: SessionId, mode: crate::coding::PermissionMode },
    /// A session's Teams state (broadcast on a change, and the reply to `GetTeams`): `size` is the
    /// team-size guideline the reminder carries.
    TeamsChanged { session: SessionId, on: bool, size: u32 },
    /// Reply to `ListRules`, `AddRule`, `RemoveRule`.
    Rules { rules: Vec<crate::coding::RuleInfo> },
    /// Reply to `ListHooks`, `TrustHooks`.
    Hooks {
        hooks: Vec<crate::coding::HookInfo>,
        /// The digest of what the project asks to be trusted with (its hooks, widening rules,
        /// directories, commands, mode): `TrustHooks` must send it back unchanged.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        digest: String,
    },
    /// Reply to `GetSettings`, `SetSetting`.
    Settings { layers: Vec<crate::coding::SettingsLayer> },
    /// Reply to `ListCommands`.
    Commands { list: Vec<crate::coding::CommandInfo> },
    /// Reply to `ListAgentTypes`.
    AgentTypes { list: Vec<crate::coding::AgentTypeInfo> },
    /// `ask_user` is waiting; answered with `AnswerQuestion` (broadcast).
    QuestionAsked { session: SessionId, id: String, questions: Vec<crate::coding::Question> },
    /// The session's todo list changed (broadcast).
    Todos { session: SessionId, items: Vec<crate::coding::TodoItem> },
    /// Reply to `ListSchedules`, `AddSchedule`, `RemoveSchedule`.
    Schedules { list: Vec<crate::coding::ScheduleInfo> },
    /// A rewind finished (broadcast).
    Rewound { session: SessionId, turn: u32, scope: crate::coding::RewindScope },
    /// Reply to `ListCheckpoints`.
    Checkpoints { session: SessionId, list: Vec<crate::coding::CheckpointInfo> },
    /// Reply to `SessionDiff`: a unified diff.
    SessionDiff {
        session: SessionId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn: Option<u32>,
        diff: String,
    },
    /// Reply to `SessionCost`.
    SessionCost { session: SessionId, report: crate::coding::CostReport },
    /// Reply to `ExportSession`.
    SessionExported { session: SessionId, format: crate::coding::ExportFormat, text: String },
    /// A PR was opened for a run (broadcast).
    PrOpened { run: RunId, url: String },
    /// Reply to `ListExperiments`; also broadcast when one changes.
    Experiments { list: Vec<crate::coding::ExperimentInfo> },
    /// Reply to `McpList`, `McpAdd`, `McpRemove`.
    McpServers { list: Vec<crate::coding::McpServerInfo> },
    /// Reply to `PutBlob`: the reference an `Ask` carries in `images`.
    BlobStored { blob: String, mime: String, w: u32, h: u32 },
    /// Reply to `ListJobs`.
    Jobs { list: Vec<crate::coding::JobInfo> },
    /// A background job started (broadcast).
    JobStarted { job: crate::coding::JobInfo },
    /// New output from a watched job (`job_watch`, the `!` shell), in order.
    JobOutput {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<SessionId>,
        id: String,
        text: String,
    },
    /// A background job ended (broadcast).
    JobExited {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<SessionId>,
        id: String,
        code: i32,
    },

    // ---- M4.6 wave 2 (types in `background.rs`).
    /// Reply to `ListTasks`.
    Tasks { list: Vec<TaskInfo> },
    /// A task started or changed state (broadcast).
    TaskUpdated(TaskInfo),
    /// Reply to `TaskOutput`: `next_since` is the offset to ask from next; `done` once the task
    /// ended and nothing is left.
    TaskOutput { id: String, text: String, next_since: u64, done: bool },
    /// Reply to `ListWorkflows`.
    WorkflowList { saved: Vec<SavedWorkflow>, runs: Vec<WorkflowInfo> },
    /// A workflow run's phase, agents or budget changed (broadcast; also the reply to
    /// `WatchWorkflow`).
    WorkflowUpdated(WorkflowInfo),
    /// One `log()` line of a workflow (broadcast).
    WorkflowLog { run: RunId, line: String },
    /// `report_findings` stored findings (broadcast), and the reply to `ListFindings`.
    Findings {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<SessionId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run: Option<RunId>,
        findings: Vec<Finding>,
    },
    /// `notify` raised a notification (broadcast); the toast and the spoken line are the daemon's.
    Notified {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<SessionId>,
        title: String,
        #[serde(default)]
        body: String,
        urgency: Urgency,
    },
    /// Reply to `NotifySettings` (also broadcast on change).
    NotifyState(NotifyState),
    /// `send_file` copied a file to the outbox (broadcast): the client shows a file card.
    FileSent { session: SessionId, file: SentFile },
    /// `exit_plan` wrote a plan file and waits for `AnswerPlan` (broadcast).
    PlanReady { session: SessionId, id: String, path: String, text: String },
    /// `/fast` changed (broadcast).
    FastChanged { session: SessionId, state: FastState },
    /// Reply to `SuggestRules`.
    RuleSuggestions { list: Vec<RuleSuggestion> },
    /// Work the last window close interrupted, offered at the next launch to a screen client
    /// (`summary` is the tally, `3 team tasks`). Answered with `ResumeInterrupted`.
    Interrupted { runs: Vec<InterruptedRun>, summary: String },
    /// Whether the daemon runs on after the window closes (broadcast on change; the reply to
    /// `KeepRunning`).
    KeepRunning { on: bool },
    /// The daemon is closing everything (a closed window or a stop choice): work is paused, models
    /// unloaded and state saved before it exits (broadcast, then `ShuttingDown`).
    Closing { reason: String },
}

impl Event {
    /// The session this event belongs to, if any.
    pub fn session(&self) -> Option<&SessionId> {
        match self {
            Event::SessionOpened { session, .. }
            | Event::Transcript { session, .. }
            | Event::SessionNotes { session, .. }
            | Event::TurnStarted { session, .. }
            | Event::TextDelta { session, .. }
            | Event::ReasoningDelta { session, .. }
            | Event::TextRetracted { session, .. }
            | Event::ToolStarted { session, .. }
            | Event::ToolFinished { session, .. }
            | Event::TurnFinished { session, .. }
            | Event::ApprovalRequested { session, .. }
            | Event::Recalled { session, .. }
            | Event::TurnTimings { session, .. }
            | Event::ContextUsage { session, .. }
            | Event::ModeChanged { session, .. }
            | Event::TeamsChanged { session, .. }
            | Event::QuestionAsked { session, .. }
            | Event::Todos { session, .. }
            | Event::Rewound { session, .. }
            | Event::Checkpoints { session, .. }
            | Event::SessionDiff { session, .. }
            | Event::SessionCost { session, .. }
            | Event::SessionExported { session, .. }
            | Event::FileSent { session, .. }
            | Event::PlanReady { session, .. }
            | Event::FastChanged { session, .. } => Some(session),
            Event::Error { session, .. }
            | Event::JobOutput { session, .. }
            | Event::JobExited { session, .. }
            | Event::Findings { session, .. }
            | Event::Notified { session, .. } => session.as_ref(),
            Event::Phase(p) => p.session.as_ref(),
            Event::JobStarted { job } => job.session.as_ref(),
            Event::TaskUpdated(task) => task.session.as_ref(),
            _ => None,
        }
    }
}
