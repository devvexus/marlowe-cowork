//! A task on its worker thread: read what it needs, run the function or the employee, and hand
//! back a [`Record`]. Nothing here decides what runs next and nothing here writes the journal;
//! the coordinator does both, in order.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use marlowe_model::{CancelToken, Provider};
use serde_json::{Map, Value};

use super::functions::{default_prompt, Finished, TaskInput, Toolbox};
use super::gate::ModelGate;
use super::index::sub_of;
use super::record::{Outcome, Record};
use super::runner::{run_employee, AttemptReport, EmployeeCall, Failure, Observer, RuleFailure};
use super::EngineConfig;
use crate::actor::ProjectHandle;
use crate::form::{data_schema, model_schema, Form};
use crate::mutation::Mutation;
use crate::registry::Registry;
use crate::spec::{ModelClass, StepSpec};
use crate::state::{Agent, AgentStatus, Item, ItemStatus, Tokens};

/// What every worker shares.
pub(super) struct Shared {
    pub handle: ProjectHandle,
    pub provider: Arc<dyn Provider>,
    pub gate: Arc<dyn ModelGate>,
    pub tools: Arc<Toolbox>,
    pub registry: Arc<Registry>,
    pub cfg: EngineConfig,
    pub cancel: CancelToken,
    /// The next agent number.
    pub agents: AtomicU32,
}

impl Shared {
    pub fn now(&self) -> u64 {
        (self.cfg.clock)()
    }

    /// The model a step of this class runs on.
    pub fn model_for(&self, class: ModelClass) -> &str {
        match class {
            ModelClass::Employee => &self.cfg.employee_model,
            ModelClass::Lead => &self.cfg.lead_model,
        }
    }
}

/// A merge pass.
#[derive(Debug, Clone)]
pub(super) struct MergeTask {
    pub members: Vec<String>,
    pub threshold: usize,
    pub pass: u32,
}

/// A task ready to run.
#[derive(Debug, Clone)]
pub(super) struct Task {
    pub id: u64,
    pub phase_ix: usize,
    pub step_ix: usize,
    pub phase: String,
    pub round: u32,
    pub step: Arc<StepSpec>,
    pub item: Option<String>,
    pub sub: String,
    pub key: String,
    pub merge: Option<MergeTask>,
    pub seq: u64,
}

struct Ctx {
    input: TaskInput,
    form: Option<Form>,
    out_form: Option<Form>,
}

fn read_ctx(sh: &Shared, t: &Task) -> Result<Ctx, String> {
    let (item_id, members, step, phase, round, sub, out_form_id) =
        (t.item.clone(), t.merge.as_ref().map(|m| m.members.clone()).unwrap_or_default(), t.step.clone(), t.phase.clone(), t.round, t.sub.clone(), {
            sh.registry.phase_of_step(&t.step.id).and_then(|p| sh.registry.phase_spec(p)).and_then(|p| p.output.clone())
        });
    let pass = t.merge.as_ref().map_or(0, |m| m.pass);
    let today = today_of(sh.now());
    let project_handle = sh.handle.clone();
    let step2 = step.clone();
    let sub2 = sub.clone();
    sh.handle
        .read(move |st| {
            let item = item_id.as_deref().and_then(|id| st.items.get(id)).cloned();
            let members: Vec<Item> = members.iter().filter_map(|id| st.items.get(id)).cloned().collect();
            let existing: Vec<Item> = match (step2.kind, step2.emit.as_deref()) {
                (crate::state::StepKind::Merge, Some(kind)) => st
                    .items_of_kind(kind)
                    .into_iter()
                    .filter(|i| i.step == step2.id && sub_of(st, i) == sub2)
                    .cloned()
                    .collect(),
                _ => Vec::new(),
            };
            let form = step2.form.as_deref().and_then(|f| st.forms.get(f)).cloned();
            let out_form = out_form_id.as_deref().and_then(|f| st.forms.get(f)).cloned();
            Ctx {
                input: TaskInput {
                    project: st.id.clone(),
                    phase,
                    round,
                    step: step2,
                    item,
                    sub: sub2,
                    plan: Arc::new(st.plan.clone()),
                    members,
                    existing,
                    pass,
                    today,
                    project_handle,
                },
                form,
                out_form,
            }
        })
        .map_err(|e| e.to_string())
        .and_then(|c| {
            if t.item.is_some() && c.input.item.is_none() {
                Err(format!("input item `{}` is gone", t.item.clone().unwrap_or_default()))
            } else {
                Ok(c)
            }
        })
}

