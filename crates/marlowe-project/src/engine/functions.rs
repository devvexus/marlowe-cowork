//! The toolbox: the named functions a phase declaration points at.
//!
//! A phase is data (`registry/phases/*.json`); what it runs is looked up here by name. Four kinds
//! of function exist, and nothing else is needed to add a phase type:
//!
//! | Kind | Used by | Does |
//! |---|---|---|
//! | [`PromptFn`] | a fan-out or merge (`role`/`prompt`) | builds the system blocks (role card and plan context, shared by siblings so the engine's cache holds) and the one user message (the one item, last, fenced) |
//! | [`FinishFn`] | a fan-out or merge (`fn`) | the form's rules and the code-filled fields: turns the valid model output into data, extra items and effects, or rejects it with a [`RuleFailure`] the model is told |
//! | [`CodeFn`] | a code step (`fn`) | a function with no model: search, fetch, extract, assemble |
//! | [`PayloadFn`] / [`AnswerFn`] | a gate | builds the card; acts on the user's answer |
//!
//! Everything a function wants to change in the project beyond its own items is an [`Effect`]:
//! serialisable, so it is journaled with the task and replayed on resume.

use std::collections::HashMap;
use std::sync::Arc;

use marlowe_model::{CancelToken, SystemBlock};
use marlowe_proto::ProjectGateOption;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::fence;
use super::runner::RuleFailure;
use crate::actor::ProjectHandle;
use crate::plan::Plan;
use crate::spec::StepSpec;
use crate::state::{Gate, GateAnswer, Item, Phase, PlanRecord, ProjectState, StageStatus};

/// A change to the project beyond a task's own items. Journaled with the task, so it replays.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "do", rename_all = "snake_case")]
pub enum Effect {
    /// Insert a phase instance before `before` (appended when absent). Idempotent: an existing
    /// phase of that id is left alone.
    InsertPhase {
        /// The phase to go in front of.
        before: Option<String>,
        /// The new instance.
        phase: Phase,
    },
    /// Replace the plan (a continuation).
    SetPlan {
        /// The new plan.
        plan: Box<Plan>,
    },
    /// Record a decision about a plan.
    PlanRecord {
        /// The record.
        record: PlanRecord,
    },
    /// Grant the run its network reads: public https GETs of pages it found, robots.txt honoured,
    /// up to `quota` in all. Idempotent: the quota only grows and the count used is kept.
    NetGrant {
        /// The most fetches in all.
        quota: u32,
    },
    /// Set a phase's status.
    PhaseStatus {
        /// The phase id.
        id: String,
        /// The status.
        status: StageStatus,
    },
    /// Skip every phase that has not finished, up to the next Report (else Finish) phase: the
    /// user chose to write the report with what exists.
    JumpToReport,
    /// Raise the budgets to `to` quotes (the user chose to continue at a budget checkin). Absolute,
    /// so replaying it twice changes nothing.
    RaiseBudget {
        /// The number of raises the project has after this one.
        to: u32,
    },
}

/// What a task is working on, handed to every function.
#[derive(Clone)]
pub struct TaskInput {
    /// The project id.
    pub project: String,
    /// The phase instance (`ph_dc_2`).
    pub phase: String,
    /// The phase's round.
    pub round: u32,
    /// The step's declaration.
    pub step: Arc<StepSpec>,
    /// The input item (`None` for a task over the whole project).
    pub item: Option<Item>,
    /// The group the input belongs to (the question id), empty for none.
    pub sub: String,
    /// The plan in force.
    pub plan: Arc<Plan>,
    /// A merge: the new items this pass takes in.
    pub members: Vec<Item>,
    /// A merge: the items earlier passes of this group already made.
    pub existing: Vec<Item>,
    /// A merge: how many passes this group has had.
    pub pass: u32,
    /// Today, ISO (`2026-10-23`): the date a model is told because its knowledge is old.
    pub today: String,
    /// Read access to the project state.
    pub project_handle: ProjectHandle,
}

impl TaskInput {
    /// The parent a new item of this task hangs under: the question for an item of a question's
    /// tree, the first member's parent for a merge.
    pub fn parent_for_new(&self) -> Option<String> {
        if let Some(m) = self.members.first() {
            return m.parent.clone();
        }
        let it = self.item.as_ref()?;
        if it.kind == "question" {
            Some(it.id.clone())
        } else {
            it.parent.clone()
        }
    }

