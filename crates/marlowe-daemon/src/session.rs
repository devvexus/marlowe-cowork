//! Session runners: one thread per active session, owning its `Conversation`.
//!
//! An idle session starts a turn on `Ask`. While a turn runs, further asks go to the steer inbox
//! (the loop folds them in at a step boundary); whatever is still unread when the turn ends runs
//! as the next turn. `Interrupt` cancels the turn and drops what was queued behind it.
//!
//! Crew results arrive as relays ([`SessionHandle::relay`]): an idle session runs a short relay
//! turn; a busy one gets the result as a steer at its next step (an untainted result) or as a
//! relay turn right after the current one (a tainted result, so the exposure is raised before the
//! conductor acts on it). A relay is never ingested into memory at user trust. A relay turn runs
//! on the model the conversation is on; the user's ask that arrives during one is not folded
//! into it but runs as its own turn next, so each gets its own reply.

use std::collections::VecDeque;
use std::time::Instant;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use crossbeam_channel::{unbounded, Receiver, RecvTimeoutError, Sender};
use marlowe_agent::{
    Agent, AgentEvent, ApprovalGate, ApprovalReq, Conversation, Environment, NoMemory, Notification, NotificationKind,
    PersonaBlock, Steer, SteerRefused, SystemPrompt, ToolHost, TurnObserver, TurnOutcome, TurnStop,
};
use marlowe_brains::{Admission, LoadCost, Priority};
use marlowe_model::{CancelToken, Message, ModelRef, Usage};
use marlowe_model::ImageRef;
use marlowe_proto::{ApprovalDecision, Event, HeadlessOptions, PhaseEvent, PhaseKind, SessionId, Slot};

use crate::daemon::Core;
use crate::mind::{ComposedTools, MindMemory, MindTools, RecallSink};
use crate::phases::TurnClock;
use crate::checkpoint::SessionRecorder;
use crate::stack::{self, Stack};
use crate::{lock, HostRequest};

/// Share of the effective context window the memory tier may use (capped at
/// `marlowe_mind::MAX_INJECT_TOKENS`).
const MEMORY_TIER_DIVISOR: u32 = 8;

/// Work for the runner thread that is not a turn (a rewind): runs with the conversation while no
/// turn does.
pub(crate) type AdminJob = Box<dyn FnOnce(&mut Conversation) + Send>;

pub(crate) enum RunnerCmd {
    Turn(TurnInput),
    /// See [`SessionHandle::run_admin`].
    Admin(AdminJob),
    Stop,
}

/// What an ask carries beside its words: attached images (U3) and `marlowe -p`'s per-run
/// options (H4).
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct AskExtras {
    pub images: Vec<ImageRef>,
    pub options: Option<HeadlessOptions>,
    /// When the daemon took the ask, and how long it spent since the user's Enter (the client's
    /// stamp against the daemon's). Only an ask that starts a turn at once carries them: one that
    /// queued behind a running turn starts its clock when it runs.
    pub received: Option<std::time::Instant>,
    pub sending_ms: Option<u64>,
}

impl AskExtras {
    fn is_empty(&self) -> bool {
        self.images.is_empty() && self.options.is_none()
    }

    /// Folds a later ask's extras in: images add up, the later options win.
    fn merge(&mut self, later: AskExtras) {
        // A queued ask's clock starts when it runs, not when it was typed.
        self.images.extend(later.images);
        if later.options.is_some() {
            self.options = later.options;
        }
    }
}

/// What one turn starts from: the user's words, or a crew result to relay.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TurnInput {
    pub text: String,
    /// A crew relay: not the user's words, so memory does not ingest it at user trust.
    pub relay: bool,
    /// The relayed result carries untrusted content; the conversation is tainted before the turn.
    pub tainted: bool,
    pub extras: AskExtras,
}

impl TurnInput {
    pub fn user(text: String) -> Self {
        Self { text, relay: false, tainted: false, extras: AskExtras::default() }
    }

    pub fn relay(text: String, tainted: bool) -> Self {
        Self { text, relay: true, tainted, extras: AskExtras::default() }
    }
}

/// Every relay's text starts with this, so a relay steer left unread when a turn ends is told
/// apart from the user's own steers.
pub(crate) const RELAY_PREFIX: &str = "[crew · run ";

fn is_relay(text: &str) -> bool {
    text.starts_with(RELAY_PREFIX)
}

struct RunState {
    busy: bool,
    /// The token of the running (or about-to-run) turn.
    cancel: CancelToken,
    /// Asks too long to be a steer; run after the current turn.
    queued: Vec<String>,
    /// The queued asks' images and options, for the turn that runs them.
    queued_extras: AskExtras,
    /// Crew results waiting for the current turn to end; each runs as a relay turn.
    relays: Vec<TurnInput>,
    /// The running turn is a relay turn: the user's asks queue for their own turn.
    relay_turn: bool,
    /// Job notices and background-agent reports whose relay turn could not run (relays never load
    /// a model): they go out after the user's next turn, when the conductor's model is up.
    held: Vec<TurnInput>,
    /// Set when the runner has exited or is about to; asks must reload the session.
    closed: bool,
}

