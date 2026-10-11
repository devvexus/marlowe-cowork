//! Session runners: one thread per active session, owning its `Conversation`.
//!
//! An idle session starts a turn on `Ask`. While a turn runs, further asks go to the steer inbox
//! (the loop folds them in at a step boundary); whatever is still unread when the turn ends runs
//! as the next turn. `Interrupt` cancels the turn and drops what was queued behind it.
//!
//! Crew results arrive as relays ([`SessionHandle::relay`]): an idle session runs a short relay
//! turn; a busy one gets the result as a steer at its next step (an untainted result) or as a
//! relay turn right after the current one (a tainted result, so the exposure is raised before the
//! conductor acts on it). A relay is never ingested into memory at user trust.

use std::collections::VecDeque;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use crossbeam_channel::{unbounded, Receiver, RecvTimeoutError, Sender};
use marlowe_agent::{
    Agent, AgentEvent, ApprovalGate, ApprovalReq, Conversation, Environment, NoMemory, PersonaBlock, Steer,
    SteerRefused, SystemPrompt, ToolHost, TurnOutcome, TurnStop,
};
use marlowe_brains::{Admission, Priority};
use marlowe_model::{CancelToken, Message, ModelRef, Usage};
use marlowe_proto::{ApprovalDecision, Event, SessionId, Slot};

use crate::daemon::Core;
use crate::mind::{ComposedTools, MindMemory, MindTools, RecallSink};
use crate::{lock, now_ms};

/// Share of the effective context window the memory tier may use (capped at
/// `marlowe_mind::MAX_INJECT_TOKENS`).
const MEMORY_TIER_DIVISOR: u32 = 8;

pub(crate) enum RunnerCmd {
    Turn(TurnInput),
    Stop,
}

/// What one turn starts from: the user's words, or a crew result to relay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TurnInput {
    pub text: String,
    /// A crew relay: not the user's words, so memory does not ingest it at user trust.
    pub relay: bool,
    /// The relayed result carries untrusted content; the conversation is tainted before the turn.
    pub tainted: bool,
}

impl TurnInput {
    pub fn user(text: String) -> Self {
        Self { text, relay: false, tainted: false }
    }

