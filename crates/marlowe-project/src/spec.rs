//! Phase declarations as data.
//!
//! A phase is an ordered list of steps; a step is one of four primitives (design 4.1):
//! `fanout` (one fresh-context employee per input item, forced into a form), `code` (a function
//! with no model), `merge` (a delta merge at 25/50/75/100% of a watched step) and `gate` (parks
//! the run for the user). The menu lives in `registry/phases/*.json`; a new phase type is a JSON
//! file, a form and, for a code step, a name in the [`crate::engine::Toolbox`], never a new
//! program. [`validate_menu`] checks the whole menu at once, so a mistyped edge or a form that
//! does not exist is a test failure, not a stuck project.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::registry::Registry;
use crate::state::{GateKind, StepKind};

/// How an item flows from one step to the next.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Edge {
    /// An item moves on the moment it is `done`.
    #[default]
    Stream,
    /// The next step waits until the upstream step has nothing left (everywhere, or for one group).
    Barrier,
}

/// One incoming edge of a step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct After {
    /// The upstream step's id (unique across the menu).
    pub step: String,
    /// Stream or barrier.
    pub edge: Edge,
    /// For a barrier: `sub` waits per group (per question) instead of for everything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per: Option<String>,
}

impl<'de> Deserialize<'de> for After {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Name(String),
            Full {
                step: String,
                #[serde(default)]
                edge: Edge,
                #[serde(default)]
                per: Option<String>,
            },
        }
        Ok(match Raw::deserialize(d)? {
            Raw::Name(step) => After { step, edge: Edge::Stream, per: None },
            Raw::Full { step, edge, per } => After { step, edge, per },
        })
    }
}

/// The resource a task runs on. Fetch and search hold no model slot, so a page can be read while
/// every model slot is busy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lane {
    /// A model call (employees, the lead): bounded by the crew's slots.
    Model,
    /// Page fetches, a host at a time.
    Fetch,
    /// Search services.
    Search,
    /// Pure computation.
    Cpu,
    /// Disk and the Marlowe home.
    Io,
}

impl Lane {
    /// Stable lower-case word.
    pub fn as_str(self) -> &'static str {
        match self {
            Lane::Model => "model",
            Lane::Fetch => "fetch",
            Lane::Search => "search",
            Lane::Cpu => "cpu",
            Lane::Io => "io",
        }
    }
}

/// Which model class a fan-out uses; the engine's model plan maps it to a model.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelClass {
    /// The many single-task employees.
    #[default]
    Employee,
    /// The one smarter model that decides (Thinking).
    Lead,
}

/// Whether a step asks its model to think before it answers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Think {
    /// Answer straight into the form (the default: the grammar covers the answer, not reasoning).
    #[default]
    Off,
    /// Reason first, within `think_budget` tokens, where the engine supports a budget; else the
    /// form's own `reason` field comes first.
    Think,
}

/// Which items a step with no upstream step starts from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// Every item of the kind, whatever round made it.
    #[default]
    All,
    /// Only items made in the same round as the phase (a continuation's new questions).
    Round,
}