/// Why an ask was not taken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Enqueue {
    /// The runner has exited (idle eviction or a failure); reload the session and retry.
    Closed,
    Refused(String),
}

impl std::fmt::Display for Enqueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Enqueue::Closed => f.write_str("the session has stopped"),
            Enqueue::Refused(m) => f.write_str(m),
        }
    }
}

pub(crate) struct SessionHandle {
    pub id: SessionId,
    pub workspace: Option<String>,
    title: Mutex<String>,
    state: Mutex<RunState>,
    cmd_tx: Sender<RunnerCmd>,
    inbox_tx: Sender<Steer>,
    inbox_rx: Receiver<Steer>,
}

impl SessionHandle {
    pub fn new(id: SessionId, title: String, workspace: Option<String>) -> (Self, Receiver<RunnerCmd>) {
        let (cmd_tx, cmd_rx) = unbounded();
        let (inbox_tx, inbox_rx) = unbounded();
        let state =
            RunState {
                busy: false,
                cancel: CancelToken::new(),
                queued: Vec::new(),
                queued_extras: AskExtras::default(),
                relays: Vec::new(),
                relay_turn: false,
                held: Vec::new(),
                closed: false,
            };
        let h = Self { id, workspace, title: Mutex::new(title), state: Mutex::new(state), cmd_tx, inbox_tx, inbox_rx };
        (h, cmd_rx)
    }

    pub fn is_busy(&self) -> bool {
        lock(&self.state).busy
    }

    /// Sets the title from the first user message; returns it when this call set it.
    pub fn take_title(&self, text: &str, max_chars: usize) -> Option<String> {
        let mut t = lock(&self.title);
        if !t.is_empty() {
            return None;
        }
        *t = title_from(text, max_chars);
        Some(t.clone())
    }

    /// Renames the session as far as the runner is concerned (the first message no longer titles
    /// it).
    pub fn set_title(&self, title: &str) {
        *lock(&self.title) = title.to_string();
    }

    /// Runs `job` on the runner thread with the conversation, while the session is idle. The
    /// session counts as busy until it returns, so asks that arrive meanwhile queue behind it.
    /// Refused while a turn runs or after the runner has exited.
    pub fn run_admin(&self, job: AdminJob) -> Result<(), Enqueue> {
        let mut st = lock(&self.state);
        if st.closed {
            return Err(Enqueue::Closed);
        }
        if st.busy {
            return Err(Enqueue::Refused("a turn is running; interrupt it first".into()));
        }
        st.busy = true;
        st.cancel = CancelToken::new();
        self.cmd_tx.send(RunnerCmd::Admin(job)).map_err(|_| Enqueue::Closed)
    }

    /// Starts a turn, or queues `text` behind the running one.
    #[cfg(test)]
    pub fn enqueue(&self, text: String) -> Result<(), Enqueue> {
        self.enqueue_with(text, AskExtras::default())
    }

    /// [`Self::enqueue`] with images and options. An ask that carries them is never a steer: it
    /// queues for a turn of its own.
    pub fn enqueue_with(&self, text: String, extras: AskExtras) -> Result<(), Enqueue> {
        let mut st = lock(&self.state);
        if st.closed {
            return Err(Enqueue::Closed);
        }
        if !st.busy {
            st.busy = true;
            st.cancel = CancelToken::new();
            let input = TurnInput { extras, ..TurnInput::user(text) };
            return self.cmd_tx.send(RunnerCmd::Turn(input)).map_err(|_| Enqueue::Closed);
        }
        // Behind queued asks, with images or options, or during a relay turn (whose reply is the
        // relay's): a turn of its own.
        if !st.queued.is_empty() || !extras.is_empty() || st.relay_turn {
            st.queued.push(text);
            st.queued_extras.merge(extras);
            return Ok(());
        }
        match Steer::admit(&text) {
            Ok(s) => self.inbox_tx.send(s).map_err(|_| Enqueue::Closed),
            Err(SteerRefused::TooLong { .. }) => {
                st.queued.push(text);
                Ok(())
            }
            Err(e) => Err(Enqueue::Refused(e.to_string())),
        }
    }

