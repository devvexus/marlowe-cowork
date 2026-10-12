//! The conductor's second round (track F3): `/fast`, the plan file's approval, rule suggestions,
//! the shared task list, `+500k` and `ultrathink`.
//!
//! - [`services`] (per session, from `stack.rs`): the plan-file asker (`exit_plan` publishes
//!   `PlanReady` and waits for `AnswerPlan`), the shared task list with its persistence, the output
//!   token pool, and the model switch `/fast` rides on (it wraps F's `/model` switch: fast on runs
//!   the `Quick` slot's model, off falls back to whatever the session had).
//! - [`middleware`] (per session): the directive parser (`ultrathink`, `+500k`) and the sync that
//!   keeps the conductor's todo mirror equal to the shared list.
//! - [`dispatch`]: `SetFast`, `AnswerPlan`, `SuggestRules`.
//!
//! Nothing here blocks the client reader: the plan and the rule scan wait or work on the turn's
//! tool thread and the request's own thread respectively.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, Sender};
use marlowe_agent::agent::ModelSwitch;
use marlowe_agent::directive::{DirectiveMiddleware, TokenPool};
use marlowe_agent::planfile::{PlanAsker, PlanDir, PlanReview};
use marlowe_agent::subagent::ResolvedModel;
use marlowe_agent::tasklist::{ListState, TaskListStore, TaskListSync, TaskLists};
use marlowe_agent::todo::TodoSink;
use marlowe_agent::{Conversation, Services, ToolMiddleware};
use marlowe_model::{CancelToken, Part, Role};
use marlowe_proto::{Event, FastState, PlanFileDecision, Request, RuleSuggestion, SessionId, Slot};

use crate::daemon::Core;
use crate::lock;

/// What the daemon keeps for this track.
#[derive(Default)]
struct F3State {
    pools: Mutex<HashMap<SessionId, TokenPool>>,
    /// Sessions with `/fast` on, and the model each runs.
    fast: Mutex<HashMap<SessionId, ResolvedModel>>,
    plans: Mutex<HashMap<String, Sender<PlanFileDecision>>>,
    next_plan: AtomicU64,
    lists: Mutex<Option<TaskLists>>,
}

fn state(core: &Core) -> Arc<F3State> {
    core.ext.get_or_init(F3State::default)
}

// ---------------------------------------------------------------- the shared task list

/// The task lists in the daemon's key-value table: one JSON value per conversation.
struct KvLists {
    core: Weak<Core>,
}

fn list_key(root: &SessionId) -> String {
    format!("tasklist.{root}")
}

impl TaskListStore for KvLists {
    fn load(&self, root: &SessionId) -> Option<ListState> {
        let core = self.core.upgrade()?;
        let raw = core.store.kv(&list_key(root)).ok().flatten()?;
        serde_json::from_str(&raw).ok()
    }

    fn save(&self, root: &SessionId, state: &ListState) {
        let Some(core) = self.core.upgrade() else { return };
        match serde_json::to_string(state) {
            Ok(json) => {
                if let Err(e) = core.store.set_kv(&list_key(root), &json) {
                    eprintln!("marlowe: the task list of {root} was not saved: {e}");
                }
            }
            Err(e) => eprintln!("marlowe: the task list of {root} was not saved: {e}"),
        }
    }
}

/// The daemon's task lists (one value for every session; the list is keyed by conversation).
fn lists(core: &Arc<Core>) -> TaskLists {
    let st = state(core);
    let mut g = lock(&st.lists);
    g.get_or_insert_with(|| {
        let weak = Arc::downgrade(core);
        let sink_core = weak.clone();
        let sink = TodoSink(Arc::new(move |s: &SessionId, items: &[marlowe_proto::TodoItem]| {
            if let Some(core) = sink_core.upgrade() {
                core.bus.publish(Event::Todos { session: s.clone(), items: items.to_vec() });
            }
        }));
        TaskLists::new(Arc::new(KvLists { core: weak }), Some(sink))
    })
    .clone()
}

