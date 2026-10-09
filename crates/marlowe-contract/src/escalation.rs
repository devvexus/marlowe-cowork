//! The escalation record's leaf types — M3-DESIGN §3, ADR-065 §2.6.
//!
//! Everything here is **shared by the loop, the daemon, the view and the surface**, which is why
//! it lives in the deepest crate in the workspace rather than in any one of them. A second
//! definition of `EscalationSeverity` in `marlowe-view` and a first in `marlowe-loop` would be two
//! answers to one question on the channel M3-DESIGN §3 calls *"the single most dangerous channel
//! in the system"*.
//!
//! # What a model may supply, and what it may not
//!
//! A model supplies a **severity**, a **category**, an **artifact handle**, up to four **option
//! labels**, and — only under §9.1 A8's middle arm — one **sentence**. It supplies no recipient
//! (that is `marlowe_loop::escalation::escalation_route`, which reads the run tree), no
//! termination cost (that is a journal replay), and no rendering.
//!
//! [`OptionLabel`] and [`ValidatedSentence`] are the two model-authored strings, and both are
//! newtypes with a validating constructor **and a hand-written `Deserialize` that routes through
//! it** — CLAUDE.md instance #12. A checkpoint, an MCP descriptor and a spawn request are all ways
//! in, and `#[derive(Deserialize)]` is a field-wise way past `normalise` that leaves every in-code
//! test green.
//!
//! # `is_renderable` is NOT re-checked after `sanitize_line`, and ADR-065 §2.6 said it should be
//!
//! The ADR writes `normalise` as *sanitize, then refuse if any char fails `is_renderable`*.
//! **That second check can never fail**, because [`crate::text::sanitize`] does not drop a refused
//! character — it substitutes `<U+001B>`, seven renderable ASCII characters. So a
//! `TextRejected::Unrenderable` variant would be a declared control whose reader can never see it
//! true: CLAUDE.md instance #16, in the file that exists to enforce #12. It is not built, and this
//! paragraph is why.
//!
//! What survives is the bound, and the bound is measured on the **sanitized** string. That order
//! is load-bearing in the other direction too: 200 escape characters expand to 1,400, and a cap
//! applied before substitution would admit a label seven times longer than the row it has to fit.
//! **Refused, never truncated** — a truncated label is a label whose meaning the harness changed.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::text::sanitize_line;

/// The identity of one escalation, **derived rather than drawn**.
///
/// A `Uuid::new_v4` here would be unreproducible from the journal, and M3's whole subject is
/// durable runs: `EscalationDesk::from_journal` has to rebuild the pending set from the event log
/// after a restart, and it can only do that if the id is a function of what the event records.
/// So it is a v5 name UUID over the raising run and the raise's sequence number, which are both
/// in the event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EscalationId(pub Uuid);

impl EscalationId {
    /// The one constructor. `raised_by` is the run's `Uuid` — this crate cannot name `RunId`,
    /// which is `marlowe-loop`'s, and taking the inner value keeps the derivation reproducible
    /// from the journal row rather than from a `Display` impl that could be restyled.
    pub fn for_raise(raised_by: Uuid, seq: u32) -> Self {
        let mut name = raised_by.as_bytes().to_vec();
        name.extend_from_slice(&seq.to_be_bytes());
        Self(Uuid::new_v5(&Uuid::NAMESPACE_OID, &name))
    }
}

impl std::fmt::Display for EscalationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// How loudly the raiser is asking. **A model chooses its own, and that is deliberate.**
///
/// Severity selects rendering and ordering. It never selects a capability and it never selects a
/// recipient — the recipient is `escalation_route`, which reads the tree. A model that inflates
/// every escalation to `Critical` costs a human's attention and nothing else, which is what
/// M3-DESIGN §11's *"escalations reaching the user per project-hour, with false-escalation rate"*
/// measures. **Nothing may ever gate a permission on this value.**
///
/// Named `EscalationSeverity` rather than `Severity` because `marlowe_loop::driver::Urgency` is
/// `{Advisory, Immediate}` in the same crate graph, and two enums sharing the variant name
/// `Advisory` with different meanings is the collision the adversarial pass found three of.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EscalationSeverity {
    Advisory,
    Blocking,
    Critical,
}