    /// Delivers a crew result (`text` starts with [`RELAY_PREFIX`]). Idle: a relay turn starts.
    /// Busy: an untainted result steers the running turn at its next step; a tainted one (or one
    /// behind queued asks) runs as a relay turn after it.
    pub fn relay(&self, text: String, tainted: bool) -> Result<(), Enqueue> {
        let mut st = lock(&self.state);
        if st.closed {
            return Err(Enqueue::Closed);
        }
        if !st.busy {
            st.busy = true;
            // Set now: an ask arriving before the runner picks the turn up must not steer into it.
            st.relay_turn = true;
            st.cancel = CancelToken::new();
            return self.cmd_tx.send(RunnerCmd::Turn(TurnInput::relay(text, tainted))).map_err(|_| Enqueue::Closed);
        }
        if !tainted && st.queued.is_empty() {
            // Built directly: a relay is not user text and is not held to the steer length cap.
            return self.inbox_tx.send(Steer { text }).map_err(|_| Enqueue::Closed);
        }
        st.relays.push(TurnInput::relay(text, tainted));
        Ok(())
    }

    /// Closes the handle if no turn is running or queued; true when it closed.
    pub fn close_if_idle(&self) -> bool {
        let mut st = lock(&self.state);
        // Held notices would be lost with the runner: the session stays until it delivers them.
        if st.busy || !st.held.is_empty() {
            return false;
        }
        st.closed = true;
        true
    }

    pub fn close(&self) {
        let mut st = lock(&self.state);
        st.closed = true;
        st.busy = false;
    }

    /// Cancels the running turn and drops the user's asks queued behind it. Crew results are
    /// kept: they relay once the interrupted turn has ended.
    pub fn interrupt(&self) {
        let mut st = lock(&self.state);
        if st.busy {
            st.cancel.cancel();
        }
        st.queued.clear();
        while let Ok(s) = self.inbox_rx.try_recv() {
            if is_relay(&s.text) {
                st.relays.push(TurnInput::relay(s.text, false));
            }
        }
    }

    /// Cancels the running turn and ends the runner after it.
    pub fn stop(&self) {
        let mut st = lock(&self.state);
        st.cancel.cancel();
        st.queued.clear();
        let _ = self.cmd_tx.send(RunnerCmd::Stop);
    }
}

/// A one-line title from the first line of `text`.
fn title_from(text: &str, max_chars: usize) -> String {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    let mut t: String = line.chars().take(max_chars).collect();
    if line.chars().count() > max_chars {
        t = t.trim_end().to_string();
        t.push('…');
    }
    t
}

pub(crate) fn spawn_runner(core: Arc<Core>, handle: Arc<SessionHandle>, convo: Conversation, cmd_rx: Receiver<RunnerCmd>) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name(format!("marlowe-session-{}", &handle.id.as_str()[..handle.id.as_str().len().min(8)]))
        .spawn(move || runner(core, handle, convo, cmd_rx))
        .expect("spawn session runner")
}

fn runner(core: Arc<Core>, handle: Arc<SessionHandle>, convo: Conversation, cmd_rx: Receiver<RunnerCmd>) {
    // This runner's own recorder, set once the stack is up. A later runner of the same session
    // (after an idle eviction) has its own, so releasing must not take that one away.
    let mut recorder = None;
    serve(&core, &handle, convo, &cmd_rx, &mut recorder);
    if let Some(r) = &recorder {
        crate::checkpoint::release(&core, &handle.id, r);
    }
    core.release(&handle);
}

