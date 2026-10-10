//! Conductor controls (track F): Compact, AddDir, SwitchModel, ListCommands, ListAgentTypes,
//! AnswerQuestion, ReloadInstructions, DetachCall; and what every conductor session runs with.
//!
//! - [`middleware`]: instruction files and lazy rules, slash commands and `/loop`, the working
//!   set (todos, stop check, pinned set), worktree switches, loop guards.
//! - [`services`]: `ask_user` (a `QuestionAsked` / `AnswerQuestion` round trip), the subagent
//!   runner, worktrees, the todo broadcast, the control queue (`/compact`, `/add-dir`,
//!   `/memory`, applied at the session's next step boundary), the `/model` switch, model aliases
//!   for commands and agents, the session wake (a background agent's report starts a turn), and
//!   the cache-stable context policy.
//!
//! Nothing here blocks the client reader: controls are queued, questions wait on the turn's own
//! tool thread.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, Sender};
use marlowe_agent::agent::guard::LoopGuard;
use marlowe_agent::agent::{ContextPolicy, Control, Controls, ModelSwitch, Resolver};
use marlowe_agent::commands::{self, Catalog, CommandsMiddleware};
use marlowe_agent::instructions::{InstructionsMiddleware, Sources};
use marlowe_agent::subagent::{
    discover_types, model_target, read_aliases, HostFactory, ModelResolver, ModelTarget, ResolvedModel, SessionWake,
    SubagentConfig, Subagents, CLOUD_SLOTS,
};
use marlowe_agent::todo::{QuestionAsker, Questions, TodoSink, WorkingSet};
use marlowe_agent::worktree::{NoWorktrees, WorktreeMiddleware, WorktreeProvider, Worktrees};
use marlowe_agent::context::PrefixFit;
use marlowe_agent::{ApprovalGate, ApprovalReq, Conversation, FinalFlow, Services, StepInfo, ToolHost, ToolMiddleware};
use marlowe_model::{CancelToken, Effort, ModelRef};
use marlowe_proto::{ApprovalDecision, Event, Question, Request, SessionId, Slot, TodoItem};

use crate::daemon::Core;
use crate::session::RELAY_PREFIX;
use crate::{lock, HostRequest};

/// The worktree provider the conductor uses. Track I installs the crew sandbox's provider with
/// [`install_worktrees`]; until then worktrees are unavailable and say so.
#[derive(Default)]
pub(crate) struct WorktreeSlot(Mutex<Option<Arc<dyn WorktreeProvider>>>);

/// Installs the provider for `agent{isolation:"worktree"}` and `enter_worktree`.
#[allow(dead_code)]
pub(crate) fn install_worktrees(core: &Core, provider: Arc<dyn WorktreeProvider>) {
    *lock(&core.ext.get_or_init(WorktreeSlot::default).0) = Some(provider);
}

fn worktrees(core: &Core) -> Arc<dyn WorktreeProvider> {
    lock(&core.ext.get_or_init(WorktreeSlot::default).0).clone().unwrap_or_else(|| Arc::new(NoWorktrees))
}

/// Per-session state the requests reach.
#[derive(Default)]
struct ConductorState {
    controls: Mutex<HashMap<SessionId, Controls>>,
    switches: Mutex<HashMap<SessionId, ResolvedModel>>,
    questions: Mutex<HashMap<String, Sender<Vec<String>>>>,
    next_question: AtomicU64,
}

fn state(core: &Core) -> Arc<ConductorState> {
    core.ext.get_or_init(ConductorState::default)
}

/// The session's control queue, shared by the requests and the session's loop.
fn controls_for(core: &Core, session: &SessionId) -> Controls {
    lock(&state(core).controls).entry(session.clone()).or_default().clone()
}

/// Where a session's instructions, commands, skills and agents come from.
fn sources(core: &Core, workspace: Option<&Path>) -> Sources {
    Sources::for_session(Some(&core.home), workspace)
}

/// `[context] cache_stable = true` in `<home>\settings.toml`: per-turn memory and the
/// environment ride on the turn's message, so every request extends the last byte for byte
/// (design 3.4, A14). Off by default until the daemon's request-shape tests move with it.
fn cache_stable(core: &Core) -> bool {
    let Ok(text) = std::fs::read_to_string(core.home.join("settings.toml")) else { return false };
    let mut section = String::new();
    text.lines().map(str::trim).any(|l| {
        if l.starts_with('[') {
            section = l.trim_matches(['[', ']']).trim().to_string();
            return false;
        }
        let Some((k, v)) = l.split_once('=') else { return false };
        let key = k.trim();
        let full = if section.is_empty() { key.to_string() } else { format!("{section}.{key}") };
        full == "context.cache_stable" && v.trim() == "true"
    })
}