impl EscalationSeverity {
    /// The harness's word for this severity. **Harness-authored**, so it can be composed into an
    /// [`crate::escalation::EscalationSeverity`]-carrying notice without any model byte reaching
    /// it. Read by `Notice::EscalationRaised`'s producer and by the overlay's header row.
    pub fn word(self) -> &'static str {
        match self {
            Self::Advisory => "advisory",
            Self::Blocking => "blocking",
            Self::Critical => "critical",
        }
    }
}

/// Why the raiser is stuck. **Closed**, on ADR-030 §5's growth rule: a variant is added when a
/// producer must say something the set cannot express, never to carry a string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EscalationCategory {
    BlockedByPermission,
    ScopeContradiction,
    ExternalSystemRefused,
    ConflictingInstructions,
    SuspectedInjection,
    IrreversibleActionRequired,
}

impl EscalationCategory {
    /// Read by the overlay's category row and by `EscalationDesk`'s journal payload.
    pub fn word(self) -> &'static str {
        match self {
            Self::BlockedByPermission => "blocked by permission",
            Self::ScopeContradiction => "scope contradiction",
            Self::ExternalSystemRefused => "external system refused",
            Self::ConflictingInstructions => "conflicting instructions",
            Self::SuspectedInjection => "suspected injection",
            Self::IrreversibleActionRequired => "irreversible action required",
        }
    }
}

/// Why a model-authored string was refused. **One variant**, and the module header says why there
/// is not a second.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TextRejected {
    #[error(
        "the text is empty after normalisation. A blank option label is a row a human cannot \
         choose between, and a blank sentence is a field that reads as though nothing was said"
    )]
    Empty,
    #[error(
        "the text is {chars} characters after normalisation, and the cap is {max}. Refused rather \
         than truncated: truncating is the harness changing what the raiser said, on the surface \
         where a human is about to choose"
    )]
    TooLong { chars: usize, max: usize },
}

/// A model-authored sentence. §9.1 A8's **middle arm only** — arm (a), the shipped default,
/// carries `None` and the loop refuses `Some` by name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct ValidatedSentence(String);

impl ValidatedSentence {
    pub const MAX_CHARS: usize = 200;

    /// The only way to build one. See the module header for why there is no renderability check
    /// after the substitution, and why the cap is measured on the substituted string.
    pub fn normalise(raw: &str) -> Result<Self, TextRejected> {
        Ok(Self(normalise_into(raw, Self::MAX_CHARS)?))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One row in the attacker-controlled half of the option list — M3-DESIGN §3.4.
///
/// 72 characters, because the overlay draws it on one row beside a harness-drawn index and the
/// terminate row is laid out first: a label that cannot fit is refused at construction rather than
/// clipped by `ratatui` at draw time, which is SECURITY-AUDIT B3's defect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct OptionLabel(String);

impl OptionLabel {
    pub const MAX_CHARS: usize = 72;

    pub fn normalise(raw: &str) -> Result<Self, TextRejected> {
        Ok(Self(normalise_into(raw, Self::MAX_CHARS)?))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn normalise_into(raw: &str, max: usize) -> Result<String, TextRejected> {
    // `Shape::Line`, not `Shape::Prose`. A newline in an option label writes a second, fraudulent
    // row underneath the real one, and a human answering the wrong row has chosen something they
    // never saw — `text.rs`'s own account of why the line shape exists.
    let one = sanitize_line(raw.trim());
    let one = one.trim();
    if one.is_empty() {
        return Err(TextRejected::Empty);
    }
    let chars = one.chars().count();
    if chars > max {
        return Err(TextRejected::TooLong { chars, max });
    }
    Ok(one.to_string())
}

/// **Hand-written, on `GovernanceConstraint`'s model.** `#[derive(Deserialize)]` on a
/// `#[serde(transparent)]` newtype is a field-wise way in past [`ValidatedSentence::normalise`],
/// and a checkpoint is outside input.
impl<'de> Deserialize<'de> for ValidatedSentence {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Self::normalise(&raw).map_err(serde::de::Error::custom)
    }
}

impl<'de> Deserialize<'de> for OptionLabel {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Self::normalise(&raw).map_err(serde::de::Error::custom)
    }
}

/// A journal-addressed handle to something the raiser produced. **Hash only.**
///
/// # Why this is not a `ContentRef`
///
/// `CONTRACTS.md` §2 pins `ContentRef` as `{ hash, bytes, media, summary: ResultSummary, trust,
/// evicted }`, and it **carries a summary** — so attacker-shaped prose would cross upward inside
/// the record §2.3 calls typed, without anyone dereferencing anything. It is also not a Rust type:
/// `grep -rn "ContentRef" --include=*.rs crates/` returns doc comments only, and there is no
/// `ContentHash` either. `ArtifactHandle(pub ContentHash)` names two types that do not exist.
///
/// The precedent that does exist is `marlowe-extract`'s `DocumentRef`, whose `hash` is a `String`
/// — *"Hex of a 128-bit hash over the extracted text"* — and whose module header is the argument
/// in full: *"No title. No headings. No description. No snippet. Every one of those is
/// attacker-authored text and putting any of them on the reference would quietly restore the
/// thing this removes."*
///
/// The surface composes the path a user opens from the handle. The record carries no text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct ArtifactHandle(String);

/// Why a handle was refused. Separate from [`TextRejected`] because a handle is not prose: the
/// only thing wrong with one is that it is not an address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error(
    "an artifact handle must be exactly {expected} lowercase hex characters — a 128-bit content \
     address, on `DocumentRef`'s width. Anything else is a string a model chose, and a string a \
     model chose is not an address"
)]
pub struct NotAHandle {
    pub expected: usize,
}

