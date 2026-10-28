//! The project state: one JSON document that holds every phase, item, gate and agent, and is the
//! only thing the screen is derived from.
//!
//! The state is changed only through [`crate::mutation::Mutation`]s applied by one writer; each
//! change bumps `rev`. Model-written data exists only inside `items[].data`; everything else is
//! written by code or comes from the plan.

use std::collections::BTreeMap;

use marlowe_proto::ProjectGateOption;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::form::Form;
use crate::ids;
use crate::plan::Plan;
use crate::registry::Registry;

/// The `schema` value of every state document.
pub const STATE_SCHEMA: &str = "marlowe.project/1";

/// Where a project is in its life.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectStatus {
    /// A plan card is open and nothing has run.
    #[default]
    AwaitingPlan,
    /// Phases are running.
    Running,
    /// A checkin card is open.
    AwaitingCheckin,
    /// Done; the final state is archived.
    Finished,
    /// Stopped by the user; reopenable from a stage.
    Cancelled,
    /// Stopped by the harness.
    Failed,
}

impl ProjectStatus {
    /// Finished, cancelled or failed: nothing will run until it is reopened.
    pub fn is_terminal(self) -> bool {
        matches!(self, ProjectStatus::Finished | ProjectStatus::Cancelled | ProjectStatus::Failed)
    }

    /// Stable lower-case word.
    pub fn as_str(self) -> &'static str {
        match self {
            ProjectStatus::AwaitingPlan => "awaiting_plan",
            ProjectStatus::Running => "running",
            ProjectStatus::AwaitingCheckin => "awaiting_checkin",
            ProjectStatus::Finished => "finished",
            ProjectStatus::Cancelled => "cancelled",
            ProjectStatus::Failed => "failed",
        }
    }
}

/// The status of a phase or a step.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StageStatus {
    /// Not started.
    #[default]
    Pending,
    /// In progress.
    Running,
    /// Finished.
    Done,
    /// Failed.
    Failed,
    /// Not run (a plan that leaves it out, or a cancelled project).
    Skipped,
}

/// The four step primitives (design 4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    /// One fresh-context employee per input item, forced into a form.
    Fanout,
    /// A function with no model.
    Code,
    /// A delta merge at 25/50/75/100% of a watched step.
    Merge,
    /// Parks the run for the user.
    Gate,
}

/// `done` of `total`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Progress {
    /// Finished.
    pub done: u32,
    /// Planned (it may grow while a fan-out discovers work).
    pub total: u32,
}

/// Item counts of a step by status.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counts {
    /// Waiting for a slot.
    pub queued: u32,
    /// In flight.
    pub running: u32,
    /// Finished.
    pub done: u32,
    /// Failed after their attempts.
    pub failed: u32,
}

/// One step of a phase.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StepState {
    /// The step's id within its phase.
    pub id: String,
    /// Which primitive.
    pub kind: StepKind,
    /// The form (`name@version`) a fan-out or merge fills.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub form: Option<String>,
    /// Where it is.
    pub status: StageStatus,
    /// Its items by status.
    pub counts: Counts,
}

/// A phase instance. A continuation adds a new instance of the same type with a higher `round`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Phase {
    /// `ph_dc`, `ph_dc_2`, ...
    pub id: String,
    /// The phase type from the menu.
    #[serde(rename = "type")]
    pub ty: String,
    /// 1 for the first plan's round.
    pub round: u32,
    /// Where it is.
    pub status: StageStatus,
    /// When it started.
    pub started_ms: Option<u64>,
    /// When it ended.
    pub ended_ms: Option<u64>,
    /// Done of total.
    pub progress: Progress,
    /// Its steps.
    pub steps: Vec<StepState>,
    /// The one declared output the next phase reads.
    pub output: Option<Value>,
}

impl Phase {
    /// A pending phase instance of `ty` in `round`.
    pub fn pending(ty: &str, round: u32) -> Phase {
        Phase {
            id: phase_id(ty, round),
            ty: ty.to_string(),
            round,
            status: StageStatus::Pending,
            started_ms: None,
            ended_ms: None,
            progress: Progress::default(),
            steps: Vec::new(),
            output: None,
        }
    }
}