/// Refuses a request for a session the store does not know.
fn known(core: &Core, session: &SessionId) -> Result<(), String> {
    match core.store.get(session) {
        Ok(Some(_)) => Ok(()),
        Ok(None) => Err(format!("unknown session {session}")),
        Err(e) => Err(e),
    }
}

fn workspace_path(w: &Option<String>) -> Option<PathBuf> {
    w.as_ref().filter(|s| !s.trim().is_empty()).map(PathBuf::from)
}

pub(crate) fn dispatch(core: &Arc<Core>, client: u64, req: Request) {
    let reply = |event: Event| core.bus.reply(client, event);
    let refuse = |session: Option<SessionId>, message: String| reply(Event::Error { session, message });
    let session_of = |r: &Request| match r {
        Request::Compact { session, .. }
        | Request::AddDir { session, .. }
        | Request::ReloadInstructions { session }
        | Request::SwitchModel { session, .. } => Some(session.clone()),
        _ => None,
    };
    if let Some(s) = session_of(&req) {
        if let Err(e) = known(core, &s) {
            return refuse(Some(s), e);
        }
    }
    match req {
        Request::Compact { session, focus } => {
            controls_for(core, &session).push(Control::Compact(focus.filter(|f| !f.trim().is_empty())));
        }
        Request::AddDir { session, path } => {
            let p = PathBuf::from(path.trim());
            if !p.is_dir() {
                return refuse(Some(session), format!("{} is not a directory", p.display()));
            }
            controls_for(core, &session).push(Control::AddDir(p));
        }
        Request::ReloadInstructions { session } => controls_for(core, &session).push(Control::ReloadInstructions),
        Request::SwitchModel { session, model, effort } => match resolve_model(core, &model, effort) {
            Ok(m) => {
                lock(&state(core).switches).insert(session, m);
            }
            Err(e) => refuse(Some(session), e),
        },
        Request::ListCommands { workspace } => {
            let catalog = Catalog::load(&sources(core, workspace_path(&workspace).as_deref()));
            reply(Event::Commands { list: catalog.command_infos() });
        }
        Request::ListAgentTypes { workspace } => {
            let (types, _) = discover_types(&sources(core, workspace_path(&workspace).as_deref()));
            reply(Event::AgentTypes { list: types.iter().map(|t| t.info()).collect() });
        }
        Request::AnswerQuestion { session, id, answers } => match lock(&state(core).questions).remove(&id) {
            Some(tx) => {
                let _ = tx.try_send(answers);
            }
            None => refuse(Some(session), format!("question {id} is not waiting for an answer")),
        },
        // Moving a running shell to the job table is track B's (`marlowe-exec` jobs).
        Request::DetachCall { session, call_id } => {
            refuse(Some(session), format!("detaching {call_id} needs the background job table (M4.6 track B)"))
        }
        other => crate::client::not_yet(core, client, &other, "F"),
    }
}

/// A library model ready to call, for `/model` and agents' model ids.
fn resolve_model(core: &Core, model: &ModelRef, effort: Option<Effort>) -> Result<ResolvedModel, String> {
    let info = core.brains.model_info(model).map_err(|e| e.to_string())?;
    let provider = core.brains.provider_for(model).map_err(|e| e.to_string())?;
    let effort = effort.unwrap_or_else(|| core.brains.resolve(&Slot::Conductor).map(|(_, e)| e).unwrap_or_default());
    Ok(ResolvedModel { provider, info, effort })
}

/// Resolves agents' and commands' `model:` (aliases through `<home>\settings.toml`
/// `[model_aliases]`, slots, library model ids; `inherit` is the session's model).
struct LibraryResolver {
    core: Arc<Core>,
    session: SessionId,
}

impl LibraryResolver {
    fn slot(&self, slot: Slot) -> Result<ResolvedModel, String> {
        let (provider, info, effort) = self.core.brains.agent_parts(&slot).map_err(|e| e.to_string())?;
        Ok(ResolvedModel { provider, info, effort })
    }
}