    /// A new item of this task: id derived from `key_parts`, phase, step, parent and form filled
    /// in, status `done`.
    pub fn new_item(&self, kind: &str, form: &str, key_parts: &[&str]) -> Item {
        let key = key_parts.join("|");
        let mut it = Item::new(crate::ids::item_id(&self.project, kind, &format!("{}:{key}", self.step.id)), kind, form);
        it.phase = self.phase.clone();
        it.step = self.step.id.clone();
        it.parent = self.parent_for_new();
        it.status = crate::state::ItemStatus::Done;
        it
    }
}

/// A model prompt: the shared prefix, then the one item.
#[derive(Debug, Clone, PartialEq)]
pub struct Prompt {
    /// Role card and plan context. Put the long, shared text here and mark it `cache`.
    pub system: Vec<SystemBlock>,
    /// The one item's data, fenced, last.
    pub user: String,
}

/// What a function made.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Finished {
    /// Data to store: on the emitted item of a fan-out with `emit`, else merged into the input
    /// item. Empty means "the model's output as it is".
    pub data: Map<String, Value>,
    /// A new list label for the item.
    pub label: Option<String>,
    /// Further items (a code step's output, a merge's groups). Their phase, step and parent are
    /// filled in when left empty.
    pub emits: Vec<Item>,
    /// Further changes to the project.
    pub effects: Vec<Effect>,
    /// The phase's output (validated against the phase's declared output form).
    pub output: Option<Value>,
    /// Model-facing counts of what was dropped and why (stored in the record, not in the item).
    pub rejected: Option<Value>,
    /// The task read network content: the project is tainted from now on.
    pub tainted: bool,
}

/// Builds a prompt.
pub type PromptFn = Arc<dyn Fn(&TaskInput) -> Result<Prompt, String> + Send + Sync>;
/// Form rules and code-filled fields for a model's valid output. `None` means the employee failed
/// and the function may build a degraded result (a merge makes singletons); `Err` on `Some` tells
/// the model and retries.
pub type FinishFn = Arc<dyn Fn(&TaskInput, Option<Map<String, Value>>) -> Result<Finished, RuleFailure> + Send + Sync>;
/// A step with no model. `Err` is a harness-written reason.
pub type CodeFn = Arc<dyn Fn(&TaskInput, &CancelToken) -> Result<Finished, String> + Send + Sync>;

/// What a gate step builds.
#[derive(Debug, Clone, PartialEq)]
pub struct GateCard {
    /// The card (shown to the user; also the record of what was asked).
    pub payload: Value,
    /// The answers on offer.
    pub options: Vec<ProjectGateOption>,
}

/// What a gate payload function is told.
pub struct GateCtx<'a> {
    /// The state.
    pub state: &'a ProjectState,
    /// The phase instance whose step raises the gate.
    pub phase: &'a Phase,
    /// The step.
    pub step: &'a StepSpec,
    /// Why the gate opens (`plan`, `lead_decision`, `user_forced`, `failures`, `budget`).
    pub reason: &'a str,
}

/// Builds a gate's card from the state.
pub type PayloadFn = Arc<dyn for<'a> Fn(&GateCtx<'a>) -> GateCard + Send + Sync>;
/// Acts on an answered gate: the effects to apply, or a harness-written refusal.
pub type AnswerFn = Arc<dyn Fn(&ProjectState, &Gate, &GateAnswer) -> Result<Vec<Effect>, String> + Send + Sync>;

/// The named functions phase declarations point at.
#[derive(Clone, Default)]
pub struct Toolbox {
    prompts: HashMap<String, PromptFn>,
    finish: HashMap<String, FinishFn>,
    code: HashMap<String, CodeFn>,
    payloads: HashMap<String, PayloadFn>,
    answers: HashMap<String, AnswerFn>,
}

impl Toolbox {
    /// A toolbox holding only the generic functions every project uses (the plan card, the plan
    /// expansion, the checkin card and its answer, `archive`).
    pub fn new() -> Toolbox {
        let mut t = Toolbox::default();
        super::builtin::register(&mut t);
        t
    }

    /// Registers a prompt builder under `name` (a step's `prompt`, else its `role`).
    pub fn prompt(&mut self, name: &str, f: impl Fn(&TaskInput) -> Result<Prompt, String> + Send + Sync + 'static) -> &mut Self {
        self.prompts.insert(name.into(), Arc::new(f));
        self
    }

    /// Registers the rules and code-filled fields of a fan-out or merge under a step's `fn`.
    pub fn finish(&mut self, name: &str, f: impl Fn(&TaskInput, Option<Map<String, Value>>) -> Result<Finished, RuleFailure> + Send + Sync + 'static) -> &mut Self {
        self.finish.insert(name.into(), Arc::new(f));
        self
    }