/// One step of a phase.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StepSpec {
    /// Unique across the whole menu.
    pub id: String,
    /// Which primitive.
    pub kind: StepKind,
    /// The item kind the step runs over; `project` (or nothing) means one task for the project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub over: Option<String>,
    /// For a step with no upstream step: all items of the kind, or this round's.
    #[serde(default, skip_serializing_if = "is_default")]
    pub scope: Scope,
    /// Incoming edges.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub after: Vec<After>,
    /// The form (`name@version`) an employee is forced into.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub form: Option<String>,
    /// The role card name (also the prompt template id unless `prompt` says otherwise).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// The prompt builder's name in the toolbox (defaults to `role`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// Employee or lead.
    #[serde(default)]
    pub class: ModelClass,
    /// Think or answer straight away.
    #[serde(default)]
    pub effort: Think,
    /// Most reasoning tokens when `effort` is `think`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub think_budget: Option<u32>,
    /// Most answer tokens; set from the form's worst case.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output: Option<u32>,
    /// Attempts per employee (default 3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempts: Option<u32>,
    /// The kind of item the step makes. A fan-out with `emit` makes one item of that kind per
    /// input (its id derived from the input); without it the form fills the input item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emit: Option<String>,
    /// A code step's function, or the post-processing function of a fan-out or merge (the form
    /// rules and the code-filled fields), by name in the toolbox.
    #[serde(default, rename = "fn", skip_serializing_if = "Option::is_none")]
    pub func: Option<String>,
    /// A code step's lane (default `cpu`); a fan-out and a merge are always `model`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lane: Option<Lane>,
    /// Most tasks of this step at once (default: the lane's width).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concurrency: Option<u32>,
    /// A merge: the fan-out whose finished share sets the pace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watch: Option<String>,
    /// A merge: the shares at which a pass fires (default 0.25, 0.5, 0.75, 1.0).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub thresholds: Vec<f64>,
    /// A merge: fewest new items worth a pass before the last (default 1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_new: Option<u32>,
    /// A merge: the group a pass works on (`sub`: per question).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_by: Option<String>,
    /// A gate: plan or checkin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate: Option<GateKind>,
    /// A gate: the toolbox function that builds the card.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_fn: Option<String>,
    /// A gate: the toolbox function that acts on the answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer_fn: Option<String>,
    /// More than the failure share of this step's items failing raises a `failures` checkin.
    #[serde(default, skip_serializing_if = "is_false")]
    pub important: bool,
}

fn is_default<T: Default + PartialEq>(v: &T) -> bool {
    *v == T::default()
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// The thresholds a merge uses when it names none.
pub const DEFAULT_THRESHOLDS: [f64; 4] = [0.25, 0.5, 0.75, 1.0];

impl StepSpec {
    /// The step's lane: always `model` for a fan-out and a merge, `cpu` for a code step that names
    /// none, `io` for a gate.
    pub fn lane(&self) -> Lane {
        match self.kind {
            StepKind::Fanout | StepKind::Merge => Lane::Model,
            StepKind::Code => self.lane.unwrap_or(Lane::Cpu),
            StepKind::Gate => Lane::Io,
        }
    }

    /// The item kind the step runs over, or `None` for one task for the whole project.
    pub fn over_kind(&self) -> Option<&str> {
        self.over.as_deref().filter(|o| *o != "project")
    }

    /// The merge thresholds in force.
    pub fn thresholds(&self) -> Vec<f64> {
        if self.thresholds.is_empty() {
            DEFAULT_THRESHOLDS.to_vec()
        } else {
            self.thresholds.clone()
        }
    }

    /// The prompt builder's name: `prompt`, else `role`, else the step id.
    pub fn prompt_name(&self) -> &str {
        self.prompt.as_deref().or(self.role.as_deref()).unwrap_or(&self.id)
    }

    /// Attempts per employee.
    pub fn attempts(&self) -> u32 {
        self.attempts.unwrap_or(3).clamp(1, 6)
    }
}

/// One phase type: its steps and the form of the one output the next phase reads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PhaseSpec {
    /// The phase type, from [`crate::PHASE_MENU`].
    pub phase: String,
    /// The form (`name@version`) of the output a code step sets on the phase, if it has one.
    #[serde(default)]
    pub output: Option<String>,
    /// The steps, in declaration order.
    pub steps: Vec<StepSpec>,
}

impl PhaseSpec {
    /// The step with this id.
    pub fn step(&self, id: &str) -> Option<&StepSpec> {
        self.steps.iter().find(|s| s.id == id)
    }
}

