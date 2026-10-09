//! Memory values shared by the wire, the daemon and the memory service (`marlowe-mind`).

use serde::{Deserialize, Serialize};

/// What a memory is. A tag only; a decision's reason lives in its text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    Fact,
    Preference,
    Decision,
    Result,
    Note,
    /// A session summary or captured assistant prose. Never auto-injected as a fact.
    Episode,
    Procedure,
}

impl MemoryKind {
    pub const ALL: [MemoryKind; 7] = [
        MemoryKind::Fact,
        MemoryKind::Preference,
        MemoryKind::Decision,
        MemoryKind::Result,
        MemoryKind::Note,
        MemoryKind::Episode,
        MemoryKind::Procedure,
    ];

    /// Lower-case label for the UI and the tool schema.
    pub fn label(self) -> &'static str {
        match self {
            MemoryKind::Fact => "fact",
            MemoryKind::Preference => "preference",
            MemoryKind::Decision => "decision",
            MemoryKind::Result => "result",
            MemoryKind::Note => "note",
            MemoryKind::Episode => "episode",
            MemoryKind::Procedure => "procedure",
        }
    }

    /// Parses a label (case-insensitive).
    pub fn from_label(s: &str) -> Option<MemoryKind> {
        let s = s.trim();
        MemoryKind::ALL.into_iter().find(|k| k.label().eq_ignore_ascii_case(s))
    }
}

/// Where a memory applies. Serialized as `"global"` or `{"project":"<path>"}`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryScope {
    Global,
    /// A workspace root, as the session records it.
    Project(String),
}

/// A memory's effective trust after worst-case propagation (mirrors `marlowe_contract::TrustClass`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryTrust {
    /// Web, mail, MCP output, or anything derived from it.
    Untrusted,
    /// The model concluded it.
    Inferred,
    /// The harness computed it.
    Observed,
    /// The user said it.
    User,
}

impl MemoryTrust {
    /// Short label for the UI.
    pub fn label(self) -> &'static str {
        match self {
            MemoryTrust::Untrusted => "untrusted",
            MemoryTrust::Inferred => "inferred",
            MemoryTrust::Observed => "observed",
            MemoryTrust::User => "you",
        }
    }
}

/// One retrieved memory.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryHit {
    pub id: String,
    pub text: String,
    pub kind: MemoryKind,
    pub trust: MemoryTrust,
    pub scope: MemoryScope,
    /// When the underlying utterance happened, Unix epoch ms.
    pub occurred_at_ms: i64,
    /// Human-readable origin, e.g. `you, session 3f2a…` or `imported from Marlowe_Harness`.
    pub provenance: String,
    /// Retrieval score; comparable only within one result list.
    pub score: f32,
}

/// What happened to a memory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MemoryChange {
    Remembered,
    /// Superseded by the memory `by`.
    Corrected { by: String },
    /// Tombstoned.
    Forgotten,
}

/// Memory service health.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryStatsReport {
    /// Live beliefs (not tombstoned, not superseded).
    pub beliefs: u64,
    pub episodes: u64,
    pub vectors: u64,
    /// Names of the ONNX models currently resident.
    #[serde(default)]
    pub models_loaded: Vec<String>,
    /// Why the service is lexical-only and abstains from auto-injection; `None` when healthy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degraded: Option<String>,
    /// Last journal sequence number.
    pub journal_seq: u64,
}

/// Outcome of importing the old harness profile.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportReport {
    /// Memory events plus distinct conversation turns read from the old journal.
    pub read: u64,
    /// Beliefs written, conversation turns included.
    pub imported: u64,
    /// Already present (same content digest); makes re-import a no-op.
    pub skipped_duplicates: u64,
    pub tombstoned: u64,
    pub superseded: u64,
    #[serde(default)]
    pub errors: Vec<String>,
    /// Distinct non-empty conversation texts found in the old checkpoints.
    #[serde(default)]
    pub conversation_turns_read: u64,
    /// Conversation turns written (user + assistant).
    #[serde(default)]
    pub conversation_turns_imported: u64,
    /// Of those, the user's own turns (user trust).
    #[serde(default)]
    pub conversation_user_turns: u64,
    /// Of the user turns, questions and requests stored as episodes (never injected as facts).
    #[serde(default)]
    pub conversation_user_requests: u64,
    /// Of those, assistant prose (episodes, never injected).
    #[serde(default)]
    pub conversation_assistant_turns: u64,
    /// Skipped as machine payloads (tool-result JSON, harness status lines, skill banners).
    #[serde(default)]
    pub conversation_skipped: u64,
}