fn serve(
    core: &Arc<Core>,
    handle: &Arc<SessionHandle>,
    mut convo: Conversation,
    cmd_rx: &Receiver<RunnerCmd>,
    mine: &mut Option<Arc<SessionRecorder>>,
) {
    // The tool set is fixed for the session from the conductor's model as of now (prompt cache).
    let model = guard(|| conductor_model(core)).ok().flatten();
    let request = HostRequest { workspace: convo.workspace.clone(), model, session: Some(convo.session.clone()) };
    let tools = match guard(|| (core.tools)(&request)) {
        Ok(t) => t,
        Err(e) => {
            core.bus.publish(Event::Error { session: Some(handle.id.clone()), message: format!("tools unavailable: {e}") });
            return;
        }
    };
    let tools = stack::tools(core, &convo, tools);
    let tools: Arc<dyn ToolHost> = match &core.mind {
        Some(mind) => {
            let memory = MindTools::new(mind.clone(), workspace_of(&convo)).with_events(publisher(core));
            Arc::new(ComposedTools::new(tools, memory))
        }
        None => tools,
    };
    // The conductor alone gets `dispatch` (crew hosts never carry it: depth 1).
    let tools = crate::crew::with_dispatch(core, tools);
    // Turns run by this runner; an evicted runner that ran any writes an episode.
    let mut turns_run = 0u32;
    let mut evicted = false;
    // A session loaded from the store keeps the taint of what it read before: the stored flag
    // (set by `Core::session`) plus anything its history shows.
    convo.recompute_exposure(tools.as_ref());
    // A daemon killed while tools ran left an assistant message with calls and no results.
    match crate::checkpoint::repair_dangling(&core.store, &mut convo) {
        Ok(0) => {}
        Ok(n) => {
            let message = format!("{n} tool call(s) were running when the daemon last stopped; they are marked as not finished");
            core.bus.publish(Event::Error { session: Some(handle.id.clone()), message });
        }
        Err(e) => {
            core.bus.publish(Event::Error { session: Some(handle.id.clone()), message: format!("session not repaired: {e}") });
        }
    }
    // Middleware, observers and services from every track, for this runner's lifetime.
    let recorder = crate::checkpoint::start_runner(core, &convo);
    *mine = Some(recorder.clone());
    let stack = Stack::build(core, &convo);
    for o in &stack.observers {
        let _ = guard(|| o.on_session_start(&convo));
    }
    loop {
        let work = match cmd_rx.recv_timeout(core.cfg.session_idle) {
            Ok(RunnerCmd::Turn(input)) => Work::Turn(input),
            Ok(RunnerCmd::Admin(job)) => Work::Admin(job),
            Ok(RunnerCmd::Stop) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) if core.evict_if_idle(handle) => {
                evicted = true;
                break;
            }
            Err(RecvTimeoutError::Timeout) => continue,
        };
        let mut next = VecDeque::new();
        match work {
            Work::Turn(input) => next.push_back(input),
            Work::Admin(job) => {
                let _ = guard(|| job(&mut convo));
                settle(core, handle, &mut next);
            }
        }
        while let Some(input) = next.pop_front() {
            if core.is_shutting_down() {
                lock(&handle.state).busy = false;
                break;
            }
            let cancel = {
                let mut st = lock(&handle.state);
                st.relay_turn = input.relay;
                st.cancel.clone()
            };
            run_one(core, handle, &mut convo, &tools, &stack, &recorder, &input, &cancel);
            lock(&handle.state).relay_turn = false;
            turns_run += 1;
            // Hooks' systemMessage lines, failures and diagnostics (track E) reach the user.
            if let Some(r) = stack.services.get::<Arc<marlowe_agent::hooks::HookRunner>>() {
                for message in r.take_messages() {
                    core.bus.publish(Event::Error { session: Some(handle.id.clone()), message: format!("hook: {message}") });
                }
            }
            // The user's turn brought the conductor's model up: crew results that could not
            // load it themselves go out now (this session's land in the queue read below).
            if !input.relay {
                crate::crew::retry_deferred(core);
                let mut st = lock(&handle.state);
                let held = std::mem::take(&mut st.held);
                st.relays.extend(held);
            }
            settle(core, handle, &mut next);
        }
        if core.is_shutting_down() {
            break;
        }
    }
    if let (true, Some(mind)) = (evicted && turns_run > 0 && !core.is_shutting_down(), &core.mind) {
        let _ = guard(|| crate::mind_ops::write_episode(core, mind, &convo));
    }
    // SessionEnd hooks run, and the session's permissions and hook runner go (track E).
    let _ = guard(|| crate::mode_ops::forget(core, &handle.id, "other"));
}

enum Work {
    Turn(TurnInput),
    Admin(AdminJob),
}

/// After a turn or an admin job: decides, under the state lock, what runs next. An ask arriving
/// now either lands in `next` or starts a fresh turn through the command channel. The session
/// stays busy while `next` is not empty.
fn settle(core: &Arc<Core>, handle: &Arc<SessionHandle>, next: &mut VecDeque<TurnInput>) {
    let mut st = lock(&handle.state);
    let (relayed, mut left): (Vec<String>, Vec<String>) =
        handle.inbox_rx.try_iter().map(|s| s.text).partition(|t| is_relay(t));
    left.append(&mut st.queued);
    let mut relays: Vec<TurnInput> = relayed.into_iter().map(|t| TurnInput::relay(t, false)).collect();
    relays.append(&mut st.relays);
    if !left.is_empty() {
        let extras = std::mem::take(&mut st.queued_extras);
        next.push_back(TurnInput { extras, ..TurnInput::user(left.join("\n\n")) });
    }
    next.extend(relays);
    if !next.is_empty() && !core.is_shutting_down() {
        st.relay_turn = next.front().is_some_and(|i| i.relay);
        st.cancel = CancelToken::new();
    } else {
        next.clear();
        st.busy = false;
    }
}

fn workspace_of(convo: &Conversation) -> Option<String> {
    convo.workspace.as_ref().map(|p| p.display().to_string())
}

/// The workspace's branch, dirty count and last commit, from the Sentinel's short-lived cache.
/// Read-only git; a broken watcher yields no context rather than a failed turn.
fn repo_context(core: &Core, workspace: &str) -> Option<marlowe_proto::GitInfo> {
    let s = core.sentinel.as_ref()?;
    guard(|| s.git_context(std::path::Path::new(workspace))).ok().flatten()
}

