//! Client -> daemon requests.

use std::fmt;
use std::str::FromStr;

use marlowe_model::{Effort, ModelRef, ProviderKind};
use serde::{Deserialize, Serialize};

use crate::context::{ContextPolicy, KvType};
use crate::crew::{CrewBudget, Playbook, RunId};
use crate::ids::{ApprovalId, SessionId};
use crate::memory::MemoryScope;
use crate::research::PlanDecision;

/// A secret on the wire. `Debug` and `Display` never print it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(pub String);

impl Secret {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***)")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

/// A user's answer to an approval request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    Allow,
    /// Allow this tool for the rest of the session without asking.
    AllowForSession,
    Deny,
}

impl ApprovalDecision {
    pub fn is_allowed(self) -> bool {
        !matches!(self, ApprovalDecision::Deny)
    }
}

/// A model slot. Serialized as `conductor`, `crew:<role>`, `reflection`, `quick`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum Slot {
    Conductor,
    Crew(String),
    Reflection,
    Quick,
}

impl Slot {
    /// The fixed slots shown in the library table (crew roles are added per role).
    pub const FIXED: [Slot; 3] = [Slot::Conductor, Slot::Reflection, Slot::Quick];

    /// Human-facing label.
    pub fn label(&self) -> String {
        match self {
            Slot::Conductor => "Conductor".into(),
            Slot::Crew(role) => format!("Crew · {role}"),
            Slot::Reflection => "Reflection".into(),
            Slot::Quick => "Quick".into(),
        }
    }
}

/// A string that is not a slot.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown slot '{0}' (conductor, crew:<role>, reflection, quick)")]
pub struct SlotParseError(pub String);

impl fmt::Display for Slot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Slot::Conductor => f.write_str("conductor"),
            Slot::Crew(role) => write!(f, "crew:{role}"),
            Slot::Reflection => f.write_str("reflection"),
            Slot::Quick => f.write_str("quick"),
        }
    }
}

impl FromStr for Slot {
    type Err = SlotParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let t = s.trim();
        match t.to_ascii_lowercase().as_str() {
            "conductor" => return Ok(Slot::Conductor),
            "reflection" => return Ok(Slot::Reflection),
            "quick" => return Ok(Slot::Quick),
            _ => {}
        }
        match t.split_once(':') {
            Some((k, role)) if k.eq_ignore_ascii_case("crew") && !role.trim().is_empty() => {
                Ok(Slot::Crew(role.trim().to_string()))
            }
            _ => Err(SlotParseError(s.to_string())),
        }
    }
}

