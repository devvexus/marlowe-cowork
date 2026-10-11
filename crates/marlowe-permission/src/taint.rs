//! CONTRACTS.md §12 — `TaintSet`, per-**value** provenance.
//!
//! > Per-VALUE provenance, not per-message. This is what makes the `(action, target)` split
//! > checkable: taint is keyed by argument name, so a call can mix a trusted recipient with an
//! > untrusted body and be adjudicated correctly.
//!
//! # `of` fails closed, and that is the whole of it
//!
//! An argument nobody tracked is not a trusted argument. [`TaintSet::of`] returns
//! `UntrustedContent` for an unknown name, so the failure mode of a *bug in taint tracking* is
//! a blocked call rather than an unchecked one.
//!
//! The test that matters is the one for a **missing** key. A suite that only exercises
//! arguments it remembered to insert would pass identically against
//! `unwrap_or(TrustClass::UserAsserted)` — which is the same shape as every silent-fallback
//! defect this project has logged, and it would invert the security property while looking
//! like tidy code.

use std::collections::BTreeMap;

use marlowe_contract::TrustClass;
use serde::Serialize;

/// Per-argument provenance for one tool call.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct TaintSet {
    by_param: BTreeMap<String, TrustClass>,
}

impl TaintSet {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record where one argument's value came from.
    ///
    /// If the same parameter is recorded twice, **the lower class wins**. A value assembled
    /// from two sources carries the worst of them — the same worst-case rule §3.3 applies to
    /// memory lineage, for the same reason: a string built by concatenating a user's path and
    /// a fetched page's suffix is not a user-asserted path.
    pub fn insert(&mut self, param: impl Into<String>, class: TrustClass) {
        let param = param.into();
        let class = match self.by_param.get(&param) {
            Some(existing) => (*existing).min(class),
            None => class,
        };
        self.by_param.insert(param, class);
    }

    pub fn with(mut self, param: impl Into<String>, class: TrustClass) -> Self {
        self.insert(param, class);
        self
    }

    /// **Absent => `UntrustedContent`. Fail closed.**
    pub fn of(&self, param: &str) -> TrustClass {
        self.by_param.get(param).copied().unwrap_or(TrustClass::UntrustedContent)
    }

    /// Whether this parameter was tracked at all. Used only for diagnostics — no decision may
    /// branch on it, or the fail-closed default becomes reachable-around.
    pub fn tracked(&self, param: &str) -> bool {
        self.by_param.contains_key(param)
    }

    pub fn is_empty(&self) -> bool {
        self.by_param.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &TrustClass)> {
        self.by_param.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_untracked_argument_is_untrusted() {
        // THE test in this file. Every other assertion here would also pass against
        // `unwrap_or(UserAsserted)`.
        let t = TaintSet::new();
        assert_eq!(t.of("recipient"), TrustClass::UntrustedContent);

        let t = TaintSet::new().with("body", TrustClass::UserAsserted);
        assert_eq!(
            t.of("recipient"),
            TrustClass::UntrustedContent,
            "tracking one argument must not vouch for another"
        );
    }

    #[test]
    fn a_value_from_two_sources_carries_the_worse_one() {
        let t = TaintSet::new()
            .with("path", TrustClass::UserAsserted)
            .with("path", TrustClass::UntrustedContent);
        assert_eq!(t.of("path"), TrustClass::UntrustedContent);

        // ...and in the other order, because insertion order is not a security property.
        let t = TaintSet::new()
            .with("path", TrustClass::UntrustedContent)
            .with("path", TrustClass::UserAsserted);
        assert_eq!(t.of("path"), TrustClass::UntrustedContent);
    }

    #[test]
    fn tracked_is_diagnostic_only() {
        let t = TaintSet::new().with("a", TrustClass::AgentObserved);
        assert!(t.tracked("a"));
        assert!(!t.tracked("b"));
        // The value of `tracked` never differs from what `of` already implies for a decision:
        // untracked reads as untrusted either way.
        assert_eq!(t.of("b"), TrustClass::UntrustedContent);
    }
}