/// Publishes on the bus; handed to the memory hook and tools.
fn publisher(core: &Arc<Core>) -> RecallSink {
    let c = core.clone();
    Arc::new(move |e: Event| {
        c.bus.publish(e);
    })
}

/// The conductor slot's model, without building a provider.
fn conductor_model(core: &Core) -> Option<marlowe_model::ModelInfo> {
    let (model, _) = core.brains.resolve(&Slot::Conductor).ok()?;
    core.brains.model_info(&model).ok()
}

#[allow(clippy::too_many_arguments)]
fn run_one(
    core: &Arc<Core>,
    handle: &SessionHandle,
    convo: &mut Conversation,
    tools: &Arc<dyn ToolHost>,
    stack: &Stack,
    recorder: &SessionRecorder,
    input: &TurnInput,
    cancel: &CancelToken,
) {
    let text = input.text.as_str();
    if input.tainted {
        convo.exposure.tainted = true;
    }
    let session = convo.session.clone();
    let turn = convo.next_turn;
    let clock = TurnClock::start(core, &session, turn, input.extras.received, input.extras.sending_ms);
    clock.emit(PhaseEvent::start(PhaseKind::Admit));
    let admit_started = Instant::now();
    // The model the conversation is on (its `/model` switch, else the conductor's): a relay turn
    // runs where the user's turns do, on the model already loaded.
    let parts = guard(|| crate::conductor_ops::session_parts(core, &session)).and_then(|r| r);
    let (provider, info, effort) = match parts {
        Ok(p) => p,
        Err(e) => {
            core.bus.publish(Event::Error { session: Some(session), message: e });
            return;
        }
    };
    let model = info.model_ref();
    clock.set_model(&model);
    // A relay turn runs with nobody waiting on it (at start, at 3am): it never loads a model.
    let who = if input.relay { Priority::Background } else { Priority::Foreground };
    // The user's own turn on a local model: a stopped Ollama is started first, not reported.
    if !input.relay {
        if let Err(message) = ensure_engine(core, &clock, &model) {
            core.bus.publish(Event::Error { session: Some(session), message });
            return;
        }
    }
    let admitted = admit_conductor(core, &session, &model, cancel, who);
    if let Err(message) = &admitted {
        let message = message.clone();
        if input.relay {
            // The run stays due; the user's next turn delivers it on a loaded model. The user
            // hears the outcome now all the same.
            eprintln!("marlowe: a relay waits for the next turn: {message}");
            crate::crew::announce_held(core, &session, &input.text);
            if !crate::crew::defer(core, &input.text) {
                // A job notice or an agent's report: held by the session, not dropped.
                lock(&handle.state).held.push(input.clone());
            }
        } else {
            core.bus.publish(Event::Error { session: Some(session), message });
        }
        return;
    }
    clock.emit(PhaseEvent::end(PhaseKind::Admit, admit_started.elapsed().as_millis() as u64));
    // A cold load's predicted cost, for the load line.
    clock.set_load(admitted.ok().flatten());
    let model_ref = model.clone();
    clock.emit(PhaseEvent::start(PhaseKind::Prompt));
    let prompt_started = Instant::now();
    let workspace = workspace_of(convo);
    let mut system = SystemPrompt::new();
    // The memory block describes the profile and memory tools, so only when a Mind is attached.
    if core.mind.is_some() {
        system = system.block(PersonaBlock::Memory);
    }
    // The crew block describes `dispatch`, so only when the supervisor is running.
    if core.crew.get().is_some() {
        system = system.block(PersonaBlock::Crew);
    }
    // Tool groups' blocks, only for groups this session exposes.
    system = system.blocks_from(tools.persona_blocks());
    let mut env = Environment::current(workspace.clone(), info.display.clone());
    env.repo = workspace.as_deref().and_then(|w| repo_context(core, w));
    env.hints = tools.prompt_hints();
    let mut system = system.user(core.user()).environment(env);
    let options = input.extras.options.clone().unwrap_or_default();
    // `marlowe -p` waits for its subagents; any other ask in the session does not.
    if options.print {
        convo.ext.insert(marlowe_agent::subagent::ForegroundAgents);
    } else {
        convo.ext.remove::<marlowe_agent::subagent::ForegroundAgents>();
    }
    if let Some(a) = &options.append_system_prompt {
        system = system.append(a.clone());
    }
    let mut agent = Agent::new(provider, info, tools.clone(), system);
    agent.effort = effort;
    // `marlowe -p`'s limits for this turn (H4).
    if let Some(n) = options.max_steps {
        agent.budget.max_steps = n;
    }
    if let Some(usd) = options.max_budget_usd {
        agent.budget.max_cost_usd = Some(usd);
    }
    agent.memory = match &core.mind {
        Some(mind) => {
            let budget = agent.context.effective_window() / MEMORY_TIER_DIVISOR;
            let memory = MindMemory::new(mind.clone(), workspace, turn, budget, clock.sink())
                .with_tail(core.ext.get_or_init(crate::phases::TailTarget::default));
            Arc::new(if input.relay { memory.relay() } else { memory })
        }
        None => Arc::new(NoMemory),
    };
    agent.approvals = Arc::new(Gate { core: core.clone(), cancel: cancel.clone(), observers: stack.observers.clone() });
    // After the host's own middleware (seeded by `Agent::new`).
    agent.middleware.extend(stack.middleware.iter().cloned());
    agent.observers = stack.observers.clone();
    agent.services = stack.services.clone();
    // `marlowe -p`'s rules and mode (H4) go into the session's permission layer, so a denied
    // tool is refused even where it would run unasked.
    if let Some(p) = stack.services.get::<marlowe_agent::approval::Permissions>() {
        if let Some(mode) = options.permission_mode {
            p.set_mode(mode);
            core.bus.publish(Event::ModeChanged { session: session.clone(), mode });
        }
        if let Err(e) = p.grant(&options.allowed_tools, marlowe_proto::SettingsScope::Session) {
            core.bus.publish(Event::Error { session: Some(session.clone()), message: format!("allowed tools not applied: {e}") });
        }
        if let Err(e) = p.deny_for_session(&options.disallowed_tools) {
            core.bus.publish(Event::Error { session: Some(session.clone()), message: format!("disallowed tools not applied: {e}") });
        }
    }
    convo.pending_images = input.extras.images.clone();
    let window = agent.context.window_tokens;

    core.bus.publish(Event::TurnStarted { session: session.clone(), turn, model, user: text.to_string() });
    let before_len = convo.messages.len();
    // Rows are written as the loop produces them; this turn's are told where it began.
    recorder.begin_turn(convo, &model_ref);
    // Crew results folded in as steers during this turn; delivered once the turn is saved.
    let mut steered: Vec<String> = Vec::new();
    clock.emit(PhaseEvent::end(PhaseKind::Prompt, prompt_started.elapsed().as_millis() as u64));
    let mut sink = |ev: AgentEvent| {
        if let AgentEvent::SteerApplied { text } = &ev {
            if is_relay(text) {
                steered.push(text.clone());
            }
        }
        clock.on_agent(&ev);
        let token = matches!(ev, AgentEvent::TextDelta(_) | AgentEvent::ReasoningDelta { .. });
        if let Some(e) = to_event(&session, turn, window, ev) {
            core.bus.publish(e);
            if token {
                clock.token_published();
            }
        }
    };
    let outcome = guard(|| agent.run_turn(convo, text, cancel, &handle.inbox_rx, &mut sink)).unwrap_or_else(|e| TurnOutcome {
        stop: TurnStop::Error,
        usage: Usage::default(),
        steps: 0,
        text: String::new(),
        error: Some(format!("the turn failed: {e}")),
        blocked: false,
        truncated: false,
    });
    if outcome.blocked {
        // A hook refused the prompt before anything ran: nothing to store, and it is not kept.
        let message = outcome.error.clone().unwrap_or_else(|| "the prompt was blocked".into());
        recorder.discard_turn(turn);
        core.bus.publish(Event::Error { session: Some(session.clone()), message });
        core.bus.publish(Event::TurnFinished { session, turn, usage: outcome.usage, stop: outcome.stop });
        return;
    }
    convo.next_turn = convo.next_turn.max(turn + 1);
    crate::checkpoint::end_turn(core, convo, turn);

    if convo.messages.len() == before_len {
        // Nothing was appended (the turn failed early): keep the user's words anyway.
        convo.push(Message::user(text));
    }
    recorder.finish_turn(convo, text, before_len);
    let (spans, numbers) = clock.finish();
    if let Err(e) = core.store.put_timings(&session, turn, spans.clone(), numbers.clone()) {
        eprintln!("marlowe: turn timings not saved: {e}");
    }
    core.bus.publish(Event::TurnTimings { session: session.clone(), turn, spans, numbers });
    let saved = recorder.take_error().map_or(Ok(()), Err);
    match saved {
        // The crew results this turn carried are in the session's history now: a restart must
        // not relay them again.
        Ok(()) => {
            for relay in input.relay.then_some(&input.text).into_iter().chain(&steered) {
                crate::crew::delivered(core, relay);
            }
        }
        Err(e) => {
            core.bus.publish(Event::Error { session: Some(session.clone()), message: e });
        }
    }
    if let Some(message) = outcome.error.clone().or_else(|| (outcome.stop == TurnStop::Error).then(|| "the turn failed".into())) {
        core.bus.publish(Event::Error { session: Some(session.clone()), message });
    }
    core.bus.publish(Event::TurnFinished { session, turn, usage: outcome.usage, stop: outcome.stop });
}