/// `2026-10-23` from epoch milliseconds (UTC).
pub(super) fn today_of(ms: u64) -> String {
    let days = (ms / 86_400_000) as i64;
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    format!("{:04}-{:02}-{:02}", if m <= 2 { y + 1 } else { y }, m, d)
}

/// Runs a task to a record. The flag is true when the run was cancelled meanwhile (the record is
/// then not journaled).
pub(super) fn run_task(sh: &Shared, t: &Task) -> (Record, bool) {
    let begun = sh.now();
    let mut rec = Record::new(t.key.clone(), &t.phase, &t.step.id, t.item.clone(), &t.sub);
    if let Some(m) = &t.merge {
        rec.members = m.members.clone();
        rec.threshold = Some(m.threshold);
    }
    let ctx = match read_ctx(sh, t) {
        Ok(c) => c,
        Err(e) => return (failed(rec, &e, begun, sh), false),
    };
    use crate::state::StepKind;
    let result = match t.step.kind {
        StepKind::Code => run_code(sh, t, &ctx),
        StepKind::Fanout | StepKind::Merge => run_model(sh, t, &ctx, &mut rec),
        StepKind::Gate => Err(Stop::Failed("a gate is not a task".into(), None)),
    };
    match result {
        Err(Stop::Cancelled) => (rec, true),
        Err(Stop::Failed(reason, degraded)) => {
            let ended = sh.now();
            match degraded {
                Some(fin) => {
                    build(sh, t, &ctx, &mut rec, fin, None, begun, ended);
                    rec.outcome = Outcome::Failed;
                    rec.error = Some(reason);
                    (rec, false)
                }
                None => (failed_with_item(sh, t, &ctx, rec, &reason, begun), false),
            }
        }
        Ok((fin, map)) => {
            let ended = sh.now();
            build(sh, t, &ctx, &mut rec, fin, map, begun, ended);
            // A phase output must match the form the phase declares.
            if let (Some(out), Some(form)) = (&rec.output, &ctx.out_form) {
                let errs = marlowe_contract::schema::validate(out, &data_schema(form, &ctx.input.plan));
                if !errs.is_empty() {
                    rec.output = None;
                    rec.outcome = Outcome::Failed;
                    rec.error = Some(format!("the phase output does not match `{}`: {}", form.id(), errs.iter().take(2).cloned().collect::<Vec<_>>().join("; ")));
                }
            }
            (rec, false)
        }
    }
}

enum Stop {
    Cancelled,
    /// A reason, and a degraded result a merge's finish function made without the model.
    Failed(String, Option<Finished>),
}

fn failed(mut rec: Record, reason: &str, begun: u64, sh: &Shared) -> Record {
    rec.outcome = Outcome::Failed;
    rec.error = Some(reason.to_string());
    rec.ms = sh.now().saturating_sub(begun);
    rec
}