/// The id of a phase instance: `ph_<short>` for round 1, `ph_<short>_<round>` after.
pub fn phase_id(ty: &str, round: u32) -> String {
    let short = match ty {
        "Plan" => "plan",
        "DataCollection" => "dc",
        "DataValidation" => "dv",
        "Thinking" => "think",
        "Checkin" => "check",
        "Report" => "report",
        "Finish" => "finish",
        "Engineering" => "eng",
        other => return format!("ph_{}_{round}", other.to_lowercase().chars().filter(char::is_ascii_alphanumeric).collect::<String>()),
    };
    if round <= 1 {
        format!("ph_{short}")
    } else {
        format!("ph_{short}_{round}")
    }
}

/// The status of an item.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemStatus {
    /// Waiting for its turn.
    #[default]
    Queued,
    /// In flight.
    Running,
    /// Finished; its data conforms to its form.
    Done,
    /// Failed after its attempts (see `error`).
    Failed,
    /// Not run, by choice (a skipped source).
    Skipped,
    /// Refused by a rule (the grant, robots.txt); listed on the next checkin.
    Blocked,
    /// Stopped by a cancel.
    Cancelled,
}

impl ItemStatus {
    /// Stable lower-case word.
    pub fn as_str(self) -> &'static str {
        match self {
            ItemStatus::Queued => "queued",
            ItemStatus::Running => "running",
            ItemStatus::Done => "done",
            ItemStatus::Failed => "failed",
            ItemStatus::Skipped => "skipped",
            ItemStatus::Blocked => "blocked",
            ItemStatus::Cancelled => "cancelled",
        }
    }

    /// The item will not change status by itself.
    pub fn is_final(self) -> bool {
        !matches!(self, ItemStatus::Queued | ItemStatus::Running)
    }
}

/// The code-filled live slots every item has: timestamps, cpu, gpu, size, runtime. Forms read
/// them with `from: live.<key>`; `extra` carries slots a future kind adds (vram, disk).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Live {
    /// When the item started.
    pub started_ms: Option<u64>,
    /// When it ended.
    pub ended_ms: Option<u64>,
    /// Its runtime when stopped (while running the view counts from `started_ms`).
    pub runtime_ms: Option<u64>,
    /// CPU use of its process, percent.
    pub cpu_pct: Option<f64>,
    /// GPU use, percent.
    pub gpu_pct: Option<f64>,
    /// Size on disk, bytes.
    pub size_bytes: Option<u64>,
    /// Other live slots by name.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl Live {
    /// The slot `key` as JSON; `runtime_ms` counts from `started_ms` to `now_ms` while the item
    /// has no stored runtime and no end.
    pub fn get(&self, key: &str, now_ms: u64) -> Option<Value> {
        match key {
            "started_ms" => self.started_ms.map(Value::from),
            "ended_ms" => self.ended_ms.map(Value::from),
            "runtime_ms" => self.runtime(now_ms).map(Value::from),
            "cpu_pct" => self.cpu_pct.map(Value::from),
            "gpu_pct" => self.gpu_pct.map(Value::from),
            "size_bytes" => self.size_bytes.map(Value::from),
            other => self.extra.get(other).filter(|v| !v.is_null()).cloned(),
        }
    }

    /// How long the item has run: the stored runtime, else start to end, else start to now.
    pub fn runtime(&self, now_ms: u64) -> Option<u64> {
        if let Some(r) = self.runtime_ms {
            return Some(r);
        }
        let start = self.started_ms?;
        Some(self.ended_ms.unwrap_or(now_ms).saturating_sub(start))
    }
}

