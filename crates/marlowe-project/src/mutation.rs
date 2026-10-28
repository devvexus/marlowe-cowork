//! Mutations: the only way a project state changes.
//!
//! One writer applies [`Mutation`]s in order ([`ProjectState::apply`]). Each application checks
//! its references, bumps `rev`, and returns the [`Applied`] patches: RFC 6901 set and remove
//! operations that take a copy of the state from `base` to `rev`. Patches are computed by diffing
//! the touched parts of the state before and after, so a mutation cannot forget to report a
//! change. Arrays are replaced whole.
//!
//! A mutation that would leave a `done` item whose data breaks its form is refused: an item is
//! done only when its JSON is exact to how its form expects it.

use marlowe_contract::schema;
use marlowe_proto::{PatchOp, ProjectPatch};
use serde_json::{Map, Value};
use thiserror::Error;

use crate::form::{data_schema, Form};
use crate::state::*;

/// Why a mutation was refused. The state is unchanged when one is returned.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum MutationError {
    /// No phase has this id.
    #[error("no phase `{0}`")]
    UnknownPhase(String),
    /// No item has this id.
    #[error("no item `{0}`")]
    UnknownItem(String),
    /// An item names a parent that does not exist.
    #[error("item `{item}` names parent `{parent}`, which does not exist")]
    UnknownParent {
        /// The item.
        item: String,
        /// The missing parent.
        parent: String,
    },
    /// No gate has this id.
    #[error("no gate `{0}`")]
    UnknownGate(String),
    /// The gate was already answered.
    #[error("gate `{0}` is not open")]
    GateNotOpen(String),
    /// Another gate is open; a project parks on one at a time.
    #[error("gate `{0}` is still open")]
    GateAlreadyOpen(String),
    /// The answer is not one of the gate's options.
    #[error("`{choice}` is not an option of gate `{gate}`")]
    BadChoice {
        /// The gate.
        gate: String,
        /// What was sent.
        choice: String,
    },
    /// The option needs the user's text and none came.
    #[error("option `{0}` needs text")]
    NeedsText(String),
    /// A `done` item's data does not match its form.
    #[error("item `{item}` is not valid for form `{form}`: {}", .errors.join("; "))]
    Invalid {
        /// The item.
        item: String,
        /// Its form.
        form: String,
        /// What is wrong, as JSON-pointer lines.
        errors: Vec<String>,
    },
    /// The gate is not the project's current one (stale answer).
    #[error("{0}")]
    Other(String),
}

/// Fields of a phase to change; `None` leaves a field alone.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PhaseUpdate {
    /// New status.
    pub status: Option<StageStatus>,
    /// Start time.
    pub started_ms: Option<u64>,
    /// End time.
    pub ended_ms: Option<u64>,
    /// New progress.
    pub progress: Option<Progress>,
    /// The whole step list.
    pub steps: Option<Vec<StepState>>,
    /// The phase's output.
    pub output: Option<Value>,
}

/// Live slots to set; `None` leaves a slot alone.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LiveUpdate {
    /// Start time.
    pub started_ms: Option<u64>,
    /// End time.
    pub ended_ms: Option<u64>,
    /// Stored runtime.
    pub runtime_ms: Option<u64>,
    /// CPU percent.
    pub cpu_pct: Option<f64>,
    /// GPU percent.
    pub gpu_pct: Option<f64>,
    /// Size in bytes.
    pub size_bytes: Option<u64>,
    /// Other slots.
    pub extra: Vec<(String, Value)>,
}

/// Fields of an item to change; `None` leaves a field alone.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ItemUpdate {
    /// New status.
    pub status: Option<ItemStatus>,
    /// New list label.
    pub label: Option<String>,
    /// Data keys to set (an existing key is replaced; a `null` value is stored as null).
    pub data: Option<Map<String, Value>>,
    /// Live slots to set.
    pub live: Option<LiveUpdate>,
    /// The whole assigned-agent list.
    pub agents: Option<Vec<String>>,
    /// Attempts so far.
    pub attempts: Option<u32>,
    /// Set (`Some(Some(..))`) or clear (`Some(None)`) the failure reason.
    pub error: Option<Option<String>>,
    /// The journal key.
    pub key: Option<String>,
}