/// Every problem with the menu, as sentences a person can act on; empty when it is sound.
///
/// Checked: step ids are unique across the menu, every `after` and `watch` names a step, the
/// edges make no cycle, every form exists and every employee step has what it needs (a role, a
/// form), merges watch a fan-out that is upstream of them, a gate names its kind and card, and a
/// phase `output` names a form.
pub fn validate_menu(menu: &BTreeMap<String, PhaseSpec>, registry: &Registry) -> Vec<String> {
    let mut errs = Vec::new();
    let mut owner: BTreeMap<&str, &str> = BTreeMap::new();
    for (name, p) in menu {
        if p.phase != *name {
            errs.push(format!("phase file `{name}` declares `{}`", p.phase));
        }
        if let Some(o) = &p.output {
            if registry.form(o).is_none() {
                errs.push(format!("{name}: output form `{o}` is not registered"));
            }
        }
        for s in &p.steps {
            if let Some(prev) = owner.insert(&s.id, name) {
                errs.push(format!("step id `{}` is used in both {prev} and {name}; step ids are unique across the menu", s.id));
            }
        }
    }
    let all: BTreeMap<&str, &StepSpec> = menu.values().flat_map(|p| p.steps.iter()).map(|s| (s.id.as_str(), s)).collect();
    for (name, p) in menu {
        for s in &p.steps {
            let at = format!("{name}.{}", s.id);
            for a in &s.after {
                if !all.contains_key(a.step.as_str()) {
                    errs.push(format!("{at}: after `{}` is not a step", a.step));
                }
                if a.per.as_deref().is_some_and(|p| p != "sub") {
                    errs.push(format!("{at}: after `{}` has per `{}`; only `sub` is known", a.step, a.per.clone().unwrap_or_default()));
                }
                if a.per.is_some() && a.edge != Edge::Barrier {
                    errs.push(format!("{at}: `per` only belongs on a barrier edge"));
                }
            }
            if let Some(f) = &s.form {
                if registry.form(f).is_none() {
                    errs.push(format!("{at}: form `{f}` is not registered"));
                }
            }
            match s.kind {
                StepKind::Fanout | StepKind::Merge => {
                    if s.form.is_none() {
                        errs.push(format!("{at}: an employee step needs a `form`"));
                    }
                    if s.role.is_none() {
                        errs.push(format!("{at}: an employee step needs a `role`"));
                    }
                    if s.lane.is_some_and(|l| l != Lane::Model) {
                        errs.push(format!("{at}: an employee step runs on the model lane"));
                    }
                    if s.effort == Think::Think && s.think_budget.is_none() {
                        errs.push(format!("{at}: a step that thinks needs a `think_budget`"));
                    }
                }
                StepKind::Code => {
                    if s.func.is_none() {
                        errs.push(format!("{at}: a code step needs `fn`"));
                    }
                    if s.lane == Some(Lane::Model) {
                        errs.push(format!("{at}: a code step has no model; give it the cpu, io, fetch or search lane"));
                    }
                }
                StepKind::Gate => {
                    if s.gate.is_none() || s.payload_fn.is_none() {
                        errs.push(format!("{at}: a gate needs `gate` and `payload_fn`"));
                    }
                }
            }
            if s.kind == StepKind::Merge {
                match s.watch.as_deref().and_then(|w| all.get(w)) {
                    None => errs.push(format!("{at}: a merge needs `watch` naming a fan-out step")),
                    Some(w) if w.kind != StepKind::Fanout => errs.push(format!("{at}: watch `{}` is not a fan-out", w.id)),
                    Some(w) => {
                        if !upstream_of(&all, &s.id).contains(w.id.as_str()) {
                            errs.push(format!("{at}: watch `{}` is not upstream of the merge", w.id));
                        }
                    }
                }
                if s.over.is_none() || s.group_by.is_none() || s.emit.is_none() {
                    errs.push(format!("{at}: a merge needs `over`, `group_by` and `emit`"));
                }
                if s.group_by.as_deref().is_some_and(|g| g != "sub") {
                    errs.push(format!("{at}: group_by `{}` is unknown; only `sub` is known", s.group_by.clone().unwrap_or_default()));
                }
                let t = s.thresholds();
                if t.windows(2).any(|w| w[0] >= w[1]) || t.iter().any(|x| !(*x > 0.0 && *x <= 1.0)) || (t.last().copied() != Some(1.0)) {
                    errs.push(format!("{at}: thresholds must rise, lie in (0, 1] and end at 1.0"));
                }
            } else if s.watch.is_some() || !s.thresholds.is_empty() || s.group_by.is_some() {
                errs.push(format!("{at}: watch, thresholds and group_by belong on a merge"));
            }
        }
    }
    // Cycles: a step reachable from itself through `after`.
    for id in all.keys() {
        if upstream_of(&all, id).contains(id) {
            errs.push(format!("step `{id}` is its own ancestor; the edges make a cycle"));
        }
    }
    errs
}