impl TryFrom<String> for Slot {
    type Error = SlotParseError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<Slot> for String {
    fn from(s: Slot) -> String {
        s.to_string()
    }
}

/// A client request. Tagged by `op`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Must be the first line. `since_seq` replays buffered events after that seq.
    Hello {
        token: String,
        /// Client name, e.g. `tui` or `cli`.
        client: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        since_seq: Option<u64>,
    },
    /// Sends user text. No session starts a new one; a busy session queues it as a steer.
    Ask {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<SessionId>,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace: Option<String>,
    },
    /// Cancels the session's running turn.
    Interrupt { session: SessionId },
    ListSessions { limit: u32 },
    /// Replays a session's transcript to this client.
    OpenSession { session: SessionId },
    NewSession {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace: Option<String>,
    },
    Approve { approval: ApprovalId, decision: ApprovalDecision },
    LibraryState,
    DetectProviders,
    ListCatalog { provider: ProviderKind },
    AddModel { provider: ProviderKind, model_id: String },
    RemoveModel { model: ModelRef },
    /// `model: None` clears the slot back to the default.
    SetSlot {
        slot: Slot,
        #[serde(default)]
        model: Option<ModelRef>,
        #[serde(default)]
        effort: Effort,
    },
    /// Stores an API key in the credential store. Never logged.
    SetCredential { provider: ProviderKind, secret: Secret },
    Status,
    Shutdown,
    /// Searches memory. `scope: None` searches everything; a project scope includes global memories.
    MemorySearch {
        query: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scope: Option<MemoryScope>,
        limit: u32,
    },
    /// Tombstones a memory.
    MemoryForget { id: String },
    /// Supersedes a memory with corrected text, at user trust.
    MemoryCorrect { id: String, text: String },
    MemoryStats,
    /// Imports the old harness profile. `path: None` uses the default old profile location.
    ImportLegacy {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
    },
    /// Asks whether an old profile exists; answered with `LegacyAvailable` or an `Error`.
    LegacyStatus,
    /// Runs the auto-injection pipeline for `query` outside a turn (diagnostics, `memory bench`).
    /// Answered with `MemoryProbed`; the decision is journalled like any injection.
    MemoryProbe {
        query: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace: Option<String>,
    },
    /// What the profile block holds for `workspace` now; answered with `MemoryProfile`.
    MemoryProfile {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace: Option<String>,
    },
    /// Composes the brief now (`marlowe brief`); answered with `Brief` to this client only.
    Brief,
    /// Answered with `WatchState`.
    WatchList,
    /// Adds a watch root (explicit consent); answered with `WatchState`.
    WatchAdd { path: String },
    /// Removes a watch root; answered with `WatchState`.
    WatchRemove { path: String },
    /// Runs discovery; answered with `WatchCandidates`. Never adds anything.
    WatchCandidates,
    /// Lists notices, newest first, with `seen` for this client; answered with `Notices`.
    ListNotices {
        limit: u32,
        #[serde(default)]
        unseen_only: bool,
    },
    /// Marks a notice seen for this client.
    NoticeSeen { id: String },
    /// Speaks `text` (subject to voice being enabled; quiet hours do not apply to explicit asks).
    Speak { text: String },
    /// Barge-in: stops speaking now.
    StopSpeaking,
    /// Changes voice settings; `None` leaves a field as it is. Answered with `VoiceState`, so
    /// `VoiceSettings { enabled: None, voice: None }` is a query.
    VoiceSettings {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        enabled: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        voice: Option<String>,
    },
    /// Sets the user's name and address form; both are replaced and `None` clears one. The
    /// daemon writes `user.json` in the profile home and broadcasts `User`.
    SetUser {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        address: Option<String>,
    },
    /// Starts a crew run; answered with `RunUpdated` (status `queued`) right away. `session` is
    /// the conductor session whose inbox gets the result (`None`: the result is only broadcast).
    Dispatch {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<SessionId>,
        role: String,
        goal: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        playbook: Option<Playbook>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        budget: Option<CrewBudget>,
    },
    /// Answered with `Runs`, newest first.
    ListRuns { limit: u32 },
    /// Answered with `RunTranscript` to this client; live steps arrive as `RunStep` broadcasts.
    WatchRun { run: RunId },
    /// Guidance for a run, picked up at its next step.
    Steer { run: RunId, text: String },
    CancelRun { run: RunId },
    PauseRun { run: RunId },
    ResumeRun { run: RunId },
    /// Build: merges `crew/<run-id>` into the original checkout's current branch (never pushes)
    /// and removes the worktree. Research: copies the report into `<workspace>\research\<slug>.md`. Goes
    /// through an approval first.
    MergeRun { run: RunId },
    /// Removes the run's worktree and branch (or scratch dir) without merging.
    DiscardRun { run: RunId },
    /// Answered with `Roles`.
    ListRoles,
    /// Answers a deep research run's `PlanProposed`: approve it, run an edited plan instead, or
    /// reject it (the run ends cancelled). Answered with `RunUpdated`.
    ApprovePlan { run: RunId, decision: PlanDecision },
    /// The context and hardware plan for `model`; answered with `ContextPlan` to this client.
    ContextPlan { model: ModelRef },
    /// Every library model's context policy and engine state; answered with `ContextPolicies`.
    ContextPolicies,
    /// Saves `model`'s policy. Answered with `ContextPolicySaved` (broadcast), listing what
    /// changed now and what waits for a server restart. Refused above the model's maximum.
    SetContextPolicy { model: ModelRef, policy: ContextPolicy },
    /// Changes Ollama's server-wide settings and restarts it. Always goes through an approval
    /// (`SERVER_RESTART_TOOL`) that says what restarts; `None` leaves a setting as it is.
    ApplyOllamaServer {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kv_type: Option<KvType>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parallel: Option<u16>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        flash_attn: Option<bool>,
    },
}
