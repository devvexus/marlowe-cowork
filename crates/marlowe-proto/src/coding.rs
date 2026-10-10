//! Wire types for M4.6 (full-stack engineering): permission modes, rules, hooks and settings (E),
//! slash commands, agent types, questions, todos and schedules (F), checkpoints, cost and export
//! (F2), PRs (J), experiments (J2) and MCP servers (K). The seam fixes their shape so the client
//! (H) and the daemon tracks build against the same types in parallel; a track may add optional
//! fields (`#[serde(default)]`), never rename or remove one.

use marlowe_model::{ModelRef, Usage};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ids::SessionId;

/// The conductor's permission mode (design 3.5). `auto` is accepted as `bypass`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    /// Read-only tools; `exit_plan` asks to leave.
    Plan,
    /// Shells ask; edits free in the workspace.
    #[default]
    Default,
    /// Plus project-profile commands and read-only git free.
    AcceptEdits,
    /// Free inside the sandbox; tripwire hits and fixed approvals still ask.
    #[serde(alias = "auto")]
    Bypass,
    /// Anything not allowed is denied with the reason; the run continues.
    DontAsk,
}

impl PermissionMode {
    pub const ALL: [PermissionMode; 5] =
        [PermissionMode::Plan, PermissionMode::Default, PermissionMode::AcceptEdits, PermissionMode::Bypass, PermissionMode::DontAsk];

    pub fn as_str(self) -> &'static str {
        match self {
            PermissionMode::Plan => "plan",
            PermissionMode::Default => "default",
            PermissionMode::AcceptEdits => "accept_edits",
            PermissionMode::Bypass => "bypass",
            PermissionMode::DontAsk => "dont_ask",
        }
    }

    /// The next mode for Shift+Tab (plan is entered explicitly, bypass and dont_ask are not
    /// cycled into).
    pub fn cycle(self) -> PermissionMode {
        match self {
            PermissionMode::Default => PermissionMode::AcceptEdits,
            PermissionMode::AcceptEdits => PermissionMode::Plan,
            _ => PermissionMode::Default,
        }
    }
}

/// Which settings layer a change goes to (design 2.3 M9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingsScope {
    /// `<home>\settings.toml`.
    User,
    /// `.marlowe\settings.toml` (checked in).
    Project,
    /// `.marlowe\settings.local.toml` (not checked in; "always for this project").
    Local,
    /// This session only.
    Session,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleAction {
    Allow,
    Ask,
    Deny,
}

/// One permission rule, e.g. `bash(git push*)`, `edit(src/**)`, `web_fetch(domain:x)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleInfo {
    pub action: RuleAction,
    pub pattern: String,
    pub scope: SettingsScope,
    /// Where it came from (a settings file path, `.claude/settings.json` import, `--allowed-tools`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// One configured hook.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookInfo {
    /// `PreToolUse`, `PostToolUse`, `Stop`, ... (Claude Code event names).
    pub event: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matcher: Option<String>,
    pub command: String,
    pub scope: SettingsScope,
    /// Project hooks run only once trusted by digest.
    #[serde(default)]
    pub trusted: bool,
}

/// One settings layer as it stands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingsLayer {
    pub scope: SettingsScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// The layer's TOML.
    pub text: String,
}

/// A slash command (`.marlowe/commands`, `.claude/commands`, `<home>\commands`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandInfo {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argument_hint: Option<String>,
    /// File it was loaded from, or `builtin`.
    pub source: String,
}

/// An agent type for the `agent` tool (`explore`, `plan`, `general`, or from a file).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentTypeInfo {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Allowed tools; empty means the caller's tools minus `agent`.
    #[serde(default)]
    pub tools: Vec<String>,
    pub source: String,
}

/// One question from `ask_user`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Question {
    pub question: String,
    /// A short label for the modal's tab.
    #[serde(default)]
    pub header: String,
    #[serde(default)]
    pub options: Vec<String>,
    #[serde(default)]
    pub multi_select: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem {
    pub content: String,
    pub status: TodoStatus,
    /// Present-tense form shown while in progress.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_form: Option<String>,
}

/// A `/loop` or scheduled task (the daemon's `schedules` table).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduleInfo {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionId>,
    pub every_secs: u64,
    /// The prompt or slash command it runs.
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_at_ms: Option<i64>,
}

/// What `/rewind` restores.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RewindScope {
    Code,
    Conversation,
    Both,
}

/// A checkpoint in a session (one per turn that changed files).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointInfo {
    pub turn: u32,
    pub at_ms: i64,
    /// Files changed since the previous checkpoint.
    pub files: u32,
    #[serde(default)]
    pub summary: String,
}

/// Tokens and cost of a session (`/cost`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CostReport {
    pub calls: u32,
    pub usage: Usage,
    #[serde(default)]
    pub by_model: Vec<ModelCost>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelCost {
    pub model: ModelRef,
    pub calls: u32,
    pub usage: Usage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportFormat {
    Markdown,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExperimentStatus {
    Running,
    Finished,
    Failed,
    Killed,
}

/// A row of the experiments table (`train_launch`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExperimentInfo {
    pub id: String,
    pub command: String,
    pub run_dir: String,
    pub status: ExperimentStatus,
    pub started_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha: Option<String>,
    /// Latest metrics by name.
    #[serde(default)]
    pub metrics: Value,
    #[serde(default)]
    pub notes: String,
}

/// Per-ask overrides from the headless flags (design 2.7 H4: `--allowed-tools`,
/// `--disallowed-tools`, `--max-steps`, `--max-budget-usd`, `--append-system-prompt`,
/// `--permission-mode`). Track K sets them; the conductor applies them for that turn.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HeadlessOptions {
    /// Rule patterns allowed without asking (`bash(git diff*)`, `read`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_tools: Vec<String>,
    /// Rule patterns denied outright.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disallowed_tools: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_steps: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_budget_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub append_system_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<PermissionMode>,
}

/// A background job (B: `run_in_background`, Ctrl+B detach, the `!` shell).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobInfo {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<crate::SessionId>,
    pub command: String,
    pub started_ms: i64,
    /// `None` while it runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
}

/// A configured MCP server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerInfo {
    pub name: String,
    pub scope: SettingsScope,
    /// `stdio` or `http`.
    pub transport: String,
    #[serde(default)]
    pub tools: u32,
    /// `connected`, `failed: ...`, `needs auth`, `disabled`.
    pub status: String,
}