fn to_event(session: &SessionId, turn: u32, window: u32, ev: AgentEvent) -> Option<Event> {
    let session = session.clone();
    Some(match ev {
        // One model call's prompt plus its output: what the context holds now.
        AgentEvent::Usage(u) => Event::ContextUsage { session, turn, tokens: u.input + u.output, window },
        // Compaction shrinks the context before the next call: the meter drops now, not then.
        AgentEvent::Compacted { after_tokens, .. } => Event::ContextUsage { session, turn, tokens: u64::from(after_tokens), window },
        AgentEvent::TextDelta(text) => Event::TextDelta { session, turn, text },
        AgentEvent::ReasoningDelta { text, tokens } => Event::ReasoningDelta { session, turn, text, tokens },
        AgentEvent::TextRetracted { tokens } => Event::TextRetracted { session, turn, tokens },
        AgentEvent::ToolStarted { call_id, tool, summary } => Event::ToolStarted { session, turn, call_id, tool, summary },
        AgentEvent::ToolFinished { call_id, ok, summary } => Event::ToolFinished { session, turn, call_id, ok, summary },
        // Phases, the wire mark and the first-token stamp go through the turn's clock.
        AgentEvent::SteerApplied { .. }
        | AgentEvent::Phase(_)
        | AgentEvent::CallStarted { .. }
        | AgentEvent::FirstToken { .. } => return None,
    })
}

