//! The journal record of a finished task, and the keys that name tasks.
//!
//! Every task has a key, `p:<phase>:<step>:<item>:<hash>`, and every finished task appends one
//! JSON line to `phases/<phase>/<step>.jsonl` *before* its result touches the state. A record
//! holds everything the task changed (the items, the agent, the effects), so replaying it after a
//! restart gives the state the live run would have given: the model is not asked again and the
//! page is not fetched again. A task with no line, because the process died while it ran, is the
//! only thing that runs again.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::functions::Effect;
use crate::state::{Agent, Item};

/// How a task ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// It made its result.
    Done,
    /// It gave up; the item (if any) is `failed` with a harness-written reason.
    Failed,
}

/// What a finished task changed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    /// The task's key.
    pub key: String,
    /// The phase instance.
    pub phase: String,
    /// The step.
    pub step: String,
    /// The input item (`None` for a task over the project).
    #[serde(default)]
    pub item: Option<String>,
    /// The group (question) it belongs to.
    #[serde(default)]
    pub sub: String,
    /// How it ended.
    pub outcome: Outcome,
    /// A harness-written reason when it failed.
    #[serde(default)]
    pub error: Option<String>,
    /// Model attempts.
    #[serde(default)]
    pub attempts: u32,
    /// The failure class of each attempt that did not produce the form.
    #[serde(default)]
    pub classes: Vec<String>,
    /// Prompt tokens used.
    #[serde(default)]
    pub tokens_in: u64,
    /// Output tokens used.
    #[serde(default)]
    pub tokens_out: u64,
    /// The employee, as it ended.
    #[serde(default)]
    pub agent: Option<Agent>,
    /// Every item to store: the emitted or filled item and any further ones, as they end.
    #[serde(default)]
    pub items: Vec<Item>,
    /// Further changes to the project.
    #[serde(default)]
    pub effects: Vec<Effect>,
    /// The phase output a code step set.
    #[serde(default)]
    pub output: Option<Value>,
    /// Counts of what a rule dropped.
    #[serde(default)]
    pub rejected: Option<Value>,
    /// The task read network content.
    #[serde(default)]
    pub tainted: bool,
    /// A merge: the items this pass took in.
    #[serde(default)]
    pub members: Vec<String>,
    /// A merge: the threshold index that fired it.
    #[serde(default)]
    pub threshold: Option<usize>,
    /// A gate: the id of the gate whose answer this records.
    #[serde(default)]
    pub gate: Option<String>,
    /// Wall time of the task.
    #[serde(default)]
    pub ms: u64,
    /// When it was journaled.
    #[serde(default)]
    pub at_ms: u64,
}

impl Record {
    /// An empty record for a task.
    pub fn new(key: String, phase: &str, step: &str, item: Option<String>, sub: &str) -> Record {
        Record {
            key,
            phase: phase.into(),
            step: step.into(),
            item,
            sub: sub.into(),
            outcome: Outcome::Done,
            error: None,
            attempts: 0,
            classes: Vec::new(),
            tokens_in: 0,
            tokens_out: 0,
            agent: None,
            items: Vec::new(),
            effects: Vec::new(),
            output: None,
            rejected: None,
            tainted: false,
            members: Vec::new(),
            threshold: None,
            gate: None,
            ms: 0,
            at_ms: 0,
        }
    }
}

fn hash12(parts: &[&str]) -> String {
    let mut h = blake3::Hasher::new();
    for p in parts {
        h.update(p.as_bytes());
        h.update(&[0]);
    }
    h.finalize().to_hex()[..12].to_string()
}

/// The key of a task over an item (or the whole project when `item` is `None`). The hash covers
/// the item id (ids derive from content, so changed input is a new item), the step's form and
/// function, and the phase's round: a continuation round re-asks nothing already answered but a
/// changed step declaration asks again.
pub fn task_key(phase: &str, step: &str, item: Option<&str>, form: &str, func: &str, round: u32) -> String {
    let who = item.unwrap_or("project");
    format!("p:{phase}:{step}:{who}:{}", hash12(&[who, form, func, &round.to_string(), if item.is_some() { "" } else { phase }]))
}

/// The key of one merge pass: it hashes the member ids, so a pass is the same pass whatever the
/// moment it fired.
pub fn merge_key(phase: &str, step: &str, sub: &str, members: &[String]) -> String {
    let mut ids: Vec<&str> = members.iter().map(String::as_str).collect();
    ids.sort_unstable();
    format!("p:{phase}:{step}:{sub}:{}", hash12(&ids))
}

/// The key of the handling of a gate's answer.
pub fn answer_key(phase: &str, step: &str, gate: &str) -> String {
    format!("p:{phase}:{step}:answer:{}", hash12(&[gate]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_task_key_is_stable_and_changes_with_its_input() {
        let a = task_key("ph_dc", "analyze", Some("i_1"), "analyze@1", "f", 1);
        assert_eq!(a, task_key("ph_dc", "analyze", Some("i_1"), "analyze@1", "f", 1));
        assert!(a.starts_with("p:ph_dc:analyze:i_1:") && a.len() == "p:ph_dc:analyze:i_1:".len() + 12);
        assert_ne!(a, task_key("ph_dc", "analyze", Some("i_2"), "analyze@1", "f", 1));
        assert_ne!(a, task_key("ph_dc", "analyze", Some("i_1"), "analyze@2", "f", 1));
        assert_ne!(a, task_key("ph_dc", "analyze", Some("i_1"), "analyze@1", "f", 2));
        // A task over the project is per phase instance, so round 2 is not round 1.
        assert_ne!(task_key("ph_think", "decide", None, "x@1", "", 1), task_key("ph_think_2", "decide", None, "x@1", "", 1));
    }

    #[test]
    fn a_merge_key_ignores_the_order_the_members_arrived_in() {
        let m = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(merge_key("p", "s", "q1", &m(&["a", "b", "c"])), merge_key("p", "s", "q1", &m(&["c", "a", "b"])));
        assert_ne!(merge_key("p", "s", "q1", &m(&["a", "b"])), merge_key("p", "s", "q1", &m(&["a", "b", "c"])));
    }

    #[test]
    fn a_record_round_trips_through_json_unchanged() {
        let mut r = Record::new("k".into(), "ph_dc", "analyze", Some("i_1".into()), "q1");
        r.items.push(Item::new("i_2".into(), "source", "analyze@1"));
        r.effects.push(Effect::JumpToReport);
        r.outcome = Outcome::Failed;
        r.error = Some("form did not validate".into());
        let back: Record = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(back, r);
        // A record from an older build with fewer fields still loads.
        let old: Record = serde_json::from_value(serde_json::json!({"key": "k", "phase": "p", "step": "s", "outcome": "done"})).unwrap();
        assert_eq!(old.items.len(), 0);
    }
}