impl ArtifactHandle {
    /// 128 bits, matching `marlowe_extract::DocumentRef::hash`. **Confirmed against the store
    /// rather than inherited from ADR-065's sentence**, which flagged the width as unverified.
    pub const HEX_CHARS: usize = 32;

    pub fn parse(raw: &str) -> Result<Self, NotAHandle> {
        if raw.len() == Self::HEX_CHARS
            && raw.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            Ok(Self(raw.to_string()))
        } else {
            Err(NotAHandle { expected: Self::HEX_CHARS })
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for ArtifactHandle {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reachability claim in the module header, asserted rather than argued: after
    /// `sanitize_line`, every character is renderable, so a post-substitution `is_renderable`
    /// check is a control whose reader can never see it false.
    #[test]
    fn sanitize_line_leaves_nothing_for_a_renderability_check_to_find() {
        for hostile in ["a\u{1b}[2K\rb", "a\u{202e}b", "a\u{2028}b", "a\u{200b}b", "a\u{e0001}b"] {
            let out = sanitize_line(hostile);
            assert!(
                out.chars().all(crate::text::is_renderable),
                "{hostile:?} sanitized to {out:?}, which still fails is_renderable — the module \
                 header's reason for not building `TextRejected::Unrenderable` is wrong"
            );
        }
    }

    #[test]
    fn the_cap_is_measured_after_substitution_not_before() {
        // 30 escapes is 30 characters raw and 210 after substitution. A cap applied to the raw
        // string would admit a label three times the width of the row it has to fit.
        let raw = "\u{1b}".repeat(30);
        assert_eq!(raw.chars().count(), 30);
        assert!(matches!(
            OptionLabel::normalise(&raw),
            Err(TextRejected::TooLong { chars: 210, max: 72 })
        ));
    }

    #[test]
    fn an_id_is_reproducible_from_what_the_journal_records() {
        let run = Uuid::new_v5(&Uuid::NAMESPACE_OID, b"a-run");
        assert_eq!(EscalationId::for_raise(run, 3), EscalationId::for_raise(run, 3));
        assert_ne!(EscalationId::for_raise(run, 3), EscalationId::for_raise(run, 4));
    }

    #[test]
    fn a_handle_is_an_address_or_it_is_refused() {
        assert!(ArtifactHandle::parse("0123456789abcdef0123456789abcdef").is_ok());
        assert!(ArtifactHandle::parse("0123456789ABCDEF0123456789ABCDEF").is_err());
        assert!(ArtifactHandle::parse("../../etc/passwd").is_err());
        assert!(ArtifactHandle::parse("0123456789abcdef").is_err());
    }
}
