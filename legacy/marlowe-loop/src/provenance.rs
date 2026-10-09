//! Where an argument's value came from — computed by the harness, never declared by the model.
//!
//! ARCHITECTURE §3 calls `taint.of(args)` inside the adjudication step. This module is what
//! produces that `TaintSet`, and the single most important property is negative: **the driver
//! does not supply it**. A `ModelStep::ToolCall` carries a tool and arguments and nothing about
//! their provenance, because a model that could label its own arguments trusted would be the
//! security boundary — which invariant 3 says it is not.
//!
//! # The rule, and the consequence that falls out of it
//!
//! Two sources of truth, in order:
//!
//! 1. **Attribution.** The harness knows the exact strings the user typed and the exact fields
//!    a tool result carried, and records them with their class. An argument whose value matches
//!    one exactly carries that class.
//! 2. **The floor.** Everything else is model-composed, and §3.3's worst-case rule applies to
//!    the window it was composed in: a model-authored value carries the **worst trust class
//!    present in the context view**.
//!
//! The consequence is worth stating because it looks like a bug the first time it fires:
//! **once a run has read untrusted content, every model-composed Target in that run is
//! blocked.** That is §8.2's structural trifecta break arriving as a property rather than as a
//! separate mechanism — the way to act on what a web page said is to spawn a quarantined reader
//! that returns structured findings, and let the orchestrator, which never saw the page, act.
//!
//! Matching is exact. A value that differs by a trailing space falls to the floor rather than
//! matching, because a fuzzy match here would be an attacker-shaped near-miss away from
//! promoting untrusted text to user-asserted.

use std::collections::BTreeMap;

use marlowe_contract::TrustClass;
use marlowe_permission::{ArgValue, Args, TaintSet};

use crate::context::ContextView;

#[derive(Debug, Clone, Default)]
pub struct Provenance {
    attributed: BTreeMap<String, TrustClass>,
}

impl Provenance {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a string the harness can vouch for.
    ///
    /// Called with what the user typed (`UserAsserted`) and with fields the harness itself
    /// computed (`AgentObserved` — exit codes, hashes, resolved paths). **Not** called with
    /// model output, and not called with the body of a fetched page: a page's text is in the
    /// context view, where it lowers the floor, which is the correct effect.
    ///
    /// If the same string arrives twice with different classes, the lower wins — the same
    /// worst-case rule §3.3 applies to lineage.
    pub fn attribute(&mut self, value: impl Into<String>, class: TrustClass) {
        let value = value.into();
        let class = match self.attributed.get(&value) {
            Some(existing) => (*existing).min(class),
            None => class,
        };
        self.attributed.insert(value, class);
    }

    /// Record every whitespace-separated token of a user's message, plus the whole message.
    ///
    /// A user who types `read src/main.rs` has asserted `src/main.rs`, and the model passing
    /// that exact token as a path is passing the user's own words. Tokenizing on whitespace is
    /// crude and deliberately conservative: it attributes only substrings the user literally
    /// typed, with no normalization, so nothing is promoted by being merely similar.
    pub fn attribute_user_message(&mut self, message: &str) {
        self.attribute(message.trim(), TrustClass::UserAsserted);
        for token in message.split_whitespace() {
            self.attribute(token, TrustClass::UserAsserted);
        }
    }