    pub fn relay(text: String, tainted: bool) -> Self {
        Self { text, relay: true, tainted }
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
    /// Crew results waiting for the current turn to end; each runs as a relay turn.
    relays: Vec<TurnInput>,
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
            RunState { busy: false, cancel: CancelToken::new(), queued: Vec::new(), relays: Vec::new(), closed: false };
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

    /// Starts a turn, or queues `text` behind the running one.
    pub fn enqueue(&self, text: String) -> Result<(), Enqueue> {
        let mut st = lock(&self.state);
        if st.closed {
            return Err(Enqueue::Closed);
        }
        if !st.busy {
            st.busy = true;
            st.cancel = CancelToken::new();
            return self.cmd_tx.send(RunnerCmd::Turn(TurnInput::user(text))).map_err(|_| Enqueue::Closed);
        }
        if !st.queued.is_empty() {
            st.queued.push(text);
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
        if st.busy {
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
    serve(&core, &handle, convo, &cmd_rx);
    core.release(&handle);
}

fn serve(core: &Arc<Core>, handle: &Arc<SessionHandle>, mut convo: Conversation, cmd_rx: &Receiver<RunnerCmd>) {
    let tools = match guard(|| (core.tools)(convo.workspace.as_deref())) {
        Ok(t) => t,
        Err(e) => {
            core.bus.publish(Event::Error { session: Some(handle.id.clone()), message: format!("tools unavailable: {e}") });
            return;
        }
    };
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
    loop {
        let first = match cmd_rx.recv_timeout(core.cfg.session_idle) {
            Ok(RunnerCmd::Turn(input)) => input,
            Ok(RunnerCmd::Stop) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) if core.evict_if_idle(handle) => {
                evicted = true;
                break;
            }
            Err(RecvTimeoutError::Timeout) => continue,
        };
        let mut next = VecDeque::from([first]);
        while let Some(input) = next.pop_front() {
            if core.is_shutting_down() {
                lock(&handle.state).busy = false;
                break;
            }
            let cancel = lock(&handle.state).cancel.clone();
            run_one(core, handle, &mut convo, &tools, &input, &cancel);
            turns_run += 1;
            // The user's turn brought the conductor's model up: crew results that could not
            // load it themselves go out now (this session's land in the queue read below).
            if !input.relay {
                crate::crew::retry_deferred(core);
            }
            // Decide under the lock so an ask arriving now either lands in `left` or starts a
            // fresh turn through the command channel.
            let mut st = lock(&handle.state);
            let (relayed, mut left): (Vec<String>, Vec<String>) =
                handle.inbox_rx.try_iter().map(|s| s.text).partition(|t| is_relay(t));
            left.append(&mut st.queued);
            let mut relays: Vec<TurnInput> = relayed.into_iter().map(|t| TurnInput::relay(t, false)).collect();
            relays.append(&mut st.relays);
            if !left.is_empty() {
                next.push_back(TurnInput::user(left.join("\n\n")));
            }
            next.extend(relays);
            if !next.is_empty() && !core.is_shutting_down() {
                st.cancel = CancelToken::new();
            } else {
                next.clear();
                st.busy = false;
            }
        }
        if core.is_shutting_down() {
            break;
        }
    }
    if let (true, Some(mind)) = (evicted && turns_run > 0 && !core.is_shutting_down(), &core.mind) {
        let _ = guard(|| crate::mind_ops::write_episode(core, mind, &convo));
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

fn run_one(core: &Arc<Core>, handle: &SessionHandle, convo: &mut Conversation, tools: &Arc<dyn ToolHost>, input: &TurnInput, cancel: &CancelToken) {
    let text = input.text.as_str();
    if input.tainted {
        convo.exposure.tainted = true;
    }
    let session = convo.session.clone();
    let turn = convo.next_turn;
    let parts = guard(|| core.brains.agent_parts(&Slot::Conductor)).and_then(|r| r.map_err(|e| e.to_string()));
    let (provider, info, effort) = match parts {
        Ok(p) => p,
        Err(e) => {
            core.bus.publish(Event::Error { session: Some(session), message: e });
            return;
        }
    };
    let model = info.model_ref();
    // A relay turn runs with nobody waiting on it (at start, at 3am): it never loads a model.
    let who = if input.relay { Priority::Background } else { Priority::Foreground };
    if let Err(message) = admit_conductor(core, &session, &model, cancel, who) {
        if input.relay {
            // The run stays due; the user's next turn delivers it on a loaded model.
            eprintln!("marlowe: a crew result waits for the next turn: {message}");
            crate::crew::defer(core, &input.text);
        } else {
            core.bus.publish(Event::Error { session: Some(session), message });
        }
        return;
    }
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
    let mut env = Environment::current(workspace.clone(), info.display.clone());
    env.repo = workspace.as_deref().and_then(|w| repo_context(core, w));
    let system = system.user(core.user()).environment(env);
    let mut agent = Agent::new(provider, info, tools.clone(), system);
    agent.effort = effort;
    agent.memory = match &core.mind {
        Some(mind) => {
            let budget = agent.context.effective_window() / MEMORY_TIER_DIVISOR;
            let memory = MindMemory::new(mind.clone(), workspace, turn, budget, publisher(core));
            Arc::new(if input.relay { memory.relay() } else { memory })
        }
        None => Arc::new(NoMemory),
    };
    agent.approvals = Arc::new(Gate { core: core.clone(), cancel: cancel.clone() });
    let window = agent.context.window_tokens;

    core.bus.publish(Event::TurnStarted { session: session.clone(), turn, model, user: text.to_string() });
    let before_len = convo.messages.len();
    let before_summary = convo.summary.clone();
    // Crew results folded in as steers during this turn; delivered once the turn is saved.
    let mut steered: Vec<String> = Vec::new();
    let mut sink = |ev: AgentEvent| {
        if let AgentEvent::SteerApplied { text } = &ev {
            if is_relay(text) {
                steered.push(text.clone());
            }
        }
        if let Some(e) = to_event(&session, turn, window, ev) {
            core.bus.publish(e);
        }
    };
    let outcome = guard(|| agent.run_turn(convo, text, cancel, &handle.inbox_rx, &mut sink)).unwrap_or_else(|e| TurnOutcome {
        stop: TurnStop::Error,
        usage: Usage::default(),
        steps: 0,
        text: String::new(),
        error: Some(format!("the turn failed: {e}")),
        truncated: false,
    });
    convo.next_turn = convo.next_turn.max(turn + 1);

    let mut fresh = new_messages(convo, before_len, before_summary.as_deref(), text);
    if fresh.is_empty() {
        // Nothing was appended (the turn failed early): keep the user's words anyway.
        let m = Message::user(text);
        convo.push(m.clone());
        fresh.push(m);
    }
    match core.store.append(&session, fresh, convo.next_turn, convo.exposure.tainted, now_ms()) {
        // The crew results this turn carried are in the session's history now: a restart must
        // not relay them again.
        Ok(_) => {
            for relay in input.relay.then_some(&input.text).into_iter().chain(&steered) {
                crate::crew::delivered(core, relay);
            }
        }
        Err(e) => {
            core.bus.publish(Event::Error { session: Some(session.clone()), message: format!("turn not saved: {e}") });
        }
    }
    if let Some(message) = outcome.error.clone().or_else(|| (outcome.stop == TurnStop::Error).then(|| "the turn failed".into())) {
        core.bus.publish(Event::Error { session: Some(session.clone()), message });
    }
    core.bus.publish(Event::TurnFinished { session, turn, usage: outcome.usage, stop: outcome.stop });
}

/// Messages appended during the turn. If history was compacted mid-turn the prefix shifted, so
/// fall back to everything from the turn's user message (or all that remains).
fn new_messages(convo: &Conversation, before_len: usize, before_summary: Option<&str>, user_text: &str) -> Vec<Message> {
    if convo.summary.as_deref() == before_summary && convo.messages.len() >= before_len {
        return convo.messages[before_len..].to_vec();
    }
    let user = Message::user(user_text);
    match convo.messages.iter().rposition(|m| *m == user) {
        Some(i) => convo.messages[i..].to_vec(),
        None => std::iter::once(user).chain(convo.messages.iter().cloned()).collect(),
    }
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
        AgentEvent::SteerApplied { .. } => return None,
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
fn admit_conductor(core: &Arc<Core>, session: &SessionId, model: &ModelRef, cancel: &CancelToken, who: Priority) -> Result<(), String> {
    let admission = guard(|| core.brains.admit(model, who))?;
    let cost = match admission {
        Admission::Hosted | Admission::Resident => return Ok(()),
        Admission::Load(cost) => {
            eprintln!("marlowe: loading {model} for the conductor: {}", cost.describe());
            return Ok(());
        }
        Admission::Refused(why) | Admission::Wait(why) => return Err(format!("not loading {model}: {why}")),
        // Only the user's own turn asks, and only it counts loads approved earlier.
        Admission::Ask(cost) if who == Priority::Foreground => cost,
        Admission::Ask(cost) => return Err(format!("not loading {model}: {}", cost.describe())),
    };
    let key = format!("{model}|{}", cost.resident.join(","));
    if lock(&core.approved_loads).contains(&key) {
        return Ok(());
    }
    let gate = Gate { core: core.clone(), cancel: cancel.clone() };
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
        Ok(())
    } else {
        Err(format!("not loading {model}: {}", cost.describe()))
    }
}

struct Gate {
    core: Arc<Core>,
    cancel: CancelToken,
}

impl ApprovalGate for Gate {
    fn ask(&self, req: ApprovalReq) -> ApprovalDecision {
        if self.core.is_shutting_down() || self.cancel.is_cancelled() {
            return ApprovalDecision::Deny;
        }
        let (id, tx, rx) = self.core.approvals.register();
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
        });
        let decision = rx.recv_timeout(self.core.cfg.approval_timeout).unwrap_or(ApprovalDecision::Deny);
        self.core.approvals.remove(id);
        self.core.bus.publish(Event::ApprovalResolved { approval: id, decision });
        decision
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
    fn new_messages_survive_compaction() {
        let mut c = Conversation::new(SessionId("s".into()), None);
        c.push(Message::user("old"));
        c.push(Message::assistant("old reply"));
        let before = c.messages.len();
        c.push(Message::user("new"));
        c.push(Message::assistant("new reply"));
        assert_eq!(new_messages(&c, before, None, "new").len(), 2);
        // Compaction dropped the prefix and set a summary.
        c.messages.drain(..2);
        c.summary = Some("sum".into());
        let got = new_messages(&c, before, None, "new");
        assert_eq!(got, vec![Message::user("new"), Message::assistant("new reply")]);
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
