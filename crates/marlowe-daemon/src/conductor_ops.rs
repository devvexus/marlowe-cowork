//! Conductor controls (track F): Compact, AddDir, SwitchModel, ListCommands, ListAgentTypes,
//! AnswerQuestion, ReloadInstructions, DetachCall; and what every conductor session runs with.
//!
//! - [`middleware`]: instruction files and lazy rules, slash commands and `/loop`, the working
//!   set (todos, stop check, pinned set), worktree switches, loop guards.
//! - [`services`]: `ask_user` (a `QuestionAsked` / `AnswerQuestion` round trip), the subagent
//!   runner, worktrees and the session's tools rebuilt for a worktree or an added root, the todo
//!   broadcast, the control queue (`/compact`, `/add-dir`, `/memory`, applied at the session's
//!   next step boundary), the `/model` switch, model aliases for commands and agents, the session
//!   wake (a background agent's report starts a turn), and the cache-stable context policy.
//! - Background agents are recorded in `cowork.db` while they run; one still recorded at the
//!   next start was lost, and its session is told so on its next turn ([`on_boot`]).
//! - Trust for repository content ([`TrustPrompt`]): a project command that runs commands or
//!   grants tools, and instruction imports from outside the workspace, ask once (an approval
//!   round trip); the yes is kept by digest in `cowork.db`.
//! - Subagents' approvals end at once when their run is interrupted or the daemon stops.
//!
//! Nothing here blocks the client reader: controls are queued, questions wait on the turn's own
//! tool thread.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, Sender};
use rusqlite::{params, Connection};
use marlowe_agent::agent::guard::LoopGuard;
use marlowe_agent::agent::{ContextPolicy, Control, Controls, ModelSwitch, Resolver};
use marlowe_agent::commands::{self, Catalog, CommandsMiddleware};
use marlowe_agent::instructions::{InstructionsMiddleware, Sources};
use marlowe_agent::instructions::trust::Trust;
use marlowe_agent::subagent::{
    discover_types, model_target, read_aliases, ApprovalFactory, BackgroundLedger, HostFactory, ModelResolver, ModelTarget,
    ResolvedModel, SessionWake, SubagentConfig, Subagents, CLOUD_SLOTS,
};
use marlowe_agent::todo::{QuestionAsker, Questions, TodoSink, WorkingSet};
use marlowe_agent::worktree::{NoWorktrees, WorkspaceHosts, WorktreeMiddleware, WorktreeProvider, Worktrees};
use marlowe_agent::context::PrefixFit;
use marlowe_agent::{
    ApprovalGate, ApprovalReq, Conversation, FinalFlow, Services, StepInfo, ToolHost, ToolMiddleware, TurnFlow,
};
use marlowe_model::{CancelToken, Effort, ModelRef};
use marlowe_proto::{paths, ApprovalDecision, Event, Question, Request, SessionId, Slot, TodoItem};

use crate::daemon::Core;
use crate::session::RELAY_PREFIX;
use crate::{lock, now_ms, HostRequest};

/// The worktree provider the conductor uses. Track I installs the crew sandbox's provider with
/// [`install_worktrees`]; until then worktrees are unavailable and say so.
#[derive(Default)]
pub(crate) struct WorktreeSlot(Mutex<Option<Arc<dyn WorktreeProvider>>>);

/// Installs the provider for `agent{isolation:"worktree"}` and `enter_worktree`.
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
    /// The prefix-budget cuts already reported (once per daemon, not once per session).
    prefix_reported: Mutex<Vec<String>>,
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
        // Moving a running shell to the job table: the jobs live in the tool host (track B).
        Request::DetachCall { session, call_id } => crate::job_ops::detach(core, client, session, call_id),
        other => crate::client::not_yet(core, client, &other, "F"),
    }
}