/// A failed task leaves its item `failed` with the reason: the emitted item of a fan-out with
/// `emit`, else the input item.
fn failed_with_item(sh: &Shared, t: &Task, ctx: &Ctx, mut rec: Record, reason: &str, begun: u64) -> Record {
    rec.outcome = Outcome::Failed;
    rec.error = Some(reason.to_string());
    let ended = sh.now();
    rec.ms = ended.saturating_sub(begun);
    if let Some(agent) = rec.agent.as_mut() {
        agent.status = AgentStatus::Failed;
        agent.ended_ms = Some(ended);
    }
    let target = if t.step.kind == crate::state::StepKind::Fanout { target_item(t, ctx, begun) } else { None };
    if let Some(mut it) = target {
        it.status = ItemStatus::Failed;
        it.error = Some(reason.to_string());
        it.attempts = rec.attempts;
        it.key = Some(rec.key.clone());
        it.live.ended_ms = Some(ended);
        it.live.runtime_ms = Some(ended.saturating_sub(begun));
        if let Some(a) = &rec.agent {
            if !it.agents.contains(&a.id) {
                it.agents.push(a.id.clone());
            }
        }
        rec.items.push(it);
    }
    rec
}

/// The id of the item a fan-out with `emit` makes for this task.
fn emit_id(t: &Task, ctx: &Ctx, kind: &str) -> String {
    let who = t.item.clone().unwrap_or_else(|| t.phase.clone());
    crate::ids::item_id(&ctx.input.project, kind, &format!("{}:{who}", t.step.id))
}

/// The item the task's result lands on: a new item of the step's `emit` kind (fan-out), or the
/// input item (a fan-out or code step that fills it). `None` for a merge or a project-level step
/// with nothing to fill.
fn target_item(t: &Task, ctx: &Ctx, begun: u64) -> Option<Item> {
    use crate::state::StepKind;
    match (t.step.kind, t.step.emit.as_deref()) {
        (StepKind::Fanout, Some(kind)) => {
            let form = t.step.form.as_deref().unwrap_or("");
            let mut it = Item::new(emit_id(t, ctx, kind), kind, form);
            it.phase = t.phase.clone();
            it.step = t.step.id.clone();
            it.parent = ctx.input.parent_for_new();
            it.label = ctx.input.item.as_ref().map(|i| i.label.clone()).unwrap_or_default();
            it.live.started_ms = Some(begun);
            Some(it)
        }
        (StepKind::Fanout | StepKind::Code, None) => ctx.input.item.clone(),
        _ => None,
    }
}

fn run_code(sh: &Shared, t: &Task, ctx: &Ctx) -> Result<(Finished, Option<Map<String, Value>>), Stop> {
    let name = t.step.func.as_deref().unwrap_or("");
    let Some(f) = sh.tools.get_code(name) else { return Err(Stop::Failed(format!("no code function `{name}` is registered"), None)) };
    if sh.cancel.is_cancelled() {
        return Err(Stop::Cancelled);
    }
    match f(&ctx.input, &sh.cancel) {
        Ok(fin) => Ok((fin, None)),
        Err(_) if sh.cancel.is_cancelled() => Err(Stop::Cancelled),
        Err(reason) => Err(Stop::Failed(reason, None)),
    }
}

/// Puts the employee on the screen: an agent assigned to the item it works on, the emitted item
/// shown `running`, attempts and tokens counted as they come.
struct AgentObs<'a> {
    sh: &'a Shared,
    agent: Agent,
    assign: Vec<String>,
    placeholder: Option<Item>,
    /// The agent is on the screen (it waited for the GPU, or its first call started).
    shown: bool,
}

impl AgentObs<'_> {
    fn put(&self) {
        let _ = self.sh.handle.send(Mutation::PutAgent(Box::new(self.agent.clone())));
    }

    /// Puts the agent on the screen with its current status; the first time also starts its
    /// clock and shows the item it works on.
    fn show(&mut self) {
        if std::mem::replace(&mut self.shown, true) {
            return self.put();
        }
        self.agent.started_ms = self.sh.now();
        if let Some(p) = self.placeholder.as_mut() {
            p.status = ItemStatus::Running;
            p.agents = vec![self.agent.id.clone()];
            p.live.started_ms = Some(self.agent.started_ms);
            let _ = self.sh.handle.send(Mutation::PutItem(Box::new(p.clone())));
        }
        self.put();
        for id in &self.assign {
            let _ = self.sh.handle.send(Mutation::AssignAgent { item: id.clone(), agent: self.agent.id.clone() });
        }
    }
}