/// One node of the project: a question, a source, a claim, a verdict, a section, an experiment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Item {
    /// `i_<8hex>`, derived from content.
    pub id: String,
    /// What it is (`question`, `source`, `cclaim`, `experiment`, ...); free text, so a kind nobody
    /// registered still exists.
    pub kind: String,
    /// The form (`name@version`) its data follows.
    pub form: String,
    /// The item it hangs under in the tree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// The phase instance that made it.
    #[serde(default)]
    pub phase: String,
    /// The step that made it.
    #[serde(default)]
    pub step: String,
    /// Where it is.
    pub status: ItemStatus,
    /// Short text for lists (code).
    #[serde(default)]
    pub label: String,
    /// The form's data fields. Model-written values live here and nowhere else.
    #[serde(default)]
    pub data: Map<String, Value>,
    /// The live slots.
    #[serde(default)]
    pub live: Live,
    /// The agents assigned to it.
    #[serde(default)]
    pub agents: Vec<String>,
    /// Model attempts so far.
    #[serde(default)]
    pub attempts: u32,
    /// Why it failed: a harness-written reason, never model or server text.
    #[serde(default)]
    pub error: Option<String>,
    /// The journal key of the task that made it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

impl Item {
    /// A queued item of `kind` with `form` and no data yet.
    pub fn new(id: String, kind: &str, form: &str) -> Item {
        Item {
            id,
            kind: kind.to_string(),
            form: form.to_string(),
            parent: None,
            phase: String::new(),
            step: String::new(),
            status: ItemStatus::Queued,
            label: String::new(),
            data: Map::new(),
            live: Live::default(),
            agents: Vec::new(),
            attempts: 0,
            error: None,
            key: None,
        }
    }
}

/// Which gate: the plan card or a checkin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateKind {
    /// The plan awaits approval.
    Plan,
    /// The user decides what happens next.
    Checkin,
}

impl GateKind {
    /// Stable lower-case word.
    pub fn as_str(self) -> &'static str {
        match self {
            GateKind::Plan => "plan",
            GateKind::Checkin => "checkin",
        }
    }

    /// The options a card of this kind offers. `flagged` is the number of flagged additional
    /// sources a checkin could chase (0 leaves "keep researching" out of a plan card and names no
    /// count on a checkin).
    pub fn options(self, flagged: u32) -> Vec<ProjectGateOption> {
        let opt = |id: &str, label: String, needs_text: bool| ProjectGateOption { id: id.into(), label, needs_text };
        match self {
            GateKind::Plan => vec![opt("approve", "Approve the plan".into(), false), opt("cancel", "Cancel".into(), false)],
            GateKind::Checkin => vec![
                opt("approve", "Approve and write the report".into(), false),
                opt(
                    "keep_researching",
                    if flagged > 0 { format!("Keep researching ({flagged} flagged sources)") } else { "Keep researching".into() },
                    false,
                ),
                opt("steer", "Steer".into(), true),
                opt("cancel", "Cancel".into(), false),
            ],
        }
    }
}

/// Whether a gate waits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateStatus {
    /// Waiting for the user.
    Open,
    /// Answered.
    Answered,
}

/// The user's answer to a gate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateAnswer {
    /// The chosen option id.
    pub choice: String,
    /// The user's text (steer), length-capped by the caller.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// A step that parks the run for the user.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Gate {
    /// `g_1`, `g_2`, ...
    pub id: String,
    /// Plan or checkin.
    pub kind: GateKind,
    /// Open or answered.
    pub status: GateStatus,
    /// The card, code-built.
    pub payload: Value,
    /// What the user may answer.
    pub options: Vec<ProjectGateOption>,
    /// The answer, once given.
    pub answer: Option<GateAnswer>,
    /// When it opened.
    pub asked_ms: u64,
    /// When it was answered.
    pub answered_ms: Option<u64>,
    /// The phase instance whose step raised it (empty for a gate made outside the engine).
    #[serde(default)]
    pub phase: String,
    /// The step that raised it.
    #[serde(default)]
    pub step: String,
}

/// Where an agent is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    /// Waiting for a place to call from: a model slot, the GPU, memory (`Agent::note` says which).
    Waiting,
    /// In a call.
    Running,
    /// Returned a valid form.
    Done,
    /// Gave up.
    Failed,
}

/// Token counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tokens {
    /// Prompt tokens.
    #[serde(rename = "in")]
    pub input: u64,
    /// Output tokens.
    pub out: u64,
}

