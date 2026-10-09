//! Session runners: one thread per active session, owning its `Conversation`.
//!
//! An idle session starts a turn on `Ask`. While a turn runs, further asks go to the steer inbox
//! (the loop folds them in at a step boundary); whatever is still unread when the turn ends runs
//! as the next turn. `Interrupt` cancels the turn and drops what was queued behind it.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use crossbeam_channel::{unbounded, Receiver, RecvTimeoutError, Sender};
use marlowe_agent::{
    Agent, AgentEvent, ApprovalGate, ApprovalReq, Conversation, Environment, NoMemory, Steer, SteerRefused,
    SystemPrompt, ToolHost, TurnOutcome, TurnStop,
};
use marlowe_model::{CancelToken, Message, Usage};
use marlowe_proto::{ApprovalDecision, Event, SessionId, Slot};

use crate::daemon::Core;
use crate::{lock, now_ms};

pub(crate) enum RunnerCmd {
    Turn(String),
    Stop,
}

struct RunState {
    busy: bool,
    /// The token of the running (or about-to-run) turn.
    cancel: CancelToken,
    /// Asks too long to be a steer; run after the current turn.
    queued: Vec<String>,
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
        let state = RunState { busy: false, cancel: CancelToken::new(), queued: Vec::new(), closed: false };
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
            return self.cmd_tx.send(RunnerCmd::Turn(text)).map_err(|_| Enqueue::Closed);
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

    /// Cancels the running turn and drops anything queued behind it.
    pub fn interrupt(&self) {
        let mut st = lock(&self.state);
        if st.busy {
            st.cancel.cancel();
        }
        st.queued.clear();
        while self.inbox_rx.try_recv().is_ok() {}
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
    // A session loaded from the store keeps the taint of what it read before.
    convo.recompute_exposure(tools.as_ref());
    loop {
        let text = match cmd_rx.recv_timeout(core.cfg.session_idle) {
            Ok(RunnerCmd::Turn(text)) => text,
            Ok(RunnerCmd::Stop) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) if core.evict_if_idle(handle) => break,
            Err(RecvTimeoutError::Timeout) => continue,
        };
        let mut next = Some(text);
        while let Some(text) = next.take() {
            if core.is_shutting_down() {
                break;
            }
            let cancel = lock(&handle.state).cancel.clone();
            run_one(core, handle, &mut convo, &tools, &text, &cancel);
            // Decide under the lock so an ask arriving now either lands in `left` or starts a
            // fresh turn through the command channel.
            let mut st = lock(&handle.state);
            let mut left: Vec<String> = handle.inbox_rx.try_iter().map(|s| s.text).collect();
            left.append(&mut st.queued);
            if !left.is_empty() && !core.is_shutting_down() {
                st.cancel = CancelToken::new();
                next = Some(left.join("\n\n"));
            } else {
                st.busy = false;
            }
        }
        if core.is_shutting_down() {
            break;
        }
    }
}

fn run_one(core: &Arc<Core>, handle: &SessionHandle, convo: &mut Conversation, tools: &Arc<dyn ToolHost>, text: &str, cancel: &CancelToken) {
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
    let workspace = convo.workspace.as_ref().map(|p| p.display().to_string());
    let system = SystemPrompt::new()
        .user(core.user.clone())
        .environment(Environment::current(workspace, info.display.clone()));
    let mut agent = Agent::new(provider, info, tools.clone(), system);
    agent.effort = effort;
    agent.memory = Arc::new(NoMemory);
    agent.approvals = Arc::new(Gate { core: core.clone(), cancel: cancel.clone() });

    core.bus.publish(Event::TurnStarted { session: session.clone(), turn, model, user: text.to_string() });
    let before_len = convo.messages.len();
    let before_summary = convo.summary.clone();
    let mut sink = |ev: AgentEvent| {
        if let Some(e) = to_event(&session, turn, ev) {
            core.bus.publish(e);
        }
    };
    let outcome = guard(|| agent.run_turn(convo, text, cancel, &handle.inbox_rx, &mut sink)).unwrap_or_else(|e| TurnOutcome {
        stop: TurnStop::Error,
        usage: Usage::default(),
        steps: 0,
        text: String::new(),
        error: Some(format!("the turn failed: {e}")),
    });
    convo.next_turn = convo.next_turn.max(turn + 1);

    let mut fresh = new_messages(convo, before_len, before_summary.as_deref(), text);
    if fresh.is_empty() {
        // Nothing was appended (the turn failed early): keep the user's words anyway.
        let m = Message::user(text);
        convo.push(m.clone());
        fresh.push(m);
    }
    if let Err(e) = core.store.append(&session, fresh, convo.next_turn, now_ms()) {
        core.bus.publish(Event::Error { session: Some(session.clone()), message: format!("turn not saved: {e}") });
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

fn to_event(session: &SessionId, turn: u32, ev: AgentEvent) -> Option<Event> {
    let session = session.clone();
    Some(match ev {
        AgentEvent::TextDelta(text) => Event::TextDelta { session, turn, text },
        AgentEvent::ReasoningDelta { text, tokens } => Event::ReasoningDelta { session, turn, text, tokens },
        AgentEvent::TextRetracted { tokens } => Event::TextRetracted { session, turn, tokens },
        AgentEvent::ToolStarted { call_id, tool, summary } => Event::ToolStarted { session, turn, call_id, tool, summary },
        AgentEvent::ToolFinished { call_id, ok, summary } => Event::ToolFinished { session, turn, call_id, ok, summary },
        AgentEvent::Usage(_) | AgentEvent::Compacted { .. } | AgentEvent::SteerApplied { .. } => return None,
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
        assert!(matches!(rx.try_recv(), Ok(RunnerCmd::Turn(t)) if t == "one"));
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
    fn only_an_idle_handle_closes_and_a_closed_one_refuses_asks() {
        let (h, _rx) = SessionHandle::new(SessionId("s".into()), String::new(), None);
        h.enqueue("one".into()).unwrap();
        assert!(!h.close_if_idle(), "a busy session stays open");
        lock(&h.state).busy = false;
        assert!(h.close_if_idle());
        assert_eq!(h.enqueue("two".into()), Err(Enqueue::Closed));
    }
}
