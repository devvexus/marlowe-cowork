//! Kind, scope, speaker and provenance tags on a belief.
//!
//! All optional on the wire and in the journal: an untagged entry (every eval write, every
//! pre-M2 profile) reads exactly as before, and its kind falls back to `payload_kind`.

use marlowe_contract::{PayloadKind, TrustClass};
use serde::{Deserialize, Serialize};

use crate::entry::MemoryId;

/// What a belief is, at the granularity a person thinks in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BeliefKind {
    Fact,
    Preference,
    Decision,
    Result,
    Note,
    Episode,
    Procedure,
}

impl BeliefKind {
    /// The contract kind recorded alongside the tag. Total, no default arm.
    pub fn payload_kind(self) -> PayloadKind {
        match self {
            BeliefKind::Fact => PayloadKind::Fact,
            BeliefKind::Preference => PayloadKind::Fact,
            BeliefKind::Decision => PayloadKind::Commitment,
            BeliefKind::Result => PayloadKind::Fact,
            BeliefKind::Note => PayloadKind::Noticing,
            BeliefKind::Episode => PayloadKind::Episode,
            BeliefKind::Procedure => PayloadKind::Procedure,
        }
    }

    /// The kind an untagged entry reads as. Total, no default arm.
    pub fn from_payload_kind(kind: PayloadKind) -> BeliefKind {
        match kind {
            PayloadKind::Episode => BeliefKind::Episode,
            PayloadKind::Fact => BeliefKind::Fact,
            PayloadKind::Entity => BeliefKind::Fact,
            PayloadKind::Edge => BeliefKind::Fact,
            PayloadKind::Procedure => BeliefKind::Procedure,
            PayloadKind::Commitment => BeliefKind::Decision,
            PayloadKind::Person => BeliefKind::Fact,
            PayloadKind::Relationship => BeliefKind::Fact,
            PayloadKind::VoiceParams => BeliefKind::Preference,
            PayloadKind::Noticing => BeliefKind::Note,
        }
    }
}

/// Where a belief applies: everywhere, or inside one project root.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BeliefScope {
    #[default]
    Global,
    Project(String),
}

impl BeliefScope {
    pub fn is_global(&self) -> bool {
        matches!(self, BeliefScope::Global)
    }

    /// Whether a belief with this scope belongs in a turn running in `workspace`.
    /// Global always does; a project belief only inside the same root.
    pub fn admits(&self, workspace: Option<&str>) -> bool {
        match self {
            BeliefScope::Global => true,
            BeliefScope::Project(root) => workspace.is_some_and(|w| same_root(root, w)),
        }
    }
}

/// Path equality for workspace roots: separators unified, trailing separator and the Windows
/// verbatim prefix dropped, case folded on Windows.
pub fn same_root(a: &str, b: &str) -> bool {
    normalize_root(a) == normalize_root(b)
}

fn normalize_root(path: &str) -> String {
    let path = path.strip_prefix(r"\\?\").unwrap_or(path);
    let mut out: String = path.chars().map(|c| if c == '\\' { '/' } else { c }).collect();
    while out.len() > 1 && out.ends_with('/') {
        out.pop();
    }
    if cfg!(windows) {
        out.make_ascii_lowercase();
    }
    out
}

/// Who spoke a conversational turn that was stored verbatim.
///
/// `Assistant` marks the model's own prose. It stays recallable but is never an
/// auto-injection candidate: the model's words are not facts (see
/// [`crate::entry::MemoryEntry::is_injection_candidate`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Utterance {
    User,
    Assistant,
}

/// The optional tags carried on a [`crate::entry::MemoryEntry`].
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Tags {
    /// `None` on untagged entries; read through [`crate::entry::MemoryEntry::kind`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<BeliefKind>,
    #[serde(default, skip_serializing_if = "BeliefScope::is_global")]
    pub scope: BeliefScope,
    /// Set when the text is a verbatim conversation turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub utterance: Option<Utterance>,
    /// Free-form origin note shown to the user, e.g. "imported from Marlowe_Harness".
    /// Display only: trust never reads it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Why, for a decision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl Tags {
    pub fn is_empty(&self) -> bool {
        *self == Tags::default()
    }
}

/// Everything known about where a belief came from. Borrowed from the entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provenance<'a> {
    pub session_id: &'a str,
    pub turn_id: &'a str,
    /// Own class, before propagation.
    pub trust_class: TrustClass,
    /// After worst-case propagation over `derivation`. The one that decides.
    pub effective_trust: TrustClass,
    pub derivation: &'a [MemoryId],
    /// Journal seq of the `MemoryWritten` event.
    pub origin_event: u64,
    pub occurred_at_ms: i64,
    pub created_at: i64,
    pub utterance: Option<Utterance>,
    pub source: Option<&'a str>,
}

impl Provenance<'_> {
    /// A short human label: the source note when there is one, else who authored it.
    pub fn describe(&self) -> String {
        let who = match (self.source, self.utterance) {
            (Some(note), _) => note.to_string(),
            (None, Some(Utterance::User)) => format!("you said it (session {})", self.session_id),
            (None, Some(Utterance::Assistant)) => {
                format!("assistant reply (session {})", self.session_id)
            }
            (None, None) => match self.trust_class {
                TrustClass::UserAsserted => format!("you (session {})", self.session_id),
                TrustClass::AgentObserved => format!("harness (session {})", self.session_id),
                TrustClass::AgentInferred => format!("agent (session {})", self.session_id),
                TrustClass::UntrustedContent => {
                    format!("external content (session {})", self.session_id)
                }
            },
        };
        if self.derivation.is_empty() {
            who
        } else {
            format!("{who}, derived from {} memories", self.derivation.len())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_round_trips_through_its_payload_kind_where_the_mapping_is_one_to_one() {
        for kind in [BeliefKind::Fact, BeliefKind::Decision, BeliefKind::Note, BeliefKind::Episode, BeliefKind::Procedure] {
            assert_eq!(BeliefKind::from_payload_kind(kind.payload_kind()), kind);
        }
        // Preference and Result share Fact's payload kind; the tag is what keeps them apart.
        assert_eq!(BeliefKind::Preference.payload_kind(), PayloadKind::Fact);
        assert_eq!(BeliefKind::Result.payload_kind(), PayloadKind::Fact);
    }

    #[test]
    fn scope_admits_global_everywhere_and_projects_only_at_home() {
        let p = BeliefScope::Project(r"C:\Users\m\proj\".into());
        assert!(BeliefScope::Global.admits(None));
        assert!(BeliefScope::Global.admits(Some("anything")));
        assert!(!p.admits(None));
        assert!(p.admits(Some("C:/Users/m/proj")));
        assert!(p.admits(Some(r"\\?\C:\Users\m\proj")));
        assert!(!p.admits(Some(r"C:\Users\m\other")));
        assert!(!p.admits(Some(r"C:\Users\m\proj2")));
    }

    #[test]
    fn empty_tags_serialize_to_nothing() {
        assert_eq!(serde_json::to_string(&Tags::default()).unwrap(), "{}");
        let t: Tags = serde_json::from_str("{}").unwrap();
        assert!(t.is_empty());
        let scoped = Tags { scope: BeliefScope::Project("/w".into()), ..Tags::default() };
        assert_eq!(serde_json::to_string(&scoped).unwrap(), r#"{"scope":{"project":"/w"}}"#);
    }
}