/// One change to the state.
#[derive(Debug, Clone, PartialEq)]
pub enum Mutation {
    /// Set the project status (a terminal status also stamps `finished_ms`).
    SetStatus(ProjectStatus),
    /// Attach the conversation and the durable run.
    Bind {
        /// The session id.
        session: Option<String>,
        /// The run id.
        run: Option<String>,
    },
    /// Replace the current plan (a revision, or a continuation).
    SetPlan(Box<crate::plan::Plan>),
    /// Record a decision about a plan.
    PushPlanRecord(PlanRecord),
    /// Embed a form (replaces the one with the same id).
    RegisterForm(Box<Form>),
    /// Append a phase instance.
    PushPhase(Phase),
    /// Insert a phase instance before the phase `before` (appended when `before` is `None` or no
    /// longer exists). A continuation round and a checkin go in ahead of the Report and Finish
    /// phases that are already waiting.
    InsertPhase {
        /// The phase id to go in front of.
        before: Option<String>,
        /// The new instance.
        phase: Phase,
    },
    /// Change a phase.
    UpdatePhase {
        /// The phase id.
        id: String,
        /// What to change.
        update: PhaseUpdate,
    },
    /// Insert or replace an item (a new one is added to the display order of its kind).
    PutItem(Box<Item>),
    /// Change an item.
    UpdateItem {
        /// The item id.
        id: String,
        /// What to change.
        update: ItemUpdate,
    },
    /// Assign an agent to an item (idempotent).
    AssignAgent {
        /// The item id.
        item: String,
        /// The agent id.
        agent: String,
    },
    /// Insert or replace an agent.
    PutAgent(Box<Agent>),
    /// Open a gate.
    PushGate(Box<Gate>),
    /// Answer the open gate `id`.
    AnswerGate {
        /// The gate id.
        id: String,
        /// The chosen option.
        choice: String,
        /// The user's text, if the option takes one.
        text: Option<String>,
    },
    /// Replace the live project numbers.
    SetMetrics(Metrics),
    /// Replace the grants.
    SetGrants(Grants),
    /// The machine's load: the three numbers a probe reads while the engine runs. Nothing else of
    /// `metrics` changes.
    SetMachine {
        /// CPU in use, percent of the machine.
        cpu_pct: f64,
        /// GPU in use, percent.
        gpu_pct: f64,
        /// GPU memory in use, MB.
        vram_mb: u64,
    },
    /// What the engine owns of the live numbers: tokens, running time and budget raises. The
    /// other numbers (fetch counts, machine load) are left as they are.
    SetRunMetrics {
        /// Tokens used by every agent.
        tokens: crate::state::Tokens,
        /// Milliseconds the project has spent running.
        active_ms: u64,
        /// Times the user raised a budget.
        budget_raises: u32,
    },
    /// Count page reads: added to the fetch outcomes, and `used` to the network grant's count.
    /// Atomic, so parallel fetches cannot lose each other's counts.
    AddFetch {
        /// Pages read.
        ok: u32,
        /// Pages that failed.
        failed: u32,
        /// Fetches a rule refused.
        blocked: u32,
        /// Fetches charged to the grant's quota.
        used: u32,
    },
    /// Latch the taint flag (setting `false` is ignored: taint is never cleared).
    SetTainted(bool),
}

/// What an applied mutation did to the document.
#[derive(Debug, Clone, PartialEq)]
pub struct Applied {
    /// The revision before.
    pub base: u64,
    /// The revision after.
    pub rev: u64,
    /// The patches from `base` to `rev`.
    pub patches: Vec<ProjectPatch>,
}

/// A part of the state document a mutation touches: the unit the diff is taken over.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Part {
    Top(&'static str),
    Item(String),
    Agent(String),
    Form(String),
    Order(String),
}

/// Escapes one JSON-pointer segment (RFC 6901).
pub fn escape_segment(s: &str) -> String {
    s.replace('~', "~0").replace('/', "~1")
}

impl Part {
    fn path(&self) -> String {
        match self {
            Part::Top(k) => format!("/{k}"),
            Part::Item(id) => format!("/items/{}", escape_segment(id)),
            Part::Agent(id) => format!("/agents/{}", escape_segment(id)),
            Part::Form(id) => format!("/forms/{}", escape_segment(id)),
            Part::Order(k) => format!("/order/{}", escape_segment(k)),
        }
    }
}

fn to_v<T: serde::Serialize>(t: &T) -> Option<Value> {
    serde_json::to_value(t).ok()
}

