//! Wire types for M4.6 wave 2 (seam2): the background task registry and its tools (N), workflows
//! and findings (W), notifications and sent files (N), and the conductor's plan file, `/fast`
//! and rule suggestions (F3). The surfaces that show them are H3's.
//!
//! Same rules as `coding.rs`: a track may add optional fields (`#[serde(default)]`), never rename
//! or remove one.

use marlowe_model::ModelRef;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::coding::PermissionMode;
use crate::crew::RunId;
use crate::ids::SessionId;

// ---- Background tasks (design 2.12, 3.12).

/// What a background task is. Its id carries the kind: `job_*`, `agent_*`, `wf_*`, `mon_*`,
/// `cron_*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    /// A background shell command (B's job table).
    Job,
    /// A background subagent or a crew run.
    Agent,
    Workflow,
    /// A `monitor` watching a command's stdout.
    Monitor,
    /// A cron schedule or a one-shot reminder.
    Cron,
}

impl TaskKind {
    pub const ALL: [TaskKind; 5] = [TaskKind::Job, TaskKind::Agent, TaskKind::Workflow, TaskKind::Monitor, TaskKind::Cron];

    /// The id prefix: `job_`, `agent_`, `wf_`, `mon_`, `cron_`.
    pub fn prefix(self) -> &'static str {
        match self {
            TaskKind::Job => "job_",
            TaskKind::Agent => "agent_",
            TaskKind::Workflow => "wf_",
            TaskKind::Monitor => "mon_",
            TaskKind::Cron => "cron_",
        }
    }

    /// Stable lower-case word.
    pub fn as_str(self) -> &'static str {
        match self {
            TaskKind::Job => "job",
            TaskKind::Agent => "agent",
            TaskKind::Workflow => "workflow",
            TaskKind::Monitor => "monitor",
            TaskKind::Cron => "cron",
        }
    }

    /// The word the user reads: a crew run (`dispatch`, one specialist) and a script run are both
    /// team tasks. `as_str` stays the stable key.
    pub fn label(self) -> &'static str {
        match self {
            TaskKind::Agent | TaskKind::Workflow => "team task",
            other => other.as_str(),
        }
    }

    /// The typed id for `raw` (the owner's own id, e.g. a job's `j7`): `job_j7`.
    pub fn id(self, raw: &str) -> String {
        format!("{}{}", self.prefix(), raw)
    }

    /// The kind an id names, and the owner's id after the prefix.
    pub fn split(id: &str) -> Option<(TaskKind, &str)> {
        TaskKind::ALL.into_iter().find_map(|k| id.strip_prefix(k.prefix()).filter(|r| !r.is_empty()).map(|r| (k, r)))
    }
}

/// Where a task is in its life.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    #[default]
    Running,
    /// Alive but waiting: a workflow on an approval, a cron for its next fire, an armed monitor
    /// with nothing to say.
    Waiting,
    Done,
    Failed,
    /// Stopped by the user or the model (`task_stop`, Ctrl+C, shutdown).
    Stopped,
}

impl TaskStatus {
    pub fn is_terminal(self) -> bool {
        matches!(self, TaskStatus::Done | TaskStatus::Failed | TaskStatus::Stopped)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            TaskStatus::Running => "running",
            TaskStatus::Waiting => "waiting",
            TaskStatus::Done => "done",
            TaskStatus::Failed => "failed",
            TaskStatus::Stopped => "stopped",
        }
    }
}

/// One row of `task_list` and the `/tasks` panel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskInfo {
    /// Typed: `job_j3`, `wf_1a2b`, `cron_5`.
    pub id: String,
    pub kind: TaskKind,
    /// The session that owns it. `None`: daemon-wide (a durable cron, a crew run with no parent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionId>,
    /// One line: the command, the agent's description, the workflow's name, the cron's prompt.
    pub title: String,
    pub status: TaskStatus,
    pub started_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_ms: Option<i64>,
    /// Where its whole output is (BG3): a job's log, an agent's transcript, a workflow's journal.
    /// `read` and `grep` work on it while the task runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_file: Option<String>,
    /// Status detail: `exit 1`, `3 of 5 agents`, `next 09:00`, a failure message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

// ---- Workflows (design 2.11, 3.11).

/// What a crew run records when it runs a workflow script (`Run.workflow`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowRef {
    /// The saved workflow's name; `None` for an inline script or a bare `script_path`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The script on disk (an inline script is persisted to `<home>\workflows\runs\<run>`).
    pub script_path: String,
    /// blake3 of the script text; first run of a digest asks.
    pub digest: String,
    /// The JSON the script was started with, verbatim.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub args: Value,
    /// The turn waits for the return value (`background: false`).
    #[serde(default)]
    pub foreground: bool,
    /// The run whose journal seeded this one (`resume_from`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resumed_from: Option<RunId>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowAgentStatus {
    #[default]
    Queued,
    Running,
    Done,
    Failed,
    /// Skipped by the user (`agent()` returned `null`).
    Skipped,
}