impl Observer for AgentObs<'_> {
    fn waiting(&mut self, why: &str) {
        if self.agent.note.as_deref() == Some(why) {
            return;
        }
        self.agent.status = AgentStatus::Waiting;
        self.agent.note = Some(why.to_string());
        self.show();
    }

    fn started(&mut self, attempt: u32) {
        self.agent.attempt = attempt;
        self.agent.status = AgentStatus::Running;
        self.agent.note = None;
        self.show();
    }

    fn finished(&mut self, report: &AttemptReport) {
        self.agent.tokens.input += report.usage.input;
        self.agent.tokens.out += report.usage.output;
        self.put();
    }
}

fn run_model(sh: &Shared, t: &Task, ctx: &Ctx, rec: &mut Record) -> Result<(Finished, Option<Map<String, Value>>), Stop> {
    let step = &t.step;
    let Some(form) = ctx.form.as_ref() else {
        return Err(Stop::Failed(format!("form `{}` is not embedded in the project", step.form.clone().unwrap_or_default()), None));
    };
    let schema = model_schema(form, &ctx.input.plan);
    let prompt = match sh.tools.get_prompt(step.prompt_name()) {
        Some(f) => f(&ctx.input).map_err(|e| Stop::Failed(format!("the prompt could not be built: {e}"), None))?,
        None => default_prompt(&ctx.input, &form.title),
    };
    if let Some(answer) = prompt.canned.clone() {
        // Code already knows the answer: the finish function sees it as it sees a reply.
        let map = answer.as_object().cloned().unwrap_or_default();
        let fin = match step.func.as_deref().and_then(|n| sh.tools.get_finish(n)) {
            Some(f) => f(&ctx.input, Some(map.clone())).map_err(|e| Stop::Failed(format!("form rule `{}` rejected the known answer", e.rule), None))?,
            None => Finished { data: map.clone(), ..Default::default() },
        };
        return Ok((fin, Some(map)));
    }
    let model = sh.model_for(step.class).to_string();
    let call = EmployeeCall {
        model: model.clone(),
        system: prompt.system,
        prompt: prompt.user,
        schema: super::functions::narrow_schema(schema, &prompt.narrow),
        think: step.effort,
        think_budget: step.think_budget,
        max_output: step.max_output.unwrap_or(1200),
        window_room: sh.cfg.window_room,
        attempts: step.attempts(),
    };
    let finish_fn = step.func.as_deref().and_then(|n| sh.tools.get_finish(n)).cloned();
    if step.func.is_some() && finish_fn.is_none() {
        return Err(Stop::Failed(format!("no finish function `{}` is registered", step.func.clone().unwrap_or_default()), None));
    }

    let agent_no = sh.agents.fetch_add(1, Ordering::SeqCst);
    let agent = Agent {
        id: crate::ids::agent_id(agent_no),
        role: step.role.clone().unwrap_or_default(),
        model,
        step: step.id.clone(),
        item: t.item.clone(),
        status: AgentStatus::Running,
        attempt: 1,
        tokens: Tokens::default(),
        started_ms: 0,
        ended_ms: None,
        note: None,
    };
    let placeholder = target_item(t, ctx, 0).filter(|it| step.emit.is_some() && it.id != ctx.input.item.as_ref().map_or("", |i| i.id.as_str()));
    let mut assign: Vec<String> = Vec::new();
    if let Some(i) = &t.item {
        assign.push(i.clone());
    }
    if let Some(p) = &placeholder {
        assign.push(p.id.clone());
    }
    let mut obs = AgentObs { sh, agent, assign, placeholder, shown: false };

    let input = &ctx.input;
    let mut finish = |v: Value| -> Result<(Finished, Map<String, Value>), RuleFailure> {
        let map = v.as_object().cloned().unwrap_or_default();
        let fin = match &finish_fn {
            Some(f) => f(input, Some(map.clone()))?,
            None => Finished::default(),
        };
        Ok((fin, map))
    };
    let result = run_employee(sh.provider.as_ref(), sh.gate.as_ref(), &sh.cancel, &sh.cfg.runner, &call, &mut finish, &mut obs);
    let mut agent = obs.agent;
    agent.note = None;
    let usage = match &result {
        Ok(s) => s.usage,
        Err(f) => f.usage,
    };
    agent.tokens = Tokens { input: usage.input, out: usage.output };
    agent.ended_ms = Some(sh.now());
    match result {
        Ok(s) => {
            agent.status = AgentStatus::Done;
            agent.attempt = s.attempts;
            rec.attempts = s.attempts;
            rec.classes = s.classes;
            rec.tokens_in = usage.input;
            rec.tokens_out = usage.output;
            rec.agent = Some(agent);
            let (mut fin, map) = s.value;
            if finish_fn.is_none() {
                fin.data = map.clone();
            }
            Ok((fin, Some(map)))
        }
        Err(f) if f.cancelled => Err(Stop::Cancelled),
        Err(f) => {
            agent.status = AgentStatus::Failed;
            agent.attempt = f.attempts;
            rec.attempts = f.attempts;
            rec.classes = f.classes.clone();
            rec.tokens_in = usage.input;
            rec.tokens_out = usage.output;
            rec.agent = Some(agent);
            let degraded = if step.kind == crate::state::StepKind::Merge { finish_fn.as_ref().and_then(|f| f(input, None).ok()) } else { None };
            Err(Stop::Failed(failure_reason(&f), degraded))
        }
    }
}

