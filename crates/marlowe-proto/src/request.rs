//! Client -> daemon requests.

use std::fmt;
use std::str::FromStr;

use marlowe_model::{Effort, ModelRef, ProviderKind};
use serde::{Deserialize, Serialize};

use crate::ids::{ApprovalId, SessionId};

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
}
