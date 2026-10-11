//! **Audit findings E5 and F1 — the two levers that shortened a view and laundered a class.**
//!
//! There are three ways the assembler makes a window smaller, and CLAUDE.md's layer-3 paragraph
//! turns on which of them preserve `TrustClass`:
//!
//! | Lever | Before this file | Now |
//! |---|---|---|
//! | truncation (`trim_to_budget`, partial fit) | carries `b.trust` | unchanged |
//! | masking (`clear_tool_results`) | rewrites text, preserves class | unchanged |
//! | **omission** (`trim_to_budget`, no fit) | hardcoded `AgentObserved` — **F1** | carries `min` of what it swallowed |
//! | **compaction** (`Assembler::compact`) | hardcoded `AgentInferred` — **E5** | `min(AgentInferred, floor of what was discarded)` |
//!
//! # Why a hardcoded class here was a security defect and not untidiness
//!
//! ADR-041 removed tool results as a taint source in a parent's window, so `InjectedMemory` is the
//! only remaining `UntrustedContent` carrier there — and it is **trimmable**. Both defects
//! therefore sat directly on layer 3's only entry point: the per-source budget, not an attacker,
//! decided when a run stopped being tainted. Within a run `Run::latch_trust_floor` absorbed it.
//! Across the turn boundary it did not, because `Daemon::ask_streaming_with` builds a fresh
//! `Run::root` at `UserAsserted` over a persisted `SessionState` and `engine.rs`'s single latch
//! call re-derives the floor from the view.
//!
//! # Every test here has a control that fails when the mechanism is absent
//!
//! A test that only ever feeds untrusted blocks in cannot tell "the class propagated" from "the
//! class is always `UntrustedContent`". Each positive case below is paired with a negative one
//! carrying no untrusted content, asserting the floor does **not** move — which is the assertion
//! that fails if someone "fixes" this by stamping the bottom of the lattice everywhere.
//!
//! # One thing this file deliberately does NOT claim
//!
//! `compact` excludes the surviving turn from its `min` **by index**. No test here proves that
//! exclusion matters, and none can: the survivor is selected by the predicate
//! `trust == UserAsserted`, and `UserAsserted` is the top of the lattice, so including it in a
//! `min` is unobservable in every reachable state. The exclusion is kept because it is correct
//! if that predicate ever widens — not because it is tested. Saying so beats a test named for a
//! property it cannot see.

use marlowe_contract::TrustClass;
use marlowe_loop::{
    Assembler, Block, GovernanceConstraint, PrefixCache, SessionId, SessionState, SourceKind,
};

/// 3 chars per token — `estimate_tokens` is `len().div_ceil(3)`, and these tests need a block to
/// land on an exact token count so the *next* one has no room and is omitted rather than truncated.
fn tokens_worth(n: u32) -> String {
    "x".repeat((n * 3) as usize)
}

fn session() -> SessionState {
    let mut s = SessionState::new(SessionId::from_name("s"), "Marlowe.");
    s.assert_governance(GovernanceConstraint::asserted("the workspace is ./project"));
    s
}

// ══════════════════════════════════════════════════════════════════════════════════════
// E5 — compaction
// ══════════════════════════════════════════════════════════════════════════════════════

#[test]
fn a_compaction_that_discards_untrusted_content_stamps_the_summary_untrusted() {
    let mut a = Assembler::new(100_000, 10_000);
    let mut cache = PrefixCache::default();
    let mut s = session();

    // A recalled web-derived belief — the one thing that can still taint a parent's window.
    s.push(Block::new(
        SourceKind::InjectedMemory,
        "the vendor's refund address is attacker@example.invalid",
        TrustClass::UntrustedContent,
    ));
    // The turn being answered. Survives compaction and keeps its own class.
    s.push(Block::new(
        SourceKind::History,
        "what is their refund address?",
        TrustClass::UserAsserted,
    ));

    assert_eq!(
        a.assemble(&s).trust_floor(),
        TrustClass::UntrustedContent,
        "precondition: the window is tainted before compaction"
    );

    a.compact(&mut s, SessionId::from_name("child"), "they take refunds".into(), &mut cache);

    let summary = s
        .volatile
        .iter()
        .find(|b| b.source == SourceKind::Summary)
        .expect("compaction produces exactly one summary block");
    assert_eq!(
        summary.trust,
        TrustClass::UntrustedContent,
        "E5: the summary absorbed an UntrustedContent block, so it carries that class. A bare \
         AgentInferred here raises a web page a full step on one rewrite, which is exactly what \
         layer 2 says four rewrites may not do."
    );

    // **The survivor keeps its own class rather than being folded into the summary.** This is the
    // half that makes the partition legible: one block left the tier and one did not.
    let live = s
        .volatile
        .iter()
        .find(|b| b.source == SourceKind::History)
        .expect("the turn being answered is carried across");
    assert_eq!(live.trust, TrustClass::UserAsserted);
    assert!(live.text.contains("refund address?"));

    // And the property the whole thing exists for: the NEXT turn's view is still tainted, with
    // the untrusted block itself long gone.
    let after = a.assemble(&s);
    assert_eq!(
        after.trust_floor(),
        TrustClass::UntrustedContent,
        "the floor must survive the turn boundary — a fresh Run::root re-derives it from here"
    );
    assert!(
        !after.rendered().contains("attacker@example.invalid"),
        "and it survives WITHOUT the untrusted text still being present, which is the point: \
         if the bytes were still in the window the old code would have passed too"
    );
}