/// An employee (or the lead): one task, one fresh context.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Agent {
    /// `a_<n>`.
    pub id: String,
    /// The role card (`analyst`, `verifier`, ...).
    pub role: String,
    /// The model it ran on.
    pub model: String,
    /// The step it belongs to.
    pub step: String,
    /// The item it works on.
    pub item: Option<String>,
    /// Where it is.
    pub status: AgentStatus,
    /// The attempt number.
    pub attempt: u32,
    /// Tokens used.
    pub tokens: Tokens,
    /// When it started.
    pub started_ms: u64,
    /// When it ended.
    pub ended_ms: Option<u64>,
    /// What a `waiting` agent waits for ("waiting for the GPU"); cleared when its call starts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Fetch outcomes so far.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchCounts {
    /// Pages read.
    pub ok: u32,
    /// Pages that failed.
    pub failed: u32,
    /// Fetches refused by a rule.
    pub blocked: u32,
}

/// Project-wide live numbers (code-filled).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Metrics {
    /// CPU use, percent.
    pub cpu_pct: f64,
    /// GPU use, percent.
    pub gpu_pct: f64,
    /// VRAM in use, MB.
    pub vram_mb: u64,
    /// Free disk, MB.
    pub disk_free_mb: u64,
    /// Fetch outcomes.
    pub fetch: FetchCounts,
    /// Tokens used by every agent.
    pub tokens: Tokens,
    /// Milliseconds the project has spent running, parked time excluded (the minutes budget).
    #[serde(default)]
    pub active_ms: u64,
    /// How many times the user raised a budget at a budget checkin (each adds one quote).
    #[serde(default)]
    pub budget_raises: u32,
}

/// The run-scoped network grant the user gives by approving the plan (design 4.9).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetGrant {
    /// `run`.
    pub scope: String,
    /// Public https GETs of pages the run found are allowed.
    pub public_https_get: bool,
    /// robots.txt is honoured.
    pub robots: bool,
    /// Most fetches.
    pub quota: u32,
    /// Fetches used.
    pub used: u32,
}

/// What the user has granted the run.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grants {
    /// The network grant, once the plan is approved.
    #[serde(default)]
    pub net: Option<NetGrant>,
}

/// The decision made about a plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanRecord {
    /// Which plan.
    pub plan_no: u32,
    /// When it was approved.
    pub approved_ms: Option<u64>,
    /// `approve`, `keep_researching`, `steer`, ...
    pub decision: Option<String>,
}

/// The whole project, as `project.json` holds it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectState {
    /// [`STATE_SCHEMA`].
    pub schema: String,
    /// `p_<8hex>`.
    pub id: String,
    /// The project kind.
    pub kind: String,
    /// A short name.
    pub title: String,
    /// The conversation that started it.
    pub session: Option<String>,
    /// The durable run carrying it.
    pub run: Option<String>,
    /// Where it is.
    pub status: ProjectStatus,
    /// Bumped by every change.
    pub rev: u64,
    /// When it was made.
    pub created_ms: u64,
    /// When it last changed.
    pub updated_ms: u64,
    /// When it ended.
    pub finished_ms: Option<u64>,
    /// The current plan.
    pub plan: Plan,
    /// The decisions made about plans.
    pub plans: Vec<PlanRecord>,
    /// The forms in force, by `name@version`. Embedded so the state is self-describing.
    pub forms: BTreeMap<String, Form>,
    /// Phase instances in order.
    pub phases: Vec<Phase>,
    /// Every item by id.
    pub items: BTreeMap<String, Item>,
    /// Item ids by kind, in creation order (the display order).
    pub order: BTreeMap<String, Vec<String>>,
    /// Gates in the order they opened.
    pub gates: Vec<Gate>,
    /// Every agent by id.
    pub agents: BTreeMap<String, Agent>,
    /// Live project numbers.
    pub metrics: Metrics,
    /// What the user granted.
    pub grants: Grants,
    /// Set from the first network result and never cleared (design 4.9).
    pub tainted: bool,
}