/// Runs `f`, turning a panic into an error message so one bad turn cannot kill the runner.
fn guard<T>(f: impl FnOnce() -> T) -> Result<T, String> {
    catch_unwind(AssertUnwindSafe(f)).map_err(|p| {
        p.downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| p.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "panic".into())
    })
}

/// Approvals as an ApprovalRequested / Approve round trip. Denies on timeout, cancel, shutdown.
/// The conductor's model may be used for this turn: hosted, already loaded, or a cold load the
/// planner says fits. A load that does not fit beside what is resident asks, with its cost (once
/// per model and resident set).
///
/// A cold load that goes ahead returns its [`LoadCost`] (the planner's predicted bytes, for the
/// load line); a model that is hosted or already resident returns `None`.
fn admit_conductor(core: &Arc<Core>, session: &SessionId, model: &ModelRef, cancel: &CancelToken, who: Priority) -> Result<Option<LoadCost>, String> {
    let admission = guard(|| core.brains.admit(model, who))?;
    let cost = match admission {
        Admission::Hosted | Admission::Resident => return Ok(None),
        Admission::Load(cost) => {
            eprintln!("marlowe: loading {model} for the conductor: {}", cost.describe());
            return Ok(Some(cost));
        }
        Admission::Refused(why) | Admission::Wait(why) => return Err(format!("not loading {model}: {why}")),
        // Only the user's own turn asks, and only it counts loads approved earlier.
        Admission::Ask(cost) if who == Priority::Foreground => cost,
        Admission::Ask(cost) => return Err(format!("not loading {model}: {}", cost.describe())),
    };
    let key = format!("{model}|{}", cost.resident.join(","));
    if lock(&core.approved_loads).contains(&key) {
        return Ok(Some(cost));
    }
    let gate = Gate { core: core.clone(), cancel: cancel.clone(), observers: Vec::new() };
    let decision = gate.ask(ApprovalReq {
        session: session.clone(),
        call_id: format!("load-{}", model.model_id),
        tool: "load model".into(),
        summary: format!("Load {} for this turn? It does not fit beside what is loaded.", model.model_id),
        detail: format!(
            "{}.\nLoading it anyway can spill into system memory and slow the whole machine.\nAssign a smaller conductor in the Library to avoid this.",
            cost.describe()
        ),
    });
    if decision.is_allowed() {
        lock(&core.approved_loads).insert(key);
        Ok(Some(cost))
    } else {
        Err(format!("not loading {model}: {}", cost.describe()))
    }
}