/// One `agent()` call of a team task, for the `/teams` view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowAgentInfo {
    /// The call's issue sequence number; `SkipWorkflowAgent` names it.
    pub index: u32,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    pub status: WorkflowAgentStatus,
    #[serde(default)]
    pub tokens: u64,
    #[serde(default)]
    pub elapsed_ms: u64,
}

/// A team task run as the `/teams` view shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowInfo {
    pub run: RunId,
    pub name: String,
    /// `meta.phases`, in order.
    #[serde(default)]
    pub phases: Vec<String>,
    /// The phase the script is in now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    #[serde(default)]
    pub agents: Vec<WorkflowAgentInfo>,
    /// The tail of the `log()` lines.
    #[serde(default)]
    pub log: Vec<String>,
    /// The token budget (`+500k`) and what is spent of it, when the user set one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_total: Option<u64>,
    #[serde(default)]
    pub budget_spent: u64,
}

/// A workflow on disk: `.marlowe\workflows`, `<home>\workflows`, a plugin, a built-in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedWorkflow {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when_to_use: Option<String>,
    #[serde(default)]
    pub phases: Vec<String>,
    /// The file, or `builtin`.
    pub source: String,
}

// ---- Findings (G13, ReportFindings).

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingSeverity {
    Info,
    Low,
    #[default]
    Medium,
    High,
    Critical,
}

/// One validated finding of `report_findings`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    /// The lens that found it (`correctness`, `reuse`, `security`, ...).
    pub lens: String,
    pub severity: FindingSeverity,
    pub file: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    pub claim: String,
    #[serde(default)]
    pub evidence: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
}

// ---- Notifications and sent files (BG7, BG8).

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Urgency {
    Low,
    #[default]
    Normal,
    /// Bypasses the 1-per-minute limit and quiet hours' silence of voice.
    Urgent,
}

/// The notification switches (`/config`), as the daemon holds them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotifyState {
    /// Windows toasts on.
    pub toast: bool,
    /// A spoken line with a toast outside quiet hours.
    pub voice: bool,
}

impl Default for NotifyState {
    fn default() -> Self {
        Self { toast: true, voice: true }
    }
}

/// A file the model sent to the user (`send_file`): a copy in the outbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SentFile {
    /// The file's name, for the card.
    pub name: String,
    /// The copy under `<home>\outbox\<session>\`; this is what the card links to.
    pub path: String,
    pub bytes: u64,
    /// blake3 of the content (equals the source's at copy time).
    pub digest: String,
    #[serde(default)]
    pub note: String,
}

// ---- Conductor additions (F3).

/// The user's answer to a plan the conductor submitted with `exit_plan` (M13).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum PlanFileDecision {
    /// Leave plan mode for `mode` (`accept_edits` or `default`). The file's text as it stands
    /// (the user may have edited it) is what gets pinned.
    Approve { mode: PermissionMode },
    /// Stay in plan mode; `feedback` goes back to the model.
    Reject {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        feedback: Option<String>,
    },
}

/// One rule `/fewer-permission-prompts` proposes (M14).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleSuggestion {
    /// `bash(git status*)`.
    pub pattern: String,
    /// Approved calls in this project's transcripts that the pattern would have covered.
    pub count: u32,
    /// One call it covers.
    #[serde(default)]
    pub example: String,
}

/// `/fast` is on or off for a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FastState {
    pub on: bool,
    /// The model the next call uses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelRef>,
}

// ---- Closing the window (N, 2026-10-10).

/// A crew run or team task a closed window interrupted. It is paused, its checkpoints kept; the
/// next launch offers to resume it (`Event::Interrupted`, `Request::ResumeInterrupted`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InterruptedRun {
    pub run: RunId,
    pub title: String,
    /// A team task script's run (a crew run is one with a single specialist).
    #[serde(default)]
    pub workflow: bool,
    /// Where it stopped: `step 4: running cargo test`.
    #[serde(default)]
    pub stopped_at: String,
    pub at_ms: i64,
}

impl InterruptedRun {
    /// The prompt's tally: `3 team tasks` (a crew run is a team task with one specialist).
    pub fn tally(list: &[InterruptedRun]) -> String {
        match list.len() {
            0 => String::new(),
            1 => "1 team task".into(),
            n => format!("{n} team tasks"),
        }
    }
}