/// The session's `+500k` pool, linked on creation so the session's workflows spend from it too.
fn pool_for(core: &Core, session: &SessionId) -> TokenPool {
    lock(&state(core).pools)
        .entry(session.clone())
        .or_insert_with(|| {
            let pool = TokenPool::new();
            marlowe_workflow::pools::link(session, Arc::new(pool.clone()));
            pool
        })
        .clone()
}

// ---------------------------------------------------------------- the plan file

/// `exit_plan` as a `PlanReady` / `AnswerPlan` round trip; refused when no client is connected
/// to answer (the model then stops with its plan as text).
struct BusPlans {
    core: Arc<Core>,
}

impl PlanAsker for BusPlans {
    fn ask(&self, session: &SessionId, path: &Path, text: &str, cancel: &CancelToken) -> Result<PlanFileDecision, String> {
        if self.core.bus.client_count() == 0 {
            return Err("no client is connected to answer".into());
        }
        let st = state(&self.core);
        let id = format!("p{}", st.next_plan.fetch_add(1, Ordering::SeqCst) + 1);
        let (tx, rx) = bounded(1);
        lock(&st.plans).insert(id.clone(), tx);
        self.core.bus.publish(Event::PlanReady {
            session: session.clone(),
            id: id.clone(),
            path: path.display().to_string(),
            text: text.to_string(),
        });
        let deadline = Instant::now() + self.core.cfg.approval_timeout;
        let answer = loop {
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(a) => break Ok(a),
                Err(_) if cancel.is_cancelled() || self.core.is_shutting_down() => break Err("the turn was interrupted".into()),
                Err(_) if Instant::now() >= deadline => break Err("no answer in time".into()),
                Err(_) => {}
            }
        };
        lock(&st.plans).remove(&id);
        answer
    }
}

// ---------------------------------------------------------------- per-session wiring

/// F3's loop middleware for one session, added by `stack.rs`.
pub(crate) fn middleware(core: &Arc<Core>, convo: &Conversation) -> Vec<Arc<dyn ToolMiddleware>> {
    vec![DirectiveMiddleware::new(pool_for(core, &convo.session)), TaskListSync::new(lists(core))]
}

/// F3's services for one session.
pub(crate) fn services(core: &Arc<Core>, convo: &Conversation, services: Services) -> Services {
    let session = convo.session.clone();
    let st = state(core);
    // `/fast` first, then whatever `/model` set.
    let prior = services.get::<ModelSwitch>().cloned();
    let (fast_state, fast_session) = (st, session.clone());
    let switch = ModelSwitch(Arc::new(move || {
        lock(&fast_state.fast).get(&fast_session).cloned().or_else(|| prior.as_ref().and_then(|p| (p.0)()))
    }));
    services
        .with(switch)
        .with(lists(core))
        .with(pool_for(core, &session))
        .with(PlanDir(core.home.join("plans")))
        .with(PlanReview(Arc::new(BusPlans { core: core.clone() })))
}

// ---------------------------------------------------------------- requests

fn known(core: &Core, session: &SessionId) -> Result<(), String> {
    match core.store.get(session) {
        Ok(Some(_)) => Ok(()),
        Ok(None) => Err(format!("unknown session {session}")),
        Err(e) => Err(e),
    }
}