impl ProjectState {
    /// A new project from a validated plan: status `awaiting_plan`, the kind's forms embedded,
    /// the plan's phases pending (the Plan phase running), one `question` item per question.
    pub fn new(id: String, plan: Plan, registry: &Registry, now_ms: u64) -> ProjectState {
        let forms = registry.forms_for(&plan.kind);
        let mut phases: Vec<Phase> = plan.phases.iter().map(|p| Phase::pending(p, 1)).collect();
        if let Some(p) = phases.iter_mut().find(|p| p.ty == "Plan") {
            p.status = StageStatus::Running;
            p.started_ms = Some(now_ms);
        }
        let mut st = ProjectState {
            schema: STATE_SCHEMA.into(),
            id,
            kind: plan.kind.clone(),
            title: plan.title.clone(),
            session: None,
            run: None,
            status: ProjectStatus::AwaitingPlan,
            rev: 1,
            created_ms: now_ms,
            updated_ms: now_ms,
            finished_ms: None,
            plans: vec![PlanRecord { plan_no: plan.plan_no, approved_ms: None, decision: None }],
            forms,
            phases,
            items: BTreeMap::new(),
            order: BTreeMap::new(),
            gates: Vec::new(),
            agents: BTreeMap::new(),
            metrics: Metrics::default(),
            grants: Grants::default(),
            tainted: false,
            plan,
        };
        st.add_question_items();
        st
    }

    fn add_question_items(&mut self) {
        if !self.forms.contains_key("question@1") {
            return;
        }
        let phase = self.phases.iter().find(|p| p.ty == "Plan").map(|p| p.id.clone()).unwrap_or_default();
        for q in self.plan.questions.clone() {
            let mut it = Item::new(ids::item_id(&self.id, "question", &q.id), "question", "question@1");
            it.phase = phase.clone();
            it.step = "expand".into();
            it.status = ItemStatus::Done;
            it.label = q.text.clone();
            it.data.insert("id".into(), Value::String(q.id.clone()));
            it.data.insert("text".into(), Value::String(q.text.clone()));
            self.order.entry("question".into()).or_default().push(it.id.clone());
            self.items.insert(it.id.clone(), it);
        }
    }

    /// The item with this id.
    pub fn item(&self, id: &str) -> Option<&Item> {
        self.items.get(id)
    }

    /// The items of a kind in display order.
    pub fn items_of_kind(&self, kind: &str) -> Vec<&Item> {
        self.order.get(kind).into_iter().flatten().filter_map(|id| self.items.get(id)).collect()
    }

    /// The children of an item in display order (by kind, then creation order).
    pub fn children(&self, parent: &str) -> Vec<&Item> {
        self.order
            .values()
            .flatten()
            .filter_map(|id| self.items.get(id))
            .filter(|i| i.parent.as_deref() == Some(parent))
            .collect()
    }

    /// The items with no parent in display order: the top of the tree.
    pub fn roots(&self) -> Vec<&Item> {
        self.order.values().flatten().filter_map(|id| self.items.get(id)).filter(|i| i.parent.is_none()).collect()
    }

    /// The phase running now, else the first pending one, else the last.
    pub fn current_phase(&self) -> Option<&Phase> {
        self.phases
            .iter()
            .find(|p| p.status == StageStatus::Running)
            .or_else(|| self.phases.iter().find(|p| p.status == StageStatus::Pending))
            .or_else(|| self.phases.last())
    }

    /// The gate that waits for the user, if any.
    pub fn open_gate(&self) -> Option<&Gate> {
        self.gates.iter().find(|g| g.status == GateStatus::Open)
    }

    /// The phase with this id.
    pub fn phase(&self, id: &str) -> Option<&Phase> {
        self.phases.iter().find(|p| p.id == id)
    }

    /// The state as a JSON value.
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn sample() -> ProjectState {
        ProjectState::new("p_7f3a91c2".into(), crate::plan::test_plan(), &Registry::builtin(), 1_000)
    }