    /// Registers a code step's function.
    pub fn code(&mut self, name: &str, f: impl Fn(&TaskInput, &CancelToken) -> Result<Finished, String> + Send + Sync + 'static) -> &mut Self {
        self.code.insert(name.into(), Arc::new(f));
        self
    }

    /// Registers a gate's card builder.
    pub fn payload(&mut self, name: &str, f: impl for<'a> Fn(&GateCtx<'a>) -> GateCard + Send + Sync + 'static) -> &mut Self {
        self.payloads.insert(name.into(), Arc::new(f));
        self
    }

    /// Registers a gate's answer handler.
    pub fn answer(&mut self, name: &str, f: impl Fn(&ProjectState, &Gate, &GateAnswer) -> Result<Vec<Effect>, String> + Send + Sync + 'static) -> &mut Self {
        self.answers.insert(name.into(), Arc::new(f));
        self
    }

    /// The prompt builder `name`, if registered.
    pub fn get_prompt(&self, name: &str) -> Option<&PromptFn> {
        self.prompts.get(name)
    }

    /// The finish function `name`.
    pub fn get_finish(&self, name: &str) -> Option<&FinishFn> {
        self.finish.get(name)
    }

    /// The code function `name`.
    pub fn get_code(&self, name: &str) -> Option<&CodeFn> {
        self.code.get(name)
    }

    /// The card builder `name`.
    pub fn get_payload(&self, name: &str) -> Option<&PayloadFn> {
        self.payloads.get(name)
    }

    /// The answer handler `name`.
    pub fn get_answer(&self, name: &str) -> Option<&AnswerFn> {
        self.answers.get(name)
    }

    /// Which functions the menu names that this toolbox does not hold, as `phase.step: kind name`
    /// lines. Prompts are optional (a step without one gets [`default_prompt`]) and so are finish
    /// functions, so only code steps and gates can be missing.
    pub fn missing_for(&self, menu: &std::collections::BTreeMap<String, crate::spec::PhaseSpec>) -> Vec<String> {
        use crate::state::StepKind;
        let mut out = Vec::new();
        for p in menu.values() {
            for s in &p.steps {
                match s.kind {
                    StepKind::Code => {
                        if let Some(f) = &s.func {
                            if !self.code.contains_key(f) {
                                out.push(format!("{}.{}: code function `{f}`", p.phase, s.id));
                            }
                        }
                    }
                    StepKind::Gate => {
                        if let Some(f) = &s.payload_fn {
                            if !self.payloads.contains_key(f) {
                                out.push(format!("{}.{}: gate card `{f}`", p.phase, s.id));
                            }
                        }
                        if let Some(f) = &s.answer_fn {
                            if !self.answers.contains_key(f) {
                                out.push(format!("{}.{}: gate answer `{f}`", p.phase, s.id));
                            }
                        }
                    }
                    StepKind::Fanout | StepKind::Merge => {
                        if let Some(f) = &s.func {
                            if !self.finish.contains_key(f) {
                                out.push(format!("{}.{}: finish function `{f}`", p.phase, s.id));
                            }
                        }
                    }
                }
            }
        }
        out
    }
}

/// The prompt a step gets when no builder is registered for its role: the role, the objective and
/// the fields wanted, then the one item as fenced data. It is enough for a new phase type to run
/// before anyone has tuned its wording.
pub fn default_prompt(input: &TaskInput, form_title: &str) -> Prompt {
    let role = input.step.role.as_deref().unwrap_or("employee");
    let system = format!(
        "You are the {role} on a project. Do exactly one task and answer with the form `{form_title}` only: a JSON object, nothing else.\n\
         Objective: {}\nToday is {}; your knowledge is older than that.",
        fence::clean(&input.plan.objective, 500, false),
        input.today
    );
    let mut user = String::new();
    if let Some(it) = &input.item {
        user.push_str(&fence::fence(&format!("Item `{}` ({})", it.id, it.kind), &Value::Object(it.data.clone()).to_string(), 6000));
    }
    for m in &input.members {
        user.push_str(&fence::fence(&format!("New item `{}`", m.id), &Value::Object(m.data.clone()).to_string(), 1500));
        user.push('\n');
    }
    if user.is_empty() {
        user.push_str("Answer the form for the project as a whole.");
    }
    Prompt { system: vec![SystemBlock { text: system, cache: true }], user }
}