/// A library model ready to call, for `/model` and agents' model ids. Its window is the context
/// gate's, as `Brains::agent_parts` gives the conductor's: compaction and the prefix budget work
/// against what the runner really holds, not the model's advertised window.
fn resolve_model(core: &Core, model: &ModelRef, effort: Option<Effort>) -> Result<ResolvedModel, String> {
    let mut info = core.brains.model_info(model).map_err(|e| e.to_string())?;
    let saved = core.brains.with_library(|l| l.policy(model)).map_err(|e| e.to_string())?.is_some();
    if saved || info.context_window.is_some() {
        if let Ok(window) = core.brains.context().allowed_ctx(model, None) {
            info.context_window = Some(window);
        }
    }
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

/// Approvals for subagents' calls: the same round trip as the conductor's, published under the
/// parent session (a subagent's id is `<parent>/agent-N`), so clients that answer by session see
/// it; the agent is named in the summary. One per run: the run's cancel token (the caller's turn,
/// or a background agent's own) denies a pending approval at once, as does shutdown.
struct SubagentGate {
    core: Arc<Core>,
    cancel: CancelToken,
}

/// Builds a [`SubagentGate`] for each subagent run.
fn subagent_approvals(core: &Arc<Core>) -> ApprovalFactory {
    let core = core.clone();
    Arc::new(move |cancel: &CancelToken| -> Arc<dyn ApprovalGate> { Arc::new(SubagentGate { core: core.clone(), cancel: cancel.clone() }) })
}

/// Waits for an approval's answer: denied on cancel, shutdown or timeout.
fn wait_decision(core: &Core, rx: &crossbeam_channel::Receiver<ApprovalDecision>, cancel: Option<&CancelToken>) -> ApprovalDecision {
    let deadline = Instant::now() + core.cfg.approval_timeout;
    loop {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(d) => return d,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return ApprovalDecision::Deny,
            Err(_) if cancel.is_some_and(CancelToken::is_cancelled) || core.is_shutting_down() => return ApprovalDecision::Deny,
            Err(_) if Instant::now() >= deadline => return ApprovalDecision::Deny,
            Err(_) => {}
        }
    }
}

/// `(parent session, agent id)` of a subagent's composite session id.
fn subagent_of(session: &SessionId) -> (SessionId, &str) {
    match session.as_str().split_once('/') {
        Some((parent, agent)) => (SessionId(parent.to_string()), agent),
        None => (session.clone(), "agent"),
    }
}

impl ApprovalGate for SubagentGate {
    fn ask(&self, req: ApprovalReq) -> ApprovalDecision {
        if self.core.is_shutting_down() || self.cancel.is_cancelled() {
            return ApprovalDecision::Deny;
        }
        let call_id = req.call_id.clone();
        let (id, tx, rx) = self.core.approvals.register();
        let _hook = self.cancel.on_cancel(move || {
            let _ = tx.try_send(ApprovalDecision::Deny);
        });
        let (parent, agent) = subagent_of(&req.session);
        let summary = format!("(subagent {agent}) {}", req.summary);
        self.core.bus.publish(Event::ApprovalRequested {
            approval: id,
            session: parent,
            tool: req.tool,
            summary,
            detail: req.detail,
            run: None,
            proposed_rule: None,
        });
        let decision = wait_decision(&self.core, &rx, Some(&self.cancel));
        self.core.approvals.note_denial(id, &call_id);
        self.core.approvals.remove(id);
        self.core.bus.publish(Event::ApprovalResolved { approval: id, decision });
        decision
    }

    fn denial_reason(&self, call_id: &str) -> Option<String> {
        self.core.approvals.denial_reason(call_id)
    }
}

/// Trust for repository content (see the module docs): a digest the user allowed before is
/// trusted; otherwise one approval round trip, refused when no client is connected to answer.
struct TrustPrompt {
    core: Arc<Core>,
}

fn trusted(core: &Core, digest: &str) -> bool {
    f_rows(core, |c| c.query_row("SELECT COUNT(*) FROM trusted_content WHERE digest = ?1", params![digest], |r| r.get::<_, i64>(0)))
        .is_ok_and(|n| n > 0)
}

