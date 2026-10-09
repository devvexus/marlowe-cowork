//! Crew values on the wire: runs, dispatches, roles. Shared by the daemon, `marlowe-crew` and the
//! clients. Named `Crew*` where the Sentinel already owns the plain name (`RunInfo`, `RunStatus`
//! describe watched training runs).

use std::fmt;
use std::str::FromStr;

use marlowe_model::Effort;
use serde::{Deserialize, Serialize};

use crate::ids::SessionId;
use crate::request::Slot;

/// A crew run id (32 hex chars).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunId(pub String);

impl RunId {
    /// A fresh random id.
    pub fn new_random() -> Self {
        RunId(uuid::Uuid::new_v4().simple().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The first 8 chars, for display and branch names.
    pub fn short(&self) -> &str {
        self.0.get(..8).unwrap_or(&self.0)
    }
}

impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for RunId {
    fn from(s: &str) -> Self {
        RunId(s.to_string())
    }
}

/// Where a crew run is in its life.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CrewRunStatus {
    #[default]
    Queued,
    Running,
    /// Waiting on the user: an approval, a budget extension, or a merge.
    NeedsYou,
    Paused,
    Done,
    Failed,
    Cancelled,
}

impl CrewRunStatus {
    /// Done, Failed or Cancelled: the run will not step again.
    pub fn is_terminal(self) -> bool {
        matches!(self, CrewRunStatus::Done | CrewRunStatus::Failed | CrewRunStatus::Cancelled)
    }

    /// Stable lower-case word.
    pub fn as_str(self) -> &'static str {
        match self {
            CrewRunStatus::Queued => "queued",
            CrewRunStatus::Running => "running",
            CrewRunStatus::NeedsYou => "needs_you",
            CrewRunStatus::Paused => "paused",
            CrewRunStatus::Done => "done",
            CrewRunStatus::Failed => "failed",
            CrewRunStatus::Cancelled => "cancelled",
        }
    }
}

/// A fixed orchestration. A run without one is a single role-prompt dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Playbook {
    DeepResearch,
    Build,
}

impl Playbook {
    pub fn as_str(self) -> &'static str {
        match self {
            Playbook::DeepResearch => "deep_research",
            Playbook::Build => "build",
        }
    }
}

/// A string that is not a playbook.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown playbook '{0}' (deep_research, build)")]
pub struct PlaybookParseError(pub String);

impl FromStr for Playbook {
    type Err = PlaybookParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().replace(['-', ' '], "_").as_str() {
            "deep_research" | "research" => Ok(Playbook::DeepResearch),
            "build" => Ok(Playbook::Build),
            _ => Err(PlaybookParseError(s.to_string())),
        }
    }
}

/// What a run may write into.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxKind {
    /// A git worktree on branch `crew/<run-id>`.
    Worktree,
    /// A scratch directory under the profile home.
    #[default]
    Scratch,
    /// No writable directory.
    None,
}

/// A run's sandbox as clients see it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxInfo {
    pub kind: SandboxKind,
    /// The writable root (worktree or scratch dir).
    pub root: String,
    /// The original checkout a worktree was cut from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    /// `crew/<run-id>` for a worktree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

/// A run's limits. `None` means the role's default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct CrewBudget {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steps: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usd: Option<f64>,
}

impl CrewBudget {
    /// Fields set in `over` replace those in `self`.
    pub fn overlay(self, over: CrewBudget) -> CrewBudget {
        CrewBudget {
            tokens: over.tokens.or(self.tokens),
            steps: over.steps.or(self.steps),
            usd: over.usd.or(self.usd),
        }
    }

    /// The first dimension `spent` has reached: `"steps"`, `"tokens"` or `"usd"`.
    pub fn exceeded(&self, spent: &CrewSpent) -> Option<&'static str> {
        if self.steps.is_some_and(|m| spent.steps >= m) {
            return Some("steps");
        }
        if self.tokens.is_some_and(|m| spent.tokens >= m) {
            return Some("tokens");
        }
        if self.usd.is_some_and(|m| spent.usd >= m) {
            return Some("usd");
        }
        None
    }
}

/// What a run has used, summed over every agent step (scouts included).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct CrewSpent {
    #[serde(default)]
    pub tokens: u64,
    #[serde(default)]
    pub steps: u32,
    #[serde(default)]
    pub usd: f64,
}

/// How sure the crew is of a result.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    Low,
    #[default]
    Medium,
    High,
}

/// One source behind a result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Citation {
    pub title: String,
    /// A URL, an arXiv id URL, or a workspace path.
    pub url_or_path: String,
}

/// What a Build run changed on its branch, against the commit it was cut from.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffSummary {
    pub branch: String,
    /// Changed paths, relative to the repository root.
    #[serde(default)]
    pub files: Vec<String>,
    pub insertions: u32,
    pub deletions: u32,
}

/// A finished run's result: prose with provenance.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Dispatch {
    /// Outcome first, written for the conductor to relay.
    pub summary: String,
    /// Files the run produced (report, logs), absolute paths.
    #[serde(default)]
    pub artifacts: Vec<String>,
    #[serde(default)]
    pub confidence: Confidence,
    #[serde(default)]
    pub citations: Vec<Citation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<DiffSummary>,
}

/// One crew run, for the rail and the Run view.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CrewRunInfo {
    pub id: RunId,
    /// The conductor session that dispatched it; its inbox gets the result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionId>,
    pub role: String,
    /// Short display title, derived from the goal.
    pub title: String,
    pub goal: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playbook: Option<Playbook>,
    pub status: CrewRunStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<SandboxInfo>,
    pub budget: CrewBudget,
    pub spent: CrewSpent,
    /// Agent steps completed.
    pub step: u32,
    /// One line: `scout 2/4 · web_search "lora rank ablation"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_action: Option<String>,
    /// Why it needs the user, or why it failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Untrusted content has entered the run; its result relays tainted.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub tainted: bool,
    pub created_ms: i64,
    pub updated_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Dispatch>,
}

/// One stored step of a run, for the Run view's transcript replay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrewStep {
    pub step: u32,
    /// The role that took the step.
    pub role: String,
    /// One line: what the step did.
    pub action: String,
    /// The step's assistant text and tool summaries, as streamed.
    pub text: String,
    pub at_ms: i64,
}

/// A role card as clients see it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoleInfo {
    pub name: String,
    pub description: String,
    pub slot: Slot,
    pub effort: Effort,
    pub tools: Vec<String>,
    pub budget: CrewBudget,
    pub sandbox: SandboxKind,
    /// The user's `<home>\roles\<name>.md`, when that is where the card came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_file: Option<String>,
}