impl ProjectState {
    fn part_value(&self, p: &Part) -> Option<Value> {
        match p {
            Part::Top(k) => match *k {
                "status" => to_v(&self.status),
                "finished_ms" => to_v(&self.finished_ms),
                "session" => to_v(&self.session),
                "run" => to_v(&self.run),
                "plan" => to_v(&self.plan),
                "plans" => to_v(&self.plans),
                "phases" => to_v(&self.phases),
                "gates" => to_v(&self.gates),
                "metrics" => to_v(&self.metrics),
                "grants" => to_v(&self.grants),
                "tainted" => to_v(&self.tainted),
                "title" => to_v(&self.title),
                _ => None,
            },
            Part::Item(id) => self.items.get(id).and_then(to_v),
            Part::Agent(id) => self.agents.get(id).and_then(to_v),
            Part::Form(id) => self.forms.get(id).and_then(to_v),
            Part::Order(k) => self.order.get(k).and_then(to_v),
        }
    }

    fn parts_of(m: &Mutation) -> Vec<Part> {
        match m {
            Mutation::SetStatus(_) => vec![Part::Top("status"), Part::Top("finished_ms")],
            Mutation::Bind { .. } => vec![Part::Top("session"), Part::Top("run")],
            Mutation::SetPlan(_) => vec![Part::Top("plan"), Part::Top("title")],
            Mutation::PushPlanRecord(_) => vec![Part::Top("plans")],
            Mutation::RegisterForm(f) => vec![Part::Form(f.id())],
            Mutation::PushPhase(_) | Mutation::InsertPhase { .. } | Mutation::UpdatePhase { .. } => vec![Part::Top("phases")],
            Mutation::PutItem(i) => vec![Part::Item(i.id.clone()), Part::Order(i.kind.clone())],
            Mutation::UpdateItem { id, .. } => vec![Part::Item(id.clone())],
            Mutation::AssignAgent { item, .. } => vec![Part::Item(item.clone())],
            Mutation::PutAgent(a) => vec![Part::Agent(a.id.clone())],
            Mutation::PushGate(_) | Mutation::AnswerGate { .. } => vec![Part::Top("gates")],
            Mutation::SetMetrics(_) => vec![Part::Top("metrics")],
            Mutation::SetGrants(_) => vec![Part::Top("grants")],
            Mutation::SetRunMetrics { .. } | Mutation::SetMachine { .. } => vec![Part::Top("metrics")],
            Mutation::AddFetch { .. } => vec![Part::Top("metrics"), Part::Top("grants")],
            Mutation::SetTainted(_) => vec![Part::Top("tainted")],
        }
    }

    /// The errors that keep a `done` item from being stored: its data against the form it names.
    /// An item whose form is not embedded in the state is not checked (a kind nobody registered
    /// still exists); an item that is not `done` is not checked (it is still being filled).
    pub fn validate_item(&self, item: &Item) -> Vec<String> {
        if item.status != ItemStatus::Done {
            return Vec::new();
        }
        let Some(form) = self.forms.get(&item.form) else { return Vec::new() };
        schema::validate(&Value::Object(item.data.clone()), &data_schema(form, &self.plan))
    }

    /// Applies one mutation. On success the revision is bumped, `updated_ms` is `now_ms`, and the
    /// patches that describe the change are returned; on error nothing changed.
    pub fn apply(&mut self, m: Mutation, now_ms: u64) -> Result<Applied, MutationError> {
        let parts = Self::parts_of(&m);
        let before: Vec<Option<Value>> = parts.iter().map(|p| self.part_value(p)).collect();
        self.exec(m, now_ms)?;
        let base = self.rev;
        self.rev += 1;
        self.updated_ms = now_ms;
        let mut patches = Vec::new();
        for (p, old) in parts.iter().zip(before) {
            diff(&p.path(), old.as_ref(), self.part_value(p).as_ref(), &mut patches);
        }
        patches.push(ProjectPatch::set("/rev", Value::from(self.rev)));
        patches.push(ProjectPatch::set("/updated_ms", Value::from(now_ms)));
        Ok(Applied { base, rev: self.rev, patches })
    }