fn failure_reason(f: &Failure) -> String {
    f.reason.clone()
}

/// Turns a finished function result into the record's items: fills the emitted or input item,
/// defaults the extra items, carries the effects and the phase output.
#[allow(clippy::too_many_arguments)]
fn build(sh: &Shared, t: &Task, ctx: &Ctx, rec: &mut Record, fin: Finished, _map: Option<Map<String, Value>>, begun: u64, ended: u64) {
    rec.ms = ended.saturating_sub(begun);
    rec.effects = fin.effects;
    rec.output = fin.output;
    rec.rejected = fin.rejected;
    rec.tainted = fin.tainted;
    let agent_id = rec.agent.as_ref().map(|a| a.id.clone());
    if let Some(mut it) = target_item(t, ctx, begun) {
        let fills_input = t.step.emit.is_none() || t.step.kind == crate::state::StepKind::Code;
        // A code step with nothing to say leaves its input item alone.
        if !(fin.data.is_empty() && fin.label.is_none() && t.step.kind == crate::state::StepKind::Code) {
            for (k, v) in fin.data {
                it.data.insert(k, v);
            }
            if let Some(l) = fin.label {
                it.label = l;
            }
            if !fills_input {
                it.status = ItemStatus::Done;
            }
            it.attempts = rec.attempts;
            it.key = Some(rec.key.clone());
            it.live.started_ms.get_or_insert(begun);
            it.live.ended_ms = Some(ended);
            it.live.runtime_ms = Some(ended.saturating_sub(begun));
            if let Some(a) = &agent_id {
                if !it.agents.contains(a) {
                    it.agents.push(a.clone());
                }
            }
            rec.items.push(it);
        }
    }
    for mut e in fin.emits {
        if e.phase.is_empty() {
            e.phase = t.phase.clone();
        }
        if e.step.is_empty() {
            e.step = t.step.id.clone();
        }
        if e.parent.is_none() && e.kind != "question" {
            e.parent = ctx.input.parent_for_new();
        }
        e.live.started_ms.get_or_insert(begun);
        if e.status.is_final() {
            e.live.ended_ms.get_or_insert(ended);
        }
        if e.key.is_none() {
            e.key = Some(rec.key.clone());
        }
        if let Some(a) = &agent_id {
            if !e.agents.contains(a) {
                e.agents.push(a.clone());
            }
        }
        rec.items.push(e);
    }
    let _ = sh;
}
