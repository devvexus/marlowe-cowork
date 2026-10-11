//! Configured permission rules as rows (where each came from), and their validation. Matching
//! and precedence are `marlowe_permission::rules`.

use std::path::PathBuf;

use marlowe_permission::rules::{Action, Rule, RuleSet};
use marlowe_proto::{RuleAction, RuleInfo};

use crate::layers::Source;

/// One configured rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleEntry {
    pub action: RuleAction,
    pub pattern: String,
    pub source: Source,
    pub path: Option<PathBuf>,
}

impl RuleEntry {
    pub fn info(&self) -> RuleInfo {
        RuleInfo {
            action: self.action,
            pattern: self.pattern.clone(),
            scope: self.source.scope(),
            source: Some(match &self.path {
                Some(p) => p.display().to_string(),
                None => self.source.label().to_string(),
            }),
        }
    }
}

pub fn action(a: RuleAction) -> Action {
    match a {
        RuleAction::Allow => Action::Allow,
        RuleAction::Ask => Action::Ask,
        RuleAction::Deny => Action::Deny,
    }
}

/// Checks a pattern the way the engine will read it.
pub fn validate(pattern: &str) -> Result<(), String> {
    Rule::parse(pattern).map(|_| ()).map_err(|e| e.to_string())
}

/// The engine's rule set from rows; rows that do not parse are skipped (and reported by
/// [`invalid`]).
pub fn rule_set(entries: &[RuleEntry]) -> RuleSet {
    let mut set = RuleSet::new();
    for e in entries {
        let _ = set.add_text(action(e.action), &e.pattern);
    }
    set
}

/// Rows whose pattern does not parse, with the reason (shown at session start and in `/permissions`).
pub fn invalid(entries: &[RuleEntry]) -> Vec<(RuleEntry, String)> {
    entries.iter().filter_map(|e| validate(&e.pattern).err().map(|why| (e.clone(), why))).collect()
}