impl ModelResolver for LibraryResolver {
    fn resolve(&self, model: Option<&str>) -> Result<ResolvedModel, String> {
        let aliases =
            std::fs::read_to_string(self.core.home.join("settings.toml")).map(|t| read_aliases(&t)).unwrap_or_default();
        match model_target(model, &aliases) {
            ModelTarget::Inherit => match lock(&state(&self.core).switches).get(&self.session).cloned() {
                Some(m) => Ok(m),
                None => self.slot(Slot::Conductor),
            },
            ModelTarget::Slot(s) => {
                let slot = match s.as_str() {
                    "quick" => Slot::Quick,
                    "reflection" => Slot::Reflection,
                    "conductor" => Slot::Conductor,
                    other => Slot::Crew(other.trim_start_matches("crew:").to_string()),
                };
                // An unassigned slot falls back to the conductor's model.
                self.slot(slot).or_else(|_| self.slot(Slot::Conductor))
            }
            ModelTarget::Model(id) => {
                let models = self.core.brains.with_library(|l| l.models()).map_err(|e| e.to_string())?;
                let hit = models.into_iter().map(|m| m.model).find(|m| {
                    let r = m.to_string();
                    r.eq_ignore_ascii_case(&id) || m.model_id == id
                });
                match hit {
                    Some(m) => resolve_model(&self.core, &m, None),
                    None => Err(format!("model `{id}` is not in the library")),
                }
            }
        }
    }
}

/// `ask_user` as a `QuestionAsked` / `AnswerQuestion` round trip; refused when no client is
/// connected to answer (the model then decides and says so).
struct BusQuestions {
    core: Arc<Core>,
}

impl QuestionAsker for BusQuestions {
    fn ask(&self, session: &SessionId, questions: &[Question], cancel: &CancelToken) -> Result<Vec<String>, String> {
        if self.core.bus.client_count() == 0 {
            return Err("no client is connected to answer".into());
        }
        let st = state(&self.core);
        let id = format!("q{}", st.next_question.fetch_add(1, Ordering::SeqCst) + 1);
        let (tx, rx) = bounded(1);
        lock(&st.questions).insert(id.clone(), tx);
        self.core.bus.publish(Event::QuestionAsked { session: session.clone(), id: id.clone(), questions: questions.to_vec() });
        let deadline = Instant::now() + self.core.cfg.approval_timeout;
        let answer = loop {
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(a) => break Ok(a),
                Err(_) if cancel.is_cancelled() || self.core.is_shutting_down() => break Err("the turn was interrupted".into()),
                Err(_) if Instant::now() >= deadline => break Err("no answer in time".into()),
                Err(_) => {}
            }
        };
        lock(&st.questions).remove(&id);
        answer
    }
}

/// Approvals for subagents' calls: the same round trip as the conductor's.
struct SubagentGate {
    core: Arc<Core>,
}

impl ApprovalGate for SubagentGate {
    fn ask(&self, req: ApprovalReq) -> ApprovalDecision {
        if self.core.is_shutting_down() {
            return ApprovalDecision::Deny;
        }
        let (id, _tx, rx) = self.core.approvals.register();
        self.core.bus.publish(Event::ApprovalRequested {
            approval: id,
            session: req.session,
            tool: req.tool,
            summary: format!("(subagent) {}", req.summary),
            detail: req.detail,
            run: None,
            proposed_rule: None,
        });
        let decision = rx.recv_timeout(self.core.cfg.approval_timeout).unwrap_or(ApprovalDecision::Deny);
        self.core.approvals.remove(id);
        self.core.bus.publish(Event::ApprovalResolved { approval: id, decision });
        decision
    }
}

/// Starts a turn in the session (or steers it) with a background agent's report or a job
/// notice. Sent as a relay, so memory never ingests it at user trust.
fn wake(core: &Arc<Core>) -> SessionWake {
    let core = core.clone();
    SessionWake(Arc::new(move |session: &SessionId, text: String, tainted: bool| {
        let text = if text.starts_with(RELAY_PREFIX) { text } else { format!("{RELAY_PREFIX}notice] {text}") };
        match core.session(session) {
            Ok(h) => {
                if let Err(e) = h.relay(text, tainted) {
                    core.bus.publish(Event::Error { session: Some(session.clone()), message: format!("notice not delivered: {e}") });
                }
            }
            Err((s, m)) => {
                core.bus.publish(Event::Error { session: s, message: m });
            }
        }
    }))
}