    fn exec(&mut self, m: Mutation, now_ms: u64) -> Result<(), MutationError> {
        match m {
            Mutation::SetStatus(s) => {
                self.status = s;
                if s.is_terminal() {
                    self.finished_ms = Some(now_ms);
                } else {
                    self.finished_ms = None;
                }
            }
            Mutation::Bind { session, run } => {
                self.session = session;
                self.run = run;
            }
            Mutation::SetPlan(p) => {
                self.title = p.title.clone();
                self.plan = *p;
            }
            Mutation::PushPlanRecord(r) => match self.plans.iter_mut().find(|x| x.plan_no == r.plan_no) {
                Some(x) => *x = r,
                None => self.plans.push(r),
            },
            Mutation::RegisterForm(f) => {
                self.forms.insert(f.id(), *f);
            }
            Mutation::PushPhase(p) => {
                if self.phases.iter().any(|x| x.id == p.id) {
                    return Err(MutationError::Other(format!("phase `{}` already exists", p.id)));
                }
                self.phases.push(p);
            }
            Mutation::InsertPhase { before, phase } => {
                if self.phases.iter().any(|x| x.id == phase.id) {
                    return Err(MutationError::Other(format!("phase `{}` already exists", phase.id)));
                }
                match before.and_then(|b| self.phases.iter().position(|x| x.id == b)) {
                    Some(at) => self.phases.insert(at, phase),
                    None => self.phases.push(phase),
                }
            }
            Mutation::UpdatePhase { id, update } => {
                let p = self.phases.iter_mut().find(|p| p.id == id).ok_or(MutationError::UnknownPhase(id))?;
                if let Some(s) = update.status {
                    p.status = s;
                }
                if let Some(t) = update.started_ms {
                    p.started_ms = Some(t);
                }
                if let Some(t) = update.ended_ms {
                    p.ended_ms = Some(t);
                }
                if let Some(pr) = update.progress {
                    p.progress = pr;
                }
                if let Some(s) = update.steps {
                    p.steps = s;
                }
                if let Some(o) = update.output {
                    p.output = Some(o);
                }
            }
            Mutation::PutItem(item) => {
                if let Some(parent) = &item.parent {
                    if !self.items.contains_key(parent) {
                        return Err(MutationError::UnknownParent { item: item.id.clone(), parent: parent.clone() });
                    }
                }
                let errors = self.validate_item(&item);
                if !errors.is_empty() {
                    return Err(MutationError::Invalid { item: item.id.clone(), form: item.form.clone(), errors });
                }
                let ids = self.order.entry(item.kind.clone()).or_default();
                if !ids.contains(&item.id) {
                    ids.push(item.id.clone());
                }
                self.items.insert(item.id.clone(), *item);
            }
            Mutation::UpdateItem { id, update } => {
                let mut item = self.items.get(&id).cloned().ok_or_else(|| MutationError::UnknownItem(id.clone()))?;
                if let Some(s) = update.status {
                    item.status = s;
                }
                if let Some(l) = update.label {
                    item.label = l;
                }
                if let Some(d) = update.data {
                    for (k, v) in d {
                        item.data.insert(k, v);
                    }
                }
                if let Some(l) = update.live {
                    apply_live(&mut item.live, l);
                }
                if let Some(a) = update.agents {
                    item.agents = a;
                }
                if let Some(n) = update.attempts {
                    item.attempts = n;
                }
                if let Some(e) = update.error {
                    item.error = e;
                }
                if let Some(k) = update.key {
                    item.key = Some(k);
                }
                let errors = self.validate_item(&item);
                if !errors.is_empty() {
                    return Err(MutationError::Invalid { item: id, form: item.form.clone(), errors });
                }
                self.items.insert(id, item);
            }
            Mutation::AssignAgent { item, agent } => {
                let it = self.items.get_mut(&item).ok_or(MutationError::UnknownItem(item))?;
                if !it.agents.contains(&agent) {
                    it.agents.push(agent);
                }
            }
            Mutation::PutAgent(a) => {
                self.agents.insert(a.id.clone(), *a);
            }
            Mutation::PushGate(g) => {
                if let Some(open) = self.open_gate() {
                    return Err(MutationError::GateAlreadyOpen(open.id.clone()));
                }
                if self.gates.iter().any(|x| x.id == g.id) {
                    return Err(MutationError::Other(format!("gate `{}` already exists", g.id)));
                }
                self.gates.push(*g);
            }
            Mutation::AnswerGate { id, choice, text } => {
                let g = self.gates.iter_mut().find(|g| g.id == id).ok_or_else(|| MutationError::UnknownGate(id.clone()))?;
                if g.status != GateStatus::Open {
                    return Err(MutationError::GateNotOpen(id));
                }
                let opt = g.options.iter().find(|o| o.id == choice).ok_or_else(|| MutationError::BadChoice { gate: id.clone(), choice: choice.clone() })?;
                let text = text.map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
                if opt.needs_text && text.is_none() {
                    return Err(MutationError::NeedsText(choice));
                }
                g.status = GateStatus::Answered;
                g.answer = Some(GateAnswer { choice, text });
                g.answered_ms = Some(now_ms);
            }
            Mutation::SetMetrics(m) => self.metrics = m,
            Mutation::SetGrants(g) => self.grants = g,
            Mutation::SetMachine { cpu_pct, gpu_pct, vram_mb } => {
                self.metrics.cpu_pct = cpu_pct;
                self.metrics.gpu_pct = gpu_pct;
                self.metrics.vram_mb = vram_mb;
            }
            Mutation::SetRunMetrics { tokens, active_ms, budget_raises } => {
                self.metrics.tokens = tokens;
                self.metrics.active_ms = active_ms;
                self.metrics.budget_raises = budget_raises;
            }
            Mutation::AddFetch { ok, failed, blocked, used } => {
                self.metrics.fetch.ok += ok;
                self.metrics.fetch.failed += failed;
                self.metrics.fetch.blocked += blocked;
                if let Some(n) = self.grants.net.as_mut() {
                    n.used += used;
                }
            }
            Mutation::SetTainted(t) => self.tainted = self.tainted || t,
        }
        Ok(())
    }
}