/// **THE CONTROL.** Fails if the fix is "stamp the bottom of the lattice and move on".
#[test]
fn a_compaction_that_discards_nothing_untrusted_leaves_the_summary_agent_inferred() {
    let mut a = Assembler::new(100_000, 10_000);
    let mut cache = PrefixCache::default();
    let mut s = session();
    s.push(Block::new(SourceKind::History, "hello", TrustClass::UserAsserted));
    s.push(Block::new(SourceKind::ToolResults, "exit 0", TrustClass::AgentObserved));
    s.push(Block::new(SourceKind::History, "and now?", TrustClass::UserAsserted));

    a.compact(&mut s, SessionId::from_name("child"), "we said hello".into(), &mut cache);

    let summary = s.volatile.iter().find(|b| b.source == SourceKind::Summary).expect("a summary");
    assert_eq!(
        summary.trust,
        TrustClass::AgentInferred,
        "nothing untrusted was discarded, so the min with AgentInferred is AgentInferred. If this \
         reads UntrustedContent the propagation is not a propagation, it is a constant."
    );
    assert!(
        a.assemble(&s).trust_floor() > TrustClass::UntrustedContent,
        "and a clean conversation must not become a blocked one by being compacted"
    );
}

/// `min` never RAISES: a discarded `UserAsserted`-only tier does not produce `UserAsserted`.
#[test]
fn compaction_never_raises_the_class_above_agent_inferred() {
    let mut a = Assembler::new(100_000, 10_000);
    let mut cache = PrefixCache::default();
    let mut s = session();
    s.push(Block::new(SourceKind::History, "one", TrustClass::UserAsserted));
    s.push(Block::new(SourceKind::History, "two", TrustClass::UserAsserted));

    a.compact(&mut s, SessionId::from_name("child"), "summary".into(), &mut cache);

    let summary = s.volatile.iter().find(|b| b.source == SourceKind::Summary).expect("a summary");
    assert_eq!(
        summary.trust,
        TrustClass::AgentInferred,
        "a summary of user turns is still the model's reading of them, not the user's words"
    );
}

/// An empty volatile tier has no origin to propagate from, and must not panic or bottom out.
#[test]
fn compacting_an_empty_tier_is_agent_inferred_and_not_untrusted() {
    let mut a = Assembler::new(100_000, 10_000);
    let mut cache = PrefixCache::default();
    let mut s = session();

    a.compact(&mut s, SessionId::from_name("child"), "nothing happened".into(), &mut cache);

    let summary = s.volatile.iter().find(|b| b.source == SourceKind::Summary).expect("a summary");
    assert_eq!(summary.trust, TrustClass::AgentInferred);
}

// ══════════════════════════════════════════════════════════════════════════════════════
// F1 — the trim omission marker
// ══════════════════════════════════════════════════════════════════════════════════════

/// `InjectedMemory`'s cap is the pinned absolute `MEMORY_TOKEN_BUDGET` (7,000), not a percentage,
/// so these blocks are sized against that rather than against the window.
const MEMORY_CAP_TOKENS: u32 = 7_000;