/// Starts the local engine `model` needs if it is not answering, with a live "starting Ollama"
/// line while it comes up. `Err` is why it could not be started, for the user. A running engine,
/// a hosted model and a remote server cost nothing and say nothing.
fn ensure_engine(core: &Arc<Core>, clock: &Arc<TurnClock>, model: &ModelRef) -> Result<(), String> {
    let began = Instant::now();
    let mut said = false;
    let outcome = guard(|| {
        core.brains.ensure_engine(model, &mut |line| {
            said = true;
            clock.emit(PhaseEvent::start(PhaseKind::Admit).stage("ollama").detail(line));
        })
    })?;
    if said {
        clock.emit(PhaseEvent::end(PhaseKind::Admit, began.elapsed().as_millis() as u64).stage("ollama"));
    }
    match outcome {
        marlowe_hw::EngineStart::Failed(why) => Err(why),
        marlowe_hw::EngineStart::Started(note) => {
            eprintln!("marlowe: {note}");
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Observers hear about each request (E's Notification hook).
struct Gate {
    core: Arc<Core>,
    cancel: CancelToken,
    observers: Vec<Arc<dyn TurnObserver>>,
}

impl ApprovalGate for Gate {
    fn ask(&self, req: ApprovalReq) -> ApprovalDecision {
        self.ask_with(req, None)
    }

    /// The modal shows the rule "don't ask again" would add (track E).
    fn ask_with(&self, req: ApprovalReq, proposed_rule: Option<String>) -> ApprovalDecision {
        if self.core.is_shutting_down() || self.cancel.is_cancelled() {
            return ApprovalDecision::Deny;
        }
        let note = Notification { kind: NotificationKind::PermissionPrompt, message: req.summary.clone() };
        for o in &self.observers {
            let _ = guard(|| o.on_notification(&req.session, &note));
        }
        let call_id = req.call_id.clone();
        let (id, tx, rx) = self.core.approvals.register();
        self.core.approvals.waits(id, &req.session);
        let _hook = self.cancel.on_cancel(move || {
            let _ = tx.try_send(ApprovalDecision::Deny);
        });
        self.core.bus.publish(Event::ApprovalRequested {
            approval: id,
            session: req.session,
            tool: req.tool,
            summary: req.summary,
            detail: req.detail,
            run: None,
            proposed_rule,
        });
        let decision = rx.recv_timeout(self.core.cfg.approval_timeout).unwrap_or(ApprovalDecision::Deny);
        self.core.approvals.note_denial(id, &call_id);
        self.core.approvals.remove(id);
        self.core.bus.publish(Event::ApprovalResolved { approval: id, decision });
        decision
    }

    fn denial_reason(&self, call_id: &str) -> Option<String> {
        self.core.approvals.denial_reason(call_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_take_the_first_nonblank_line() {
        assert_eq!(title_from("\n  fix the build  \nplease", 60), "fix the build");
        assert_eq!(title_from("abcdefgh", 4), "abcd…");
        assert_eq!(title_from("", 4), "");
    }

    #[test]
    fn a_busy_session_queues_and_interrupt_clears() {
        let (h, rx) = SessionHandle::new(SessionId("s".into()), String::new(), None);
        h.enqueue("one".into()).unwrap();
        assert!(matches!(rx.try_recv(), Ok(RunnerCmd::Turn(t)) if t == TurnInput::user("one".into())));
        assert!(h.is_busy());
        h.enqueue("two".into()).unwrap();
        h.enqueue("x".repeat(marlowe_agent::MAX_STEER_CHARS + 1)).unwrap();
        h.enqueue("three".into()).unwrap();
        assert_eq!(h.inbox_rx.len(), 1, "short asks steer");
        assert_eq!(lock(&h.state).queued.len(), 2, "after a long one, everything queues in order");
        let cancel = lock(&h.state).cancel.clone();
        h.interrupt();
        assert!(cancel.is_cancelled());
        assert!(h.inbox_rx.is_empty() && lock(&h.state).queued.is_empty());
    }

    #[test]
    fn relays_start_a_turn_steer_or_wait_by_taint() {
        let relay = |s: &str| format!("{RELAY_PREFIX}{s}");
        let (h, rx) = SessionHandle::new(SessionId("s".into()), String::new(), None);
        h.relay(relay("a"), false).unwrap();
        assert!(matches!(rx.try_recv(), Ok(RunnerCmd::Turn(t)) if t == TurnInput::relay(relay("a"), false)), "idle: a relay turn");
        h.relay(relay("b"), false).unwrap();
        assert_eq!(h.inbox_rx.len(), 1, "busy and untainted: a steer at the next step");
        h.relay(relay("c"), true).unwrap();
        assert_eq!(lock(&h.state).relays, vec![TurnInput::relay(relay("c"), true)], "tainted: after the turn");
        h.interrupt();
        assert!(h.inbox_rx.is_empty());
        assert_eq!(lock(&h.state).relays.len(), 2, "an interrupt keeps crew results");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn only_an_idle_handle_closes_and_a_closed_one_refuses_asks() {
        let (h, _rx) = SessionHandle::new(SessionId("s".into()), String::new(), None);
        h.enqueue("one".into()).unwrap();
        assert!(!h.close_if_idle(), "a busy session stays open");
        lock(&h.state).busy = false;
        assert!(h.close_if_idle());
        assert_eq!(h.enqueue("two".into()), Err(Enqueue::Closed));
    }
}