fn apply_live(live: &mut Live, u: LiveUpdate) {
    if let Some(v) = u.started_ms {
        live.started_ms = Some(v);
    }
    if let Some(v) = u.ended_ms {
        live.ended_ms = Some(v);
    }
    if let Some(v) = u.runtime_ms {
        live.runtime_ms = Some(v);
    }
    if let Some(v) = u.cpu_pct {
        live.cpu_pct = Some(v);
    }
    if let Some(v) = u.gpu_pct {
        live.gpu_pct = Some(v);
    }
    if let Some(v) = u.size_bytes {
        live.size_bytes = Some(v);
    }
    for (k, v) in u.extra {
        live.extra.insert(k, v);
    }
}

/// Appends the patches that turn `old` into `new` at `path`. Objects are compared key by key;
/// everything else (arrays included) is replaced whole.
pub fn diff(path: &str, old: Option<&Value>, new: Option<&Value>, out: &mut Vec<ProjectPatch>) {
    match (old, new) {
        (None, None) => {}
        (Some(_), None) => out.push(ProjectPatch { op: PatchOp::Remove, path: path.to_string(), value: None }),
        (None, Some(n)) => out.push(ProjectPatch::set(path, n.clone())),
        (Some(Value::Object(o)), Some(Value::Object(n))) => {
            for (k, nv) in n {
                diff(&format!("{path}/{}", escape_segment(k)), o.get(k), Some(nv), out);
            }
            for k in o.keys() {
                if !n.contains_key(k) {
                    out.push(ProjectPatch::remove(format!("{path}/{}", escape_segment(k))));
                }
            }
        }
        (Some(o), Some(n)) => {
            if o != n {
                out.push(ProjectPatch::set(path, n.clone()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::replica::apply_patches;
    use crate::state::tests::sample;

    fn source_item(st: &ProjectState, n: u32) -> Item {
        let mut it = Item::new(crate::ids::item_id(&st.id, "source", &format!("k{n}")), "source", "analyze@1");
        it.parent = Some(st.items_of_kind("question")[0].id.clone());
        it.phase = "ph_dc".into();
        it.step = "analyze".into();
        it
    }

    fn done_data() -> Map<String, Value> {
        json!({
            "index": 1, "title": "Kimi K3", "relevancy": "High", "source_kind": "official",
            "one_line": "Kimi K3 released", "source": "https://example.org/kimi", "notes": "short",
            "claims": [{"text": "K3 is out", "quote": "K3 is out today", "sub": "q1"}],
            "additional_sources": [],
            "records": [{"model_name": "Kimi K3", "vendor": "Moonshot", "status": "released", "release_date": null, "size": null}]
        })
        .as_object()
        .unwrap()
        .clone()
    }

    #[test]
    fn every_mutation_bumps_rev_and_reports_patches_that_replay_to_the_same_state() {
        let mut st = sample();
        let start = st.to_value();
        let mut doc = start.clone();
        let mut all = Vec::new();
        let it = source_item(&st, 1);
        let id = it.id.clone();
        let muts = vec![
            Mutation::Bind { session: Some("s_1".into()), run: Some("r_1".into()) },
            Mutation::PutItem(Box::new(it)),
            Mutation::UpdateItem {
                id: id.clone(),
                update: ItemUpdate {
                    status: Some(ItemStatus::Running),
                    live: Some(LiveUpdate { started_ms: Some(2_000), cpu_pct: Some(41.0), ..LiveUpdate::default() }),
                    attempts: Some(1),
                    ..ItemUpdate::default()
                },
            },
            Mutation::PutAgent(Box::new(Agent {
                id: "a_1".into(),
                role: "analyst".into(),
                model: "m".into(),
                step: "analyze".into(),
                item: Some(id.clone()),
                status: AgentStatus::Running,
                attempt: 1,
                tokens: Tokens::default(),
                started_ms: 2_000,
                ended_ms: None,
                note: None,
            })),
            Mutation::AssignAgent { item: id.clone(), agent: "a_1".into() },
            Mutation::UpdateItem { id: id.clone(), update: ItemUpdate { data: Some(done_data()), status: Some(ItemStatus::Done), ..ItemUpdate::default() } },
            Mutation::UpdatePhase { id: "ph_plan".into(), update: PhaseUpdate { status: Some(StageStatus::Done), ended_ms: Some(3_000), progress: Some(Progress { done: 1, total: 1 }), ..PhaseUpdate::default() } },
            Mutation::PushGate(Box::new(Gate {
                id: "g_1".into(),
                kind: GateKind::Plan,
                status: GateStatus::Open,
                payload: json!({"questions": 3}),
                options: GateKind::Plan.options(0),
                answer: None,
                asked_ms: 1_500,
                answered_ms: None,
            phase: String::new(),
            step: String::new(),
            })),
            Mutation::AnswerGate { id: "g_1".into(), choice: "approve".into(), text: None },
            Mutation::SetGrants(Grants { net: Some(NetGrant { scope: "run".into(), public_https_get: true, robots: true, quota: 60, used: 0 }) }),
            Mutation::SetMetrics(Metrics { cpu_pct: 12.5, ..Metrics::default() }),
            Mutation::AddFetch { ok: 2, failed: 1, blocked: 1, used: 3 },
            Mutation::SetRunMetrics { tokens: Tokens { input: 5, out: 6 }, active_ms: 7, budget_raises: 1 },
            Mutation::SetMachine { cpu_pct: 41.0, gpu_pct: 77.0, vram_mb: 9_000 },
            Mutation::SetTainted(true),
            Mutation::SetStatus(ProjectStatus::Running),
            Mutation::SetStatus(ProjectStatus::Finished),
        ];
        let n = muts.len() as u64;
        let mut expect_rev = st.rev;
        for (i, m) in muts.into_iter().enumerate() {
            let a = st.apply(m, 2_000 + i as u64).unwrap_or_else(|e| panic!("mutation {i}: {e}"));
            assert_eq!(a.base, expect_rev);
            expect_rev += 1;
            assert_eq!(a.rev, expect_rev);
            all.push(a);
        }
        assert_eq!(st.rev, 1 + n);
        for a in &all {
            apply_patches(&mut doc, &a.patches).unwrap();
        }
        assert_eq!(doc, st.to_value(), "replaying the patches in order gives the final state");
        assert_eq!(st.finished_ms, Some(2_000 + n - 1));
        assert!(st.tainted);
    }

    #[test]
    fn a_small_change_makes_a_small_patch() {
        let mut st = sample();
        let it = source_item(&st, 1);
        let id = it.id.clone();
        st.apply(Mutation::PutItem(Box::new(it)), 10).unwrap();
        let a = st
            .apply(Mutation::UpdateItem { id: id.clone(), update: ItemUpdate { live: Some(LiveUpdate { cpu_pct: Some(50.0), ..LiveUpdate::default() }), ..ItemUpdate::default() } }, 11)
            .unwrap();
        let paths: Vec<&str> = a.patches.iter().map(|p| p.path.as_str()).collect();
        assert_eq!(paths, [format!("/items/{id}/live/cpu_pct").as_str(), "/rev", "/updated_ms"]);
    }

    #[test]
    fn put_item_orders_by_kind_once_and_checks_the_parent() {
        let mut st = sample();
        let it = source_item(&st, 1);
        st.apply(Mutation::PutItem(Box::new(it.clone())), 1).unwrap();
        st.apply(Mutation::PutItem(Box::new(it.clone())), 2).unwrap();
        assert_eq!(st.order["source"], [it.id.clone()]);
        let mut orphan = source_item(&st, 2);
        orphan.parent = Some("i_nope".into());
        let rev = st.rev;
        assert!(matches!(st.apply(Mutation::PutItem(Box::new(orphan)), 3), Err(MutationError::UnknownParent { .. })));
        assert_eq!(st.rev, rev, "a refused mutation changes nothing");
        assert_eq!(st.children(&it.parent.clone().unwrap()).len(), 1);
    }

    #[test]
    fn a_done_item_must_match_its_form_exactly() {
        let mut st = sample();
        let it = source_item(&st, 1);
        let id = it.id.clone();
        st.apply(Mutation::PutItem(Box::new(it)), 1).unwrap();
        let cases: Vec<(&str, Box<dyn Fn(&mut Map<String, Value>)>)> = vec![
            ("an enum outside its values", Box::new(|d| { d.insert("relevancy".into(), json!("Maybe")); })),
            ("a string over its cap", Box::new(|d| { d.insert("one_line".into(), json!("x".repeat(141))); })),
            ("a missing required field", Box::new(|d| { d.remove("relevancy"); })),
            ("an unknown key", Box::new(|d| { d.insert("surprise".into(), json!(1)); })),
            ("too many claims", Box::new(|d| { d.insert("claims".into(), json!(vec![json!({"text": "t", "quote": "q"}); 9])); })),
            ("a record column the plan does not have", Box::new(|d| { d.insert("records".into(), json!([{"colour": "red"}])); })),
        ];
        for (why, tweak) in cases {
            let mut d = done_data();
            tweak(&mut d);
            let rev = st.rev;
            let r = st.apply(Mutation::UpdateItem { id: id.clone(), update: ItemUpdate { data: Some(d), status: Some(ItemStatus::Done), ..ItemUpdate::default() } }, 5);
            assert!(matches!(r, Err(MutationError::Invalid { .. })), "{why}: {r:?}");
            assert_eq!(st.rev, rev, "{why}");
            assert_eq!(st.item(&id).unwrap().status, ItemStatus::Queued, "{why}: the item is untouched");
        }
        st.apply(Mutation::UpdateItem { id: id.clone(), update: ItemUpdate { data: Some(done_data()), status: Some(ItemStatus::Done), ..ItemUpdate::default() } }, 6).unwrap();
        assert_eq!(st.item(&id).unwrap().status, ItemStatus::Done);
    }

    #[test]
    fn a_running_item_may_be_partial_and_an_unregistered_kind_is_not_checked() {
        let mut st = sample();
        let mut it = Item::new("i_ffffffff".into(), "gizmo", "gizmo@1");
        it.status = ItemStatus::Done;
        it.data.insert("anything".into(), json!([1, 2, 3]));
        st.apply(Mutation::PutItem(Box::new(it)), 1).unwrap();
        let mut partial = source_item(&st, 3);
        partial.status = ItemStatus::Running;
        partial.data.insert("index".into(), json!(3));
        st.apply(Mutation::PutItem(Box::new(partial)), 2).unwrap();
    }

    #[test]
    fn gates_open_one_at_a_time_and_answers_are_checked() {
        let mut st = sample();
        let gate = |id: &str, kind: GateKind| {
            Box::new(Gate { id: id.into(), kind, status: GateStatus::Open, payload: json!({}), options: kind.options(2), answer: None, asked_ms: 1, answered_ms: None, phase: String::new(), step: String::new(), })
        };
        st.apply(Mutation::PushGate(gate("g_1", GateKind::Checkin)), 1).unwrap();
        assert!(matches!(st.apply(Mutation::PushGate(gate("g_2", GateKind::Plan)), 2), Err(MutationError::GateAlreadyOpen(_))));
        let bad = |st: &mut ProjectState, choice: &str, text: Option<&str>| st.apply(Mutation::AnswerGate { id: "g_1".into(), choice: choice.into(), text: text.map(String::from) }, 3);
        assert!(matches!(bad(&mut st, "explode", None), Err(MutationError::BadChoice { .. })));
        assert!(matches!(bad(&mut st, "steer", None), Err(MutationError::NeedsText(_))));
        assert!(matches!(bad(&mut st, "steer", Some("   ")), Err(MutationError::NeedsText(_))));
        assert!(matches!(st.apply(Mutation::AnswerGate { id: "g_9".into(), choice: "approve".into(), text: None }, 3), Err(MutationError::UnknownGate(_))));
        bad(&mut st, "steer", Some("  focus on dates ")).unwrap();
        let g = &st.gates[0];
        assert_eq!(g.status, GateStatus::Answered);
        assert_eq!(g.answer.as_ref().unwrap().text.as_deref(), Some("focus on dates"));
        assert_eq!(g.answered_ms, Some(3));
        assert!(matches!(bad(&mut st, "approve", None), Err(MutationError::GateNotOpen(_))));
        st.apply(Mutation::PushGate(gate("g_2", GateKind::Plan)), 4).unwrap();
    }

    #[test]
    fn taint_is_a_latch() {
        let mut st = sample();
        st.apply(Mutation::SetTainted(true), 1).unwrap();
        st.apply(Mutation::SetTainted(false), 2).unwrap();
        assert!(st.tainted);
    }

    #[test]
    fn unknown_references_are_refused() {
        let mut st = sample();
        assert!(matches!(st.apply(Mutation::UpdatePhase { id: "ph_zzz".into(), update: PhaseUpdate::default() }, 1), Err(MutationError::UnknownPhase(_))));
        assert!(matches!(st.apply(Mutation::UpdateItem { id: "i_zzz".into(), update: ItemUpdate::default() }, 1), Err(MutationError::UnknownItem(_))));
        assert!(matches!(st.apply(Mutation::AssignAgent { item: "i_zzz".into(), agent: "a_1".into() }, 1), Err(MutationError::UnknownItem(_))));
        assert!(st.apply(Mutation::PushPhase(Phase::pending("Plan", 1)), 1).is_err());
        st.apply(Mutation::PushPhase(Phase::pending("Checkin", 1)), 1).unwrap();
    }

    #[test]
    fn diff_removes_keys_and_replaces_arrays_whole() {
        let mut out = Vec::new();
        diff("/x", Some(&json!({"a": 1, "b": [1, 2], "c": {"d": 1}})), Some(&json!({"a": 1, "b": [1, 2, 3], "c": {}, "e": 5})), &mut out);
        let want = vec![
            ProjectPatch::set("/x/b", json!([1, 2, 3])),
            ProjectPatch::set("/x/e", json!(5)),
            ProjectPatch::remove("/x/c/d"),
        ];
        // Order: keys of the new value first, then removals.
        assert_eq!(out.len(), 3);
        for w in &want {
            assert!(out.contains(w), "{w:?} in {out:?}");
        }
    }

    #[test]
    fn pointer_segments_are_escaped() {
        assert_eq!(escape_segment("a/b~c"), "a~1b~0c");
    }

    #[test]
    fn the_machine_load_changes_three_numbers_and_nothing_else_of_the_metrics() {
        let mut st = sample();
        st.apply(Mutation::AddFetch { ok: 3, failed: 0, blocked: 0, used: 3 }, 10).unwrap();
        st.apply(Mutation::SetRunMetrics { tokens: Tokens { input: 9, out: 4 }, active_ms: 77, budget_raises: 2 }, 11).unwrap();
        let a = st.apply(Mutation::SetMachine { cpu_pct: 41.0, gpu_pct: 77.0, vram_mb: 9_000 }, 12).unwrap();
        let paths: Vec<&str> = a.patches.iter().map(|p| p.path.as_str()).collect();
        assert!(paths.contains(&"/metrics/cpu_pct") && paths.contains(&"/metrics/gpu_pct") && paths.contains(&"/metrics/vram_mb"), "{paths:?}");
        assert!(!paths.iter().any(|p| p.starts_with("/metrics/fetch") || p.starts_with("/metrics/tokens") || p.contains("active_ms")), "{paths:?}");
        let m = st.metrics;
        assert_eq!((m.cpu_pct, m.gpu_pct, m.vram_mb, m.fetch.ok, m.tokens.input, m.active_ms, m.budget_raises), (41.0, 77.0, 9_000, 3, 9, 77, 2));
    }
}
