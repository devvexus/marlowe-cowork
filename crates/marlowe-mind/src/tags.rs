//! Conversions between the core's belief tags and the wire types, and recall scope filters.

use marlowe_memory::{BeliefKind, BeliefScope, MemoryEntry};
use marlowe_proto::{MemoryKind, MemoryScope};

pub(crate) fn wire_kind(k: BeliefKind) -> MemoryKind {
    match k {
        BeliefKind::Fact => MemoryKind::Fact,
        BeliefKind::Preference => MemoryKind::Preference,
        BeliefKind::Decision => MemoryKind::Decision,
        BeliefKind::Result => MemoryKind::Result,
        BeliefKind::Note => MemoryKind::Note,
        BeliefKind::Episode => MemoryKind::Episode,
        BeliefKind::Procedure => MemoryKind::Procedure,
    }
}

pub(crate) fn belief_kind(k: MemoryKind) -> BeliefKind {
    match k {
        MemoryKind::Fact => BeliefKind::Fact,
        MemoryKind::Preference => BeliefKind::Preference,
        MemoryKind::Decision => BeliefKind::Decision,
        MemoryKind::Result => BeliefKind::Result,
        MemoryKind::Note => BeliefKind::Note,
        MemoryKind::Episode => BeliefKind::Episode,
        MemoryKind::Procedure => BeliefKind::Procedure,
    }
}

pub(crate) fn wire_scope(s: &BeliefScope) -> MemoryScope {
    match s {
        BeliefScope::Global => MemoryScope::Global,
        BeliefScope::Project(p) => MemoryScope::Project(p.clone()),
    }
}

pub(crate) fn belief_scope(s: &MemoryScope) -> BeliefScope {
    match s {
        MemoryScope::Global => BeliefScope::Global,
        MemoryScope::Project(p) => BeliefScope::Project(p.clone()),
    }
}

/// A session summary written by `write_episode` (not assistant prose, which is also Episode kind).
pub(crate) fn is_episode_summary(e: &MemoryEntry) -> bool {
    e.kind() == BeliefKind::Episode && e.tags.utterance.is_none()
}

/// Which scopes an explicit search sees: `None` everything, `Global` only global,
/// `Project(p)` that project plus global.
pub(crate) fn recall_admits(filter: Option<&MemoryScope>, e: &MemoryEntry) -> bool {
    match filter {
        None => true,
        Some(MemoryScope::Global) => e.scope().is_global(),
        Some(MemoryScope::Project(p)) => e.scope().admits(Some(p)),
    }
}

/// First 8 characters of a session id, for provenance text.
pub(crate) fn short(session: &str) -> &str {
    match session.char_indices().nth(8) {
        Some((i, _)) => &session[..i],
        None => session,
    }
}
