//! The profile: what was deliberately recorded about the user, rendered for context every turn.
//!
//! Short personal facts score poorly on the cross-encoder (admission-floor study), so a young
//! store cannot rely on retrieval to surface them. What the conductor or the user deliberately
//! recorded lives in context instead; retrieval serves episodes, imports and the long tail.

use marlowe_contract::TrustClass;
use marlowe_memory::{BeliefKind, MemoryEntry};

use crate::actor::is_live;
use crate::select::civil;
use crate::{legacy, LEGACY_PROVENANCE};

/// Default token budget of the profile block.
pub const DEFAULT_PROFILE_TOKENS: u32 = 1_500;

/// Longest single profile line, in characters, before it is clipped.
const MAX_LINE_CHARS: usize = 400;

const OPEN: &str = "<profile>";
const CLOSE: &str = "</profile>";

/// A request for the profile block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileQuery {
    /// The session's workspace; Project items of other workspaces are left out.
    pub workspace: Option<String>,
    /// 0 means [`DEFAULT_PROFILE_TOKENS`].
    pub budget_tokens: u32,
}

impl Default for ProfileQuery {
    fn default() -> Self {
        Self { workspace: None, budget_tokens: DEFAULT_PROFILE_TOKENS }
    }
}

/// The rendered profile. Byte-identical for the same store, workspace and budget.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Profile {
    /// Empty when nothing qualifies.
    pub block: String,
    /// The memories in `block`, in order.
    pub ids: Vec<String>,
    /// Estimated tokens of `block`.
    pub tokens: u32,
    /// Members left out by the budget.
    pub omitted: u32,
}

/// Rough token estimate, the context assembler's (about three characters per token).
pub(crate) fn estimate_tokens(text: &str) -> u32 {
    (text.chars().count() as u32).div_ceil(3)
}

/// Order of kinds in the block; `None` for kinds that never enter it.
fn kind_rank(k: BeliefKind) -> Option<u8> {
    match k {
        BeliefKind::Preference => Some(0),
        BeliefKind::Decision => Some(1),
        BeliefKind::Fact => Some(2),
        BeliefKind::Procedure => Some(3),
        BeliefKind::Result | BeliefKind::Note | BeliefKind::Episode => None,
    }
}

/// A deliberate record (remember, a correction, the harness) rather than a conversation turn,
/// an import or a summary.
pub(crate) fn is_deliberate(e: &MemoryEntry) -> bool {
    e.tags.utterance.is_none()
        && e.source_session_id != legacy::SESSION
        && !e.tags.source.as_deref().is_some_and(|s| s.starts_with(LEGACY_PROVENANCE))
}

/// Whether `e` belongs in the profile for `workspace` at `now_ms`.
pub(crate) fn is_member(e: &MemoryEntry, workspace: Option<&str>, now_ms: i64) -> bool {
    is_live(e)
        && kind_rank(e.kind()).is_some()
        && is_deliberate(e)
        && e.effective_trust > TrustClass::UntrustedContent
        && e.is_matured(now_ms)
        && e.scope().admits(workspace)
}

/// Builds the profile from `entries` (any order).
pub(crate) fn build<'a>(entries: impl Iterator<Item = &'a MemoryEntry>, q: &ProfileQuery, now_ms: i64) -> Profile {
    let workspace = q.workspace.as_deref().map(str::trim).filter(|w| !w.is_empty());
    let budget = if q.budget_tokens == 0 { DEFAULT_PROFILE_TOKENS } else { q.budget_tokens };
    let mut members: Vec<&MemoryEntry> = entries.filter(|e| is_member(e, workspace, now_ms)).collect();
    // Kind first, newest first within a kind, id breaks ties.
    members.sort_by(|a, b| {
        kind_rank(a.kind())
            .cmp(&kind_rank(b.kind()))
            .then_with(|| b.occurred_at_ms.cmp(&a.occurred_at_ms))
            .then_with(|| a.id.cmp(&b.id))
    });
    if members.is_empty() {
        return Profile::default();
    }

    let total = members.len();
    let mut lines: Vec<String> = Vec::new();
    let mut ids = Vec::new();
    // Frame plus the worst-case omitted note, so the finished block never exceeds the budget.
    let reserve = estimate_tokens(OPEN) + estimate_tokens(CLOSE) + estimate_tokens(&omitted_note(total)) + 3;
    let mut used = reserve;
    for e in members {
        let l = line(e);
        let cost = estimate_tokens(&l) + 1;
        if used + cost > budget {
            break;
        }
        used += cost;
        lines.push(l);
        ids.push(e.id.clone());
    }
    let omitted = (total - ids.len()) as u32;
    if ids.is_empty() {
        return Profile { omitted, ..Profile::default() };
    }
    let mut block = String::from(OPEN);
    for l in &lines {
        block.push('\n');
        block.push_str(l);
    }
    if omitted > 0 {
        block.push('\n');
        block.push_str(&omitted_note(omitted as usize));
    }
    block.push('\n');
    block.push_str(CLOSE);
    let tokens = estimate_tokens(&block);
    Profile { block, ids, tokens, omitted }
}

fn omitted_note(n: usize) -> String {
    let what = if n == 1 { "item is" } else { "items are" };
    format!("({n} older {what} not shown; recall finds them.)")
}

/// `- kind · 3 Sep: text`, with a decision's reason as `— because …`.
fn line(e: &MemoryEntry) -> String {
    let kind = crate::tags::wire_kind(e.kind()).label();
    let mut text = one_line(&e.text);
    let reason = e.tags.reason.as_deref().map(one_line).filter(|r| !r.is_empty());
    if let Some(r) = &reason {
        // `remember` appends the reason to the stored text so retrieval sees it.
        if let Some(stripped) = text.strip_suffix(&format!(" (reason: {r})")) {
            text = stripped.to_string();
        }
        text = format!("{text} \u{2014} because {r}");
    }
    clip(&format!("- {kind} \u{b7} {}: {text}", day_month(e.occurred_at_ms)), MAX_LINE_CHARS)
}

/// `3 Sep` (UTC).
fn day_month(ms: i64) -> String {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let (_, m, d) = civil(ms);
    format!("{d} {}", MONTHS[(m as usize).clamp(1, 12) - 1])
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut t: String = s.chars().take(max - 1).collect();
    t = t.trim_end().to_string();
    t.push('\u{2026}');
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn day_month_is_short_and_utc() {
        // 2026-09-03T12:00:00Z
        assert_eq!(day_month(1_788_436_800_000), "3 Sep");
        assert_eq!(day_month(0), "1 Jan");
    }

    #[test]
    fn long_lines_are_clipped() {
        let s = clip(&"x".repeat(500), 10);
        assert_eq!(s.chars().count(), 10);
        assert!(s.ends_with('\u{2026}'));
    }
}