    #[test]
    fn a_new_project_awaits_its_plan_with_forms_phases_and_questions() {
        let st = sample();
        assert_eq!(st.schema, STATE_SCHEMA);
        assert_eq!(st.status, ProjectStatus::AwaitingPlan);
        assert_eq!(st.rev, 1);
        assert_eq!(st.kind, "research");
        assert!(st.forms.contains_key("analyze@1") && st.forms.contains_key("question@1"));
        assert!(!st.forms.contains_key("experiment@1"), "only the kind's forms are embedded");
        let names: Vec<&str> = st.phases.iter().map(|p| p.ty.as_str()).collect();
        assert_eq!(names, ["Plan", "DataCollection", "DataValidation", "Thinking", "Report", "Finish"]);
        assert_eq!(st.phases[0].status, StageStatus::Running);
        assert_eq!(st.phases[1].id, "ph_dc");
        assert_eq!(st.items_of_kind("question").len(), 3);
        assert_eq!(st.roots().len(), 3);
        assert_eq!(st.current_phase().unwrap().id, "ph_plan");
        assert!(st.open_gate().is_none());
    }

    #[test]
    fn state_round_trips_through_json_equal() {
        let mut st = sample();
        let mut it = Item::new("i_00000001".into(), "source", "analyze@1");
        it.live.extra.insert("vram_mb".into(), serde_json::json!(512));
        it.data.insert("index".into(), serde_json::json!(1));
        st.order.entry("source".into()).or_default().push(it.id.clone());
        st.items.insert(it.id.clone(), it);
        st.gates.push(Gate {
            id: "g_1".into(),
            kind: GateKind::Plan,
            status: GateStatus::Open,
            payload: serde_json::json!({"questions": 3}),
            options: GateKind::Plan.options(0),
            answer: None,
            asked_ms: 5,
            answered_ms: None,
        phase: String::new(),
        step: String::new(),
        });
        st.grants.net = Some(NetGrant { scope: "run".into(), public_https_get: true, robots: true, quota: 60, used: 3 });
        let s = serde_json::to_string(&st).unwrap();
        let back: ProjectState = serde_json::from_str(&s).unwrap();
        assert_eq!(back, st);
        assert_eq!(back.to_value(), st.to_value());
        // The documented key names.
        let v = st.to_value();
        for k in ["schema", "id", "kind", "title", "session", "run", "status", "rev", "created_ms", "updated_ms", "finished_ms", "plan", "plans", "forms", "phases", "items", "order", "gates", "agents", "metrics", "grants", "tainted"] {
            assert!(v.get(k).is_some(), "{k}");
        }
        assert_eq!(v["phases"][0]["type"], "Plan");
        assert_eq!(v["metrics"]["tokens"]["in"], 0);
    }

    #[test]
    fn live_runtime_counts_from_the_start_while_running() {
        let mut l = Live::default();
        assert_eq!(l.runtime(10_000), None);
        l.started_ms = Some(4_000);
        assert_eq!(l.runtime(10_000), Some(6_000));
        l.ended_ms = Some(7_000);
        assert_eq!(l.runtime(10_000), Some(3_000));
        l.runtime_ms = Some(2_500);
        assert_eq!(l.get("runtime_ms", 0), Some(serde_json::json!(2_500)));
        assert_eq!(l.get("cpu_pct", 0), None);
        l.extra.insert("vram_mb".into(), serde_json::json!(8));
        assert_eq!(l.get("vram_mb", 0), Some(serde_json::json!(8)));
    }

    #[test]
    fn phase_ids_carry_the_round() {
        assert_eq!(phase_id("DataCollection", 1), "ph_dc");
        assert_eq!(phase_id("DataCollection", 2), "ph_dc_2");
        assert_eq!(phase_id("Plan", 2), "ph_plan_2");
        assert_eq!(phase_id("Mystery Phase", 1), "ph_mysteryphase_1");
    }

    #[test]
    fn gate_options_name_the_flagged_count() {
        let o = GateKind::Checkin.options(7);
        assert_eq!(o.iter().map(|x| x.id.as_str()).collect::<Vec<_>>(), ["approve", "keep_researching", "steer", "cancel"]);
        assert_eq!(o[1].label, "Keep researching (7 flagged sources)");
        assert!(o[2].needs_text && !o[0].needs_text);
        assert_eq!(GateKind::Plan.options(0).len(), 2);
    }
}