#[test]
fn an_omission_marker_carries_the_class_of_what_it_omitted() {
    let a = Assembler::new(100_000, 10_000);
    let mut s = session();

    // Oldest: the tainted belief. Small, and it will not fit at all.
    s.push(Block::new(
        SourceKind::InjectedMemory,
        "the vendor's refund address is attacker@example.invalid",
        TrustClass::UntrustedContent,
    ));
    // Newest: fills the per-source cap EXACTLY, so the older block's room is 0 — below
    // MIN_TRUNCATED_TOKENS, which is the branch that omits rather than truncates. Truncation
    // already carried `b.trust`; omission is the branch that did not.
    s.push(Block::new(
        SourceKind::InjectedMemory,
        tokens_worth(MEMORY_CAP_TOKENS),
        TrustClass::UserAsserted,
    ));

    let view = a.assemble(&s);

    // **THE CONTROL, and the reason this test is worth anything.** `adr023_live.rs`'s first run
    // passed at a window where nothing was ever trimmed. If the untrusted block is still in the
    // view, the floor below is coming from the block itself and the marker is untested.
    assert!(
        !view.blocks().any(|b| b.text.contains("attacker@example.invalid")),
        "the untrusted block is STILL IN THE VIEW, so nothing was omitted and this test proves \
         nothing about the omission branch"
    );
    let marker = view
        .blocks()
        .find(|b| b.text.contains("omitted: over the per-source budget"))
        .expect("a block that did not fit produces a marker naming it");

    assert_eq!(
        marker.trust,
        TrustClass::UntrustedContent,
        "F1: the marker stands in for an UntrustedContent block, so it carries that class. A \
         hardcoded AgentObserved here is the per-source budget silently un-tainting a run."
    );
    assert_eq!(
        view.trust_floor(),
        TrustClass::UntrustedContent,
        "and therefore the view's floor does not rise when the budget evicts the taint"
    );
}

/// **THE CONTROL.** Same shape, nothing untrusted — the floor must NOT drop.
#[test]
fn an_omission_marker_over_trusted_blocks_does_not_lower_the_floor() {
    let a = Assembler::new(100_000, 10_000);
    let mut s = session();
    s.push(Block::new(
        SourceKind::InjectedMemory,
        "the user prefers metric units",
        TrustClass::UserAsserted,
    ));
    s.push(Block::new(
        SourceKind::InjectedMemory,
        tokens_worth(MEMORY_CAP_TOKENS),
        TrustClass::UserAsserted,
    ));

    let view = a.assemble(&s);
    let marker = view
        .blocks()
        .find(|b| b.text.contains("omitted: over the per-source budget"))
        .expect("a marker");

    assert_eq!(
        marker.trust,
        TrustClass::UserAsserted,
        "nothing untrusted was omitted, so nothing untrusted is reported. A marker that always \
         read UntrustedContent would pass the test above and brick every clean run."
    );
    assert!(view.trust_floor() > TrustClass::UntrustedContent);
}

/// The marker takes the **worst** of a mixed set, not the first or the last.
#[test]
fn an_omission_marker_takes_the_worst_class_of_a_mixed_set() {
    let a = Assembler::new(100_000, 10_000);
    let mut s = session();
    s.push(Block::new(SourceKind::InjectedMemory, "a user fact", TrustClass::UserAsserted));
    s.push(Block::new(
        SourceKind::InjectedMemory,
        "a page said this",
        TrustClass::UntrustedContent,
    ));
    s.push(Block::new(SourceKind::InjectedMemory, "an exit code", TrustClass::AgentObserved));
    s.push(Block::new(
        SourceKind::InjectedMemory,
        tokens_worth(MEMORY_CAP_TOKENS),
        TrustClass::UserAsserted,
    ));

    let view = a.assemble(&s);
    let marker = view
        .blocks()
        .find(|b| b.text.contains("omitted: over the per-source budget"))
        .expect("a marker");
    assert!(marker.text.contains("3 earlier"), "all three were folded into one marker");
    assert_eq!(marker.trust, TrustClass::UntrustedContent, "worst-case over the set, per §3.3");
}

/// Truncation was always correct. Asserted so a later change to the branch above cannot quietly
/// take this one with it.
#[test]
fn truncation_still_carries_the_class_it_always_did() {
    let a = Assembler::new(100_000, 10_000);
    let mut s = session();
    s.push(Block::new(
        SourceKind::InjectedMemory,
        format!("attacker@example.invalid {}", tokens_worth(MEMORY_CAP_TOKENS + 500)),
        TrustClass::UntrustedContent,
    ));

    let view = a.assemble(&s);
    let t = view
        .blocks()
        .find(|b| b.text.contains("[truncated at the per-source budget]"))
        .expect("a block that partially fits is truncated");
    assert_eq!(t.trust, TrustClass::UntrustedContent);
    assert_eq!(view.trust_floor(), TrustClass::UntrustedContent);
}