pub(crate) fn dispatch(core: &Arc<Core>, client: u64, req: Request) {
    let refuse = |session: Option<SessionId>, message: String| core.bus.reply(client, Event::Error { session, message });
    match req {
        Request::SetFast { session, on } => {
            if let Err(e) = known(core, &session) {
                return refuse(Some(session), e);
            }
            match set_fast(core, &session, on) {
                Ok(state) => {
                    core.bus.publish(Event::FastChanged { session, state });
                }
                Err(e) => refuse(Some(session), e),
            }
        }
        Request::AnswerPlan { session, id, decision } => match lock(&state(core).plans).remove(&id) {
            Some(tx) => {
                let _ = tx.try_send(decision);
            }
            None => refuse(Some(session), format!("plan {id} is not waiting for an answer")),
        },
        Request::SuggestRules { workspace } => {
            let core = core.clone();
            // The scan reads every session of the project: off the client reader.
            let spawned = std::thread::Builder::new().name("marlowe-suggest-rules".into()).spawn(move || {
                let list = suggest_rules(&core, workspace.as_deref().map(PathBuf::from).as_deref());
                core.bus.reply(client, Event::RuleSuggestions { list });
            });
            if let Err(e) = spawned {
                refuse(None, format!("could not scan the transcripts: {e}"));
            }
        }
        other => crate::client::not_yet(core, client, &other, "F3"),
    }
}

/// `/fast`: the conductor runs on the `Quick` slot's model (an unassigned slot is the conductor's
/// own), history kept; off restores the session's model.
fn set_fast(core: &Arc<Core>, session: &SessionId, on: bool) -> Result<FastState, String> {
    let st = state(core);
    if !on {
        lock(&st.fast).remove(session);
        let model = core.brains.resolve(&Slot::Conductor).ok().map(|(m, _)| m);
        return Ok(FastState { on: false, model });
    }
    let (provider, info, effort) = core.brains.agent_parts(&Slot::Quick).map_err(|e| e.to_string())?;
    let model = info.model_ref();
    lock(&st.fast).insert(session.clone(), ResolvedModel { provider, info, effort });
    Ok(FastState { on: true, model: Some(model) })
}

// ---------------------------------------------------------------- /fewer-permission-prompts

/// The shell lines and MCP tools the project's past sessions ran, as the transcripts record them.
fn observed_calls(core: &Core, workspace: Option<&Path>) -> Vec<marlowe_permission::suggest::Observed> {
    use marlowe_permission::suggest::Observed;
    let same = |w: &Option<String>| match (w, workspace) {
        (Some(a), Some(b)) => same_path(Path::new(a), b),
        (None, None) => true,
        _ => false,
    };
    let sessions = core.store.list(200).unwrap_or_default();
    let mut out = Vec::new();
    for s in sessions.iter().filter(|s| same(&s.workspace)) {
        let Ok(turns) = core.store.load(&s.id) else { continue };
        // A call counts when its result says it ran: a denied or refused call does not.
        let mut results: HashMap<String, bool> = HashMap::new();
        for t in turns.iter().filter(|t| t.role == Role::Tool) {
            for p in &t.content {
                if let Part::ToolResult(r) = p {
                    let refused = r.is_error && (r.content.starts_with("The user denied") || r.content.starts_with("Not run:"));
                    results.insert(r.call_id.clone(), !refused);
                }
            }
        }
        for t in turns.iter().filter(|t| t.role == Role::Assistant) {
            for p in &t.content {
                let Part::ToolCall(c) = p else { continue };
                if results.get(&c.id) != Some(&true) {
                    continue;
                }
                let command = c.args.get("command").and_then(|v| v.as_str()).map(str::to_string);
                if matches!(c.name.as_str(), "bash" | "powershell") && command.is_some() || c.name.starts_with("mcp__") {
                    out.push(Observed { tool: c.name.clone(), command });
                }
            }
        }
    }
    out
}

fn same_path(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| p.to_string_lossy().replace('\\', "/").trim_end_matches('/').to_ascii_lowercase();
    norm(a) == norm(b)
}

fn suggest_rules(core: &Arc<Core>, workspace: Option<&Path>) -> Vec<RuleSuggestion> {
    let calls = observed_calls(core, workspace);
    let settings = marlowe_settings::Settings::load(&core.home, workspace, None);
    let roots: Vec<PathBuf> = workspace.map(|w| vec![w.to_path_buf()]).unwrap_or_default();
    marlowe_permission::suggest::suggest(&calls, &roots, &settings.rule_set())
        .into_iter()
        .map(|s| RuleSuggestion { pattern: s.pattern, count: s.count, example: s.example })
        .collect()
}