impl Trust for TrustPrompt {
    fn trust(&self, session: &SessionId, subject: &str, lines: &[String], digests: &[String]) -> bool {
        if !digests.is_empty() && digests.iter().all(|d| trusted(&self.core, d)) {
            return true;
        }
        if self.core.is_shutting_down() || self.core.bus.client_count() == 0 {
            return false;
        }
        let (id, _tx, rx) = self.core.approvals.register();
        self.core.bus.publish(Event::ApprovalRequested {
            approval: id,
            session: session.clone(),
            tool: "trust".into(),
            summary: format!("trust {subject}"),
            detail: lines.join("\n"),
            run: None,
            proposed_rule: None,
        });
        let decision = wait_decision(&self.core, &rx, None);
        self.core.approvals.remove(id);
        self.core.bus.publish(Event::ApprovalResolved { approval: id, decision });
        if !decision.is_allowed() {
            return false;
        }
        for d in digests {
            let r = f_rows(&self.core, |c| {
                c.execute(
                    "INSERT OR REPLACE INTO trusted_content (digest, subject, at_ms) VALUES (?1, ?2, ?3)",
                    params![d, subject, now_ms()],
                )
            });
            if let Err(e) = r {
                eprintln!("marlowe: trust for {subject} not kept: {e}");
            }
        }
        true
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

/// Tells the user when the prefix budget cut something (design 3.4: never silent): the
/// instruction files, repo map or skills index that did not fit. Each distinct cut is said once
/// per daemon (every session on one model cuts alike); `/context` shows it any time. The wire has
/// no notice kind for this yet, so it goes out as an error line.
struct PrefixNotice {
    core: Arc<Core>,
}

impl PrefixNotice {
    fn check(&self, convo: &Conversation) {
        let warnings = convo.ext.get::<PrefixFit>().map(|f| f.warnings.clone()).unwrap_or_default();
        if warnings.is_empty() {
            return;
        }
        let st = state(&self.core);
        let mut reported = lock(&st.prefix_reported);
        let fresh: Vec<String> = warnings.into_iter().filter(|w| !reported.contains(w)).collect();
        if fresh.is_empty() {
            return;
        }
        reported.extend(fresh.iter().cloned());
        let message = format!("context: {} (see /context)", fresh.join("; "));
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

// ---------------------------------------------------------------- background agents

/// F's tables in `cowork.db`: `background_agents` and `trusted_content`.
#[derive(Default)]
struct FRows(Mutex<Option<Connection>>);

fn f_rows<T>(core: &Core, f: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> Result<T, String> {
    let rows = core.ext.get_or_init(FRows::default);
    let mut db = lock(&rows.0);
    if db.is_none() {
        let conn = Connection::open(paths::cowork_db(&core.home)).map_err(|e| e.to_string())?;
        conn.busy_timeout(Duration::from_secs(5)).map_err(|e| e.to_string())?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS background_agents (
                session TEXT NOT NULL,
                id TEXT NOT NULL,
                label TEXT NOT NULL,
                started_ms INTEGER NOT NULL,
                lost INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS trusted_content (
                digest TEXT PRIMARY KEY,
                subject TEXT NOT NULL,
                at_ms INTEGER NOT NULL
            );",
        )
        .map_err(|e| e.to_string())?;
        *db = Some(conn);
    }
    f(db.as_ref().expect("opened")).map_err(|e| e.to_string())
}

/// Records background agents while they run; stops them when the daemon shuts down.
struct Ledger {
    core: Arc<Core>,
}

impl BackgroundLedger for Ledger {
    fn started(&self, session: &SessionId, id: &str, label: &str) {
        let r = f_rows(&self.core, |c| {
            c.execute(
                "INSERT INTO background_agents (session, id, label, started_ms, lost) VALUES (?1, ?2, ?3, ?4, 0)",
                params![session.as_str(), id, label, now_ms()],
            )
        });
        if let Err(e) = r {
            eprintln!("marlowe: background agent {id} not recorded: {e}");
        }
    }

    fn finished(&self, session: &SessionId, id: &str) {
        let _ = f_rows(&self.core, |c| {
            c.execute("DELETE FROM background_agents WHERE session = ?1 AND id = ?2 AND lost = 0", params![session.as_str(), id])
        });
    }

    fn stopping(&self) -> bool {
        self.core.is_shutting_down()
    }
}

/// At start (from `schedule_ops::on_boot`): every background agent still recorded was running
/// when the last daemon stopped, so it was lost. Must not block.
pub(crate) fn on_boot(core: &Arc<Core>) {
    if let Err(e) = f_rows(core, |c| c.execute("UPDATE background_agents SET lost = 1", [])) {
        eprintln!("marlowe: background agents not checked: {e}");
    }
}

/// The lost background agents of `session`, removed from the table.
fn take_lost(core: &Core, session: &SessionId) -> Vec<(String, String)> {
    f_rows(core, |c| {
        let mut st = c.prepare("SELECT id, label FROM background_agents WHERE session = ?1 AND lost = 1 ORDER BY started_ms")?;
        let rows: rusqlite::Result<Vec<(String, String)>> =
            st.query_map(params![session.as_str()], |r| Ok((r.get(0)?, r.get(1)?)))?.collect();
        let rows = rows?;
        if !rows.is_empty() {
            c.execute("DELETE FROM background_agents WHERE session = ?1 AND lost = 1", params![session.as_str()])?;
        }
        Ok(rows)
    })
    .unwrap_or_default()
}

/// Tells a session, on its next turn, about background agents a restart lost (it was told it
/// would hear when they finished). The note rides on the turn's text, not in memory.
struct LostAgents {
    core: Arc<Core>,
}

impl ToolMiddleware for LostAgents {
    fn name(&self) -> &str {
        "lost-agents"
    }

    fn before_turn(&self, convo: &mut Conversation, text: &mut String) -> TurnFlow {
        for (id, label) in take_lost(&self.core, &convo.session) {
            text.push_str(&format!(
                "\n\n[harness: background agent {id} ({label}) was stopped when Marlowe restarted; it did not finish and \
                 no report will come. Start it again if it is still needed.]"
            ));
        }
        TurnFlow::Continue
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
    let trust: Arc<dyn Trust> = Arc::new(TrustPrompt { core: core.clone() });
    vec![
        InstructionsMiddleware::with_trust(src.clone(), trust.clone()),
        CommandsMiddleware::with_schedule(src, run, Some(schedule), Some(trust)),
        WorkingSet::new(),
        WorktreeMiddleware::new(),
        LoopGuard::new(),
        Arc::new(PrefixNotice { core: core.clone() }),
        Arc::new(LostAgents { core: core.clone() }),
        Arc::new(GoodMark { core: core.clone() }),
    ]
}

/// `/good`: marks this session, as it stands, worth keeping for `marlowe export trajectories`
/// (track M's keep rule): `daemon_kv` `export.good.<session>` = `{"upto": <stored messages>,
/// "at_ms": <now>}`, the same row `marlowe export good` writes. No model call.
struct GoodMark {
    core: Arc<Core>,
}

/// The `daemon_kv` key prefix `marlowe export` reads (`export::source::GOOD_PREFIX`).
const GOOD_PREFIX: &str = "export.good.";

impl ToolMiddleware for GoodMark {
    fn name(&self) -> &str {
        "good-mark"
    }

    fn before_turn(&self, convo: &mut Conversation, text: &mut String) -> TurnFlow {
        if text.trim() != "/good" {
            return TurnFlow::Continue;
        }
        let upto = convo.messages.len();
        let value = serde_json::json!({"upto": upto, "at_ms": now_ms()}).to_string();
        let reply = match self.core.store.set_kv(&format!("{GOOD_PREFIX}{}", convo.session), &value) {
            Ok(()) => format!(
                "/good: this session ({upto} messages) is marked worth keeping; `marlowe export trajectories` takes it when its project has export enabled"
            ),
            Err(e) => format!("/good: the mark was not saved: {e}"),
        };
        TurnFlow::Block(reply)
    }
}

/// F's services for one session, added by `stack.rs`.
pub(crate) fn services(core: &Arc<Core>, convo: &Conversation, services: Services) -> Services {
    let session = convo.session.clone();
    let resolver: Arc<dyn ModelResolver> = Arc::new(LibraryResolver { core: core.clone(), session: session.clone() });
    let tools_core = core.clone();
    let model = core.brains.agent_parts(&Slot::Conductor).ok().map(|(_, info, _)| info);
    let tools: HostFactory = Arc::new(move |workspace: Option<&Path>| -> Arc<dyn ToolHost> {
        (tools_core.tools)(&HostRequest { workspace: workspace.map(Path::to_path_buf), model: model.clone(), session: None })
    });
    // The session's own tools for a worktree it entered or a root it added (built as session.rs
    // builds them, from the conductor's model).
    let rooted = tools.clone();
    let hosts = WorkspaceHosts::new(move |root: &Path| rooted(Some(root)));
    let worktree_provider = worktrees(core);
    let sub = Subagents::new(SubagentConfig {
        resolver: resolver.clone(),
        tools,
        approvals: subagent_approvals(core),
        worktrees: worktree_provider.clone(),
        sources: sources(core, None),
        wake: Some(wake(core)),
        middleware: vec![WorkingSet::new(), LoopGuard::new()],
        cloud_slots: CLOUD_SLOTS,
        ledger: Some(Arc::new(Ledger { core: core.clone() })),
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
        .with(hosts)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_subagent_approval_goes_to_its_parent_session() {
        let id = SessionId("abc123/agent-2".into());
        let (parent, agent) = subagent_of(&id);
        assert_eq!((parent.as_str(), agent), ("abc123", "agent-2"));
        let (parent, _) = subagent_of(&SessionId("plain".into()));
        assert_eq!(parent.as_str(), "plain");
    }
}