/// Tells the user when the prefix budget cut something (design 3.4: never silent), once per
/// change: the instruction files, repo map or skills index that did not fit.
struct PrefixNotice {
    core: Arc<Core>,
    last: Mutex<Vec<String>>,
}

impl PrefixNotice {
    fn check(&self, convo: &Conversation) {
        let warnings = convo.ext.get::<PrefixFit>().map(|f| f.warnings.clone()).unwrap_or_default();
        let mut last = lock(&self.last);
        if warnings.is_empty() || *last == warnings {
            return;
        }
        *last = warnings.clone();
        let message = format!("context: {} (see /context)", warnings.join("; "));
        self.core.bus.publish(Event::Error { session: Some(convo.session.clone()), message });
    }
}

impl ToolMiddleware for PrefixNotice {
    fn name(&self) -> &str {
        "prefix-notice"
    }

    fn after_step(&self, _info: StepInfo, convo: &mut Conversation) -> Vec<marlowe_model::Message> {
        self.check(convo);
        Vec::new()
    }

    fn before_final(&self, _info: StepInfo, convo: &mut Conversation, _text: &str) -> FinalFlow {
        self.check(convo);
        FinalFlow::Accept
    }
}

/// F's loop middleware for one session, added by `stack.rs`.
pub(crate) fn middleware(core: &Arc<Core>, convo: &Conversation) -> Vec<Arc<dyn ToolMiddleware>> {
    let src = sources(core, convo.workspace.as_deref());
    let run: Arc<dyn Fn(&str, Option<&Path>) -> Result<String, String> + Send + Sync> = Arc::new(commands::run_bash);
    let c = core.clone();
    let schedule: commands::ScheduleFn = Arc::new(move |session: &SessionId, secs: u64, text: &str| {
        crate::schedule_ops::add(&c, Some(session.clone()), secs, text).map(|s| s.id)
    });
    vec![
        InstructionsMiddleware::new(src.clone()),
        CommandsMiddleware::with_schedule(src, run, schedule),
        WorkingSet::new(),
        WorktreeMiddleware::new(),
        LoopGuard::new(),
        Arc::new(PrefixNotice { core: core.clone(), last: Mutex::new(Vec::new()) }),
    ]
}

/// F's services for one session, added by `stack.rs`.
pub(crate) fn services(core: &Arc<Core>, convo: &Conversation, services: Services) -> Services {
    let session = convo.session.clone();
    let resolver: Arc<dyn ModelResolver> = Arc::new(LibraryResolver { core: core.clone(), session: session.clone() });
    let tools_core = core.clone();
    let model = core.brains.agent_parts(&Slot::Conductor).ok().map(|(_, info, _)| info);
    let tools: HostFactory = Arc::new(move |workspace: Option<&Path>| -> Arc<dyn ToolHost> {
        (tools_core.tools)(&HostRequest { workspace: workspace.map(Path::to_path_buf), model: model.clone() })
    });
    let worktree_provider = worktrees(core);
    let sub = Subagents::new(SubagentConfig {
        resolver: resolver.clone(),
        tools,
        approvals: Arc::new(SubagentGate { core: core.clone() }),
        worktrees: worktree_provider.clone(),
        sources: sources(core, None),
        wake: Some(wake(core)),
        middleware: vec![WorkingSet::new(), LoopGuard::new()],
        cloud_slots: CLOUD_SLOTS,
    });
    let todo_core = core.clone();
    let todos = TodoSink(Arc::new(move |s: &SessionId, items: &[TodoItem]| {
        todo_core.bus.publish(Event::Todos { session: s.clone(), items: items.to_vec() });
    }));
    let switch_core = core.clone();
    let switch_session = session.clone();
    let switch = ModelSwitch(Arc::new(move || lock(&state(&switch_core).switches).get(&switch_session).cloned()));
    let services = services
        .with(Questions(Arc::new(BusQuestions { core: core.clone() })))
        .with(sub)
        .with(Worktrees(worktree_provider))
        .with(todos)
        .with(controls_for(core, &session))
        .with(switch)
        .with(Resolver(resolver))
        .with(wake(core));
    if cache_stable(core) {
        services.with(ContextPolicy::CACHE_STABLE)
    } else {
        services
    }
}