/// Every step upstream of `id` through `after` (transitively), not including `id` unless a cycle
/// leads back to it.
pub fn upstream_of<'a>(all: &BTreeMap<&'a str, &'a StepSpec>, id: &str) -> BTreeSet<&'a str> {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut stack: Vec<&str> = all.get(id).map(|s| s.after.iter().map(|a| a.step.as_str()).collect()).unwrap_or_default();
    while let Some(n) = stack.pop() {
        if let Some((k, s)) = all.get_key_value(n) {
            if seen.insert(k) {
                stack.extend(s.after.iter().map(|a| a.step.as_str()));
            }
        }
    }
    seen
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn menu(phases: Vec<serde_json::Value>) -> BTreeMap<String, PhaseSpec> {
        phases.into_iter().map(|v| serde_json::from_value::<PhaseSpec>(v).unwrap()).map(|p| (p.phase.clone(), p)).collect()
    }

    fn reg() -> Registry {
        Registry::builtin()
    }

    #[test]
    fn an_after_is_a_bare_name_or_an_object() {
        let a: Vec<After> = serde_json::from_value(json!(["x", {"step": "y", "edge": "barrier", "per": "sub"}])).unwrap();
        assert_eq!(a[0], After { step: "x".into(), edge: Edge::Stream, per: None });
        assert_eq!(a[1], After { step: "y".into(), edge: Edge::Barrier, per: Some("sub".into()) });
    }

    #[test]
    fn a_sound_menu_has_no_problems() {
        let m = menu(vec![json!({"phase": "Plan", "steps": [
            {"id": "a", "kind": "code", "fn": "f"},
            {"id": "b", "kind": "fanout", "over": "question", "form": "analyze@1", "role": "r", "after": ["a"]}]})]);
        assert_eq!(validate_menu(&m, &reg()), Vec::<String>::new());
    }

    #[test]
    fn each_kind_of_mistake_is_named() {
        let m = menu(vec![
            json!({"phase": "Plan", "steps": [
                {"id": "a", "kind": "code"},
                {"id": "b", "kind": "fanout", "over": "question", "form": "nope@1", "after": ["zzz"]},
                {"id": "g", "kind": "gate"}]}),
            json!({"phase": "Finish", "steps": [{"id": "a", "kind": "code", "fn": "x"}]}),
        ]);
        let errs = validate_menu(&m, &reg()).join("\n");
        for needle in [
            "used in both",
            "needs `fn`",
            "form `nope@1` is not registered",
            "after `zzz` is not a step",
            "needs a `role`",
            "needs `gate` and `payload_fn`",
        ] {
            assert!(errs.contains(needle), "missing `{needle}` in:\n{errs}");
        }
    }

    #[test]
    fn a_cycle_and_a_bad_merge_are_found() {
        let m = menu(vec![json!({"phase": "Plan", "steps": [
            {"id": "a", "kind": "code", "fn": "f", "after": ["b"]},
            {"id": "b", "kind": "code", "fn": "f", "after": ["a"]},
            {"id": "m", "kind": "merge", "form": "condense@1", "role": "r", "watch": "a", "over": "claim", "group_by": "sub", "emit": "cclaim", "thresholds": [0.5, 0.25]}]})]);
        let errs = validate_menu(&m, &reg()).join("\n");
        assert!(errs.contains("its own ancestor"), "{errs}");
        assert!(errs.contains("is not a fan-out"), "{errs}");
        assert!(errs.contains("thresholds must rise"), "{errs}");
    }

    #[test]
    fn a_step_that_thinks_needs_a_budget() {
        let m = menu(vec![json!({"phase": "Plan", "steps": [
            {"id": "a", "kind": "fanout", "over": "question", "form": "analyze@1", "role": "r", "effort": "think"}]})]);
        assert!(validate_menu(&m, &reg()).join("\n").contains("think_budget"));
    }
}