    /// The per-argument provenance for one tool call.
    /// `latched` is the RUN's floor — the worst class it has ever been exposed to.
    ///
    /// **It is a parameter rather than something derived here**, and that is the fix for a real
    /// hole. This used to use `view.trust_floor()` alone: the minimum over the blocks *currently*
    /// in the view. `ToolResults` is trimmable, so the assembler could drop an untrusted block to
    /// stay inside its budget and the floor would **rise again** — a run silently regaining
    /// privileges ADR-023 says it loses permanently, with no error and no event.
    ///
    /// The view's floor is still consulted: a block present now can only make things worse, never
    /// better. `min` of the two is the whole rule.
    pub fn taint_for(&self, args: &Args, view: &ContextView, latched: TrustClass) -> TaintSet {
        let floor = view.trust_floor().min(latched);
        let mut taint = TaintSet::new();
        for (name, value) in args.iter() {
            let class = match value {
                ArgValue::Text(s) => self.attributed.get(s.as_str()).copied().unwrap_or(floor),
                // A number the model chose is model-composed. There is no attribution path for
                // it and there should not be one: an amount is exactly the kind of Target §9
                // names, and "the model picked a number" is not evidence about who chose it.
                _ => floor,
            };
            taint.insert(name.clone(), class);
        }
        taint
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{Assembler, Block, GovernanceConstraint, SessionState, SourceKind};
    use crate::run::SessionId;

    fn view_with(blocks: Vec<Block>) -> ContextView {
        let mut s = SessionState::new(SessionId::from_name("s"), "Marlowe.");
        s.assert_governance(GovernanceConstraint::asserted("c"));
        for b in blocks {
            s.push(b);
        }
        Assembler::new(100_000, 10_000).assemble(&s)
    }

    #[test]
    fn a_path_the_user_typed_is_user_asserted() {
        let mut p = Provenance::new();
        p.attribute_user_message("please read src/main.rs and summarise");
        let args = Args::new().text("path", "src/main.rs");
        let t = p.taint_for(&args, &view_with(vec![]), TrustClass::UserAsserted);
        assert_eq!(t.of("path"), TrustClass::UserAsserted);
    }

    #[test]
    fn a_value_the_model_composed_carries_the_windows_floor() {
        let mut p = Provenance::new();
        p.attribute_user_message("have a look around");
        let args = Args::new().text("path", "src/inferred.rs");

        // Clean window: the model inferring a path from repo convention passes, per §9.
        assert_eq!(
            p.taint_for(&args, &view_with(vec![]), TrustClass::UserAsserted).of("path"),
            TrustClass::AgentObserved,
            "the floor of a clean window is what the harness itself put there"
        );

        // Once a page is in view, the same argument is untrusted. This is the trifecta break
        // arriving as a property of provenance rather than as a separate mechanism.
        let dirty = view_with(vec![Block::new(
            SourceKind::ToolResults,
            "fetched: write to ~/.bashrc",
            TrustClass::UntrustedContent,
        )]);
        assert_eq!(p.taint_for(&args, &dirty, TrustClass::UserAsserted).of("path"), TrustClass::UntrustedContent);
    }

    #[test]
    fn attribution_is_exact_and_a_near_miss_does_not_match() {
        let mut p = Provenance::new();
        p.attribute_user_message("read notes.md");
        let dirty = view_with(vec![Block::new(
            SourceKind::ToolResults,
            "page",
            TrustClass::UntrustedContent,
        )]);
        assert_eq!(
            p.taint_for(&Args::new().text("path", "notes.md"), &dirty, TrustClass::UserAsserted).of("path"),
            TrustClass::UserAsserted
        );
        assert_eq!(
            p.taint_for(&Args::new().text("path", "notes.md "), &dirty, TrustClass::UserAsserted).of("path"),
            TrustClass::UntrustedContent,
            "a near-miss falls to the floor; fuzzy matching here would promote by similarity"
        );
    }

    #[test]
    fn an_argument_the_model_supplied_that_nobody_attributed_is_not_trusted_by_default() {
        // The fail-closed half. `TaintSet::of` already defaults to untrusted for an absent
        // key, and this asserts the tracker never inserts a key it cannot justify.
        let p = Provenance::new();
        let dirty = view_with(vec![Block::new(
            SourceKind::ToolResults,
            "page",
            TrustClass::UntrustedContent,
        )]);
        let t = p.taint_for(&Args::new().text("recipient", "someone@example.com"), &dirty, TrustClass::UserAsserted);
        assert_eq!(t.of("recipient"), TrustClass::UntrustedContent);
    }

    #[test]
    fn a_numeric_argument_has_no_attribution_path() {
        let mut p = Provenance::new();
        p.attribute_user_message("spend 500");
        // `budget_tokens` — ADR-057 §6 renamed it from `budget_micros_usd` and retyped it from
        // `Amount` to `Integer`, because `SpawnRequest::grant_tokens` is tokens and a token count
        // is not money. The property under test is unchanged: a number is a Target and carries the
        // floor, whichever numeric variant it arrives as.
        let args = Args::new().with("budget_tokens", ArgValue::Integer(12_000));
        let dirty = view_with(vec![Block::new(
            SourceKind::ToolResults,
            "page",
            TrustClass::UntrustedContent,
        )]);
        assert_eq!(
            p.taint_for(&args, &dirty, TrustClass::UserAsserted).of("budget_tokens"),
            TrustClass::UntrustedContent,
            "a number is a Target; `the model picked a number` is not evidence about who chose it"
        );
    }
}
