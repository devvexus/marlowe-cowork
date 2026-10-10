//! Extension points around the loop, so later tracks plug in without editing the loop's files.
//!
//! - [`ToolMiddleware`]: around every turn, tool call and step. Registered on
//!   [`crate::Agent::middleware`] in order (the first to short-circuit wins): the tool host's own
//!   ([`crate::ToolHost::middleware`], seeded by `Agent::new`) and the daemon's per-track list
//!   (`marlowe-daemon/src/stack.rs`). Run by `run_tools.rs` (calls) and `agent.rs` (turns, steps,
//!   compaction, the final answer).
//!   - E: hooks. UserPromptSubmit -> `before_turn`, PreToolUse -> `before_approval`, PostToolUse
//!     -> `after_call`, Stop -> `before_final` (SubagentStop: `before_final` on a subagent's
//!     `Agent`), PreCompact -> `before_compact`. dont_ask/plan-mode denials in `before_approval`.
//!   - A: `FileState` read-before-edit and stale checks in `before_approval`/`pre_mutation`, the
//!     external-change watcher's reminder in `after_step`.
//!   - B: job exit / `until_regex` notices in `after_step`.
//!   - C: post-edit diagnostics appended in `after_call` (settle window).
//!   - F: todo and verify reminders in `after_step`/`before_final`; F2: checkpoints in
//!     `pre_mutation`.
//! - [`TurnObserver`]: watches sessions, model calls and tool batches. F2 appends a `turns` row
//!   per model call (`model, provider, usage, latency_ms`) and per tool batch; I checkpoints crew
//!   steps; E's SessionStart and Notification hooks are `on_session_start` / `on_notification`.
//! - [`Extensions`]: a typed map on [`crate::Conversation::ext`] where each track keeps its own
//!   per-session state (A's `FileState` table, F's todos and pinned working set, E's session
//!   rules) without adding fields to `Conversation`. Tool calls and call-level middleware reach it
//!   through [`crate::CallCtx::scope`] (lent for the batch).

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::fmt;

use marlowe_model::{ChatRequest, Completion, Message, ToolCall, ToolResult};
use marlowe_proto::SessionId;

use crate::convo::Conversation;
use crate::tools::{CallCtx, Mutation, ToolOutcome};

/// What `before_turn` decides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnFlow {
    /// Run the turn (with the text as the middleware left it).
    Continue,
    /// Do not run it: no model call, and the prompt is not kept. The reason is shown to the user.
    Block(String),
}

/// What `before_approval` and `before_call` decide.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallFlow {
    /// Run the call.
    Continue,
    /// Do not run it; this is its result (a hook's block, a stale-file refusal, a mode denial).
    Done(ToolOutcome),
}

/// What `before_final` decides when the model produced a message with no tool calls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinalFlow {
    /// End the turn.
    Accept,
    /// Keep going: this text is appended as a user-role reminder and the loop takes another step
    /// (a Stop hook forcing continuation, open todos, "you edited files and ran no gate").
    Continue(String),
}

/// One step's facts, for `after_step` and observers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StepInfo {
    pub turn: u32,
    /// Model calls made in this turn so far (1 after the first).
    pub step: u32,
}

/// Around every turn, tool call and step. `before_turn`, `before_approval`, `after_step`,
/// `before_final` and `before_compact` run on the turn thread; `before_call`, `pre_mutation` and
/// `after_call` run on tool threads, possibly in parallel. Every method has a no-op default.
pub trait ToolMiddleware: Send + Sync {
    /// A short name for logs and `/hooks`.
    fn name(&self) -> &str;

    /// At the start of a turn, before the user's message is appended or any model is called.
    /// May rewrite `text` (add context) or block the turn.
    fn before_turn(&self, _convo: &mut Conversation, _text: &mut String) -> TurnFlow {
        TurnFlow::Continue
    }

    /// Before the approval gate decides on a call, in call order. May rewrite `call.args` (a
    /// hook's `updatedInput`): the gate, taint and egress then judge the rewritten call. Or it
    /// answers for the tool (a hook's deny, a mode denial, a stale-file refusal), and the call is
    /// neither asked about nor run. Changes to `call.name` or `call.id` are ignored.
    fn before_approval(&self, _call: &mut ToolCall, _cx: &CallCtx) -> CallFlow {
        CallFlow::Continue
    }

    /// After a call passed approval, right before it runs. Cannot change the call (it was
    /// approved as it is); may answer for it.
    fn before_call(&self, _call: &ToolCall, _cx: &CallCtx) -> CallFlow {
        CallFlow::Continue
    }

    /// Right before a call that will change files runs (after `before_call`), with what it will
    /// change. `Err` refuses the call with that outcome. Snapshot here (checkpoints, pre-images).
    fn pre_mutation(&self, _call: &ToolCall, _cx: &CallCtx, _mutation: &Mutation) -> Result<(), ToolOutcome> {
        Ok(())
    }

    /// After a call ran (not for denied or interrupted calls). May amend the outcome
    /// (diagnostics, a hook's `additionalContext`).
    fn after_call(&self, _call: &ToolCall, _cx: &CallCtx, _outcome: &mut ToolOutcome) {}

    /// After a step's tool results were appended. Returns messages to append before the next
    /// model call (reminders, notices); they go in as user-role text.
    fn after_step(&self, _info: StepInfo, _convo: &mut Conversation) -> Vec<Message> {
        Vec::new()
    }

    /// When the model answered with no tool calls.
    fn before_final(&self, _info: StepInfo, _convo: &mut Conversation, _text: &str) -> FinalFlow {
        FinalFlow::Accept
    }

    /// Before history is compacted: automatically when the context is full (`focus` is `None`),
    /// or on `/compact [focus]` (F's `conductor_ops` calls it).
    fn before_compact(&self, _convo: &mut Conversation, _focus: Option<&str>) {}
}

/// One finished model call, for observers.
#[derive(Debug, Clone, Copy)]
pub struct ModelCall<'a> {
    pub info: StepInfo,
    pub request: &'a ChatRequest,
    pub completion: &'a Completion,
    /// Wall-clock from request to the last streamed event.
    pub latency_ms: u64,
}

/// Why the harness wants the user's attention (E's Notification hook matchers).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotificationKind {
    /// An approval is waiting.
    PermissionPrompt,
    /// `ask_user` is waiting for an answer.
    Question,
    /// The session has been idle waiting for input.
    IdlePrompt,
}

/// One notification, for observers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    pub kind: NotificationKind,
    /// One line, e.g. the approval summary.
    pub message: String,
}

/// Watches a session. Called on the session's threads; must not block for long (F2 hands rows to
/// the store actor).
pub trait TurnObserver: Send + Sync {
    /// When a session's runner starts (a new session, or one loaded from the store).
    fn on_session_start(&self, _convo: &Conversation) {}

    /// When the harness asks for the user's attention (an approval, a question). Called from the
    /// thread that asks, while a turn may be running, so it gets the session id, not the
    /// conversation.
    fn on_notification(&self, _session: &SessionId, _note: &Notification) {}

    /// After each model call that returned a completion, with call ids assigned (a call that
    /// failed or was cut off by an interrupt before completing is not reported).
    fn on_model_call(&self, _convo: &Conversation, _call: &ModelCall<'_>) {}

    /// After a step's tool batch finished, with its calls and results in call order.
    fn on_tools_done(&self, _convo: &Conversation, _info: StepInfo, _calls: &[ToolCall], _results: &[ToolResult]) {}
}

/// A value kept in [`Extensions`]: any `Clone + Debug + Send + Sync` type.
pub trait ExtValue: Any + Send + Sync + fmt::Debug {
    fn clone_box(&self) -> Box<dyn ExtValue>;
    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
    fn into_any(self: Box<Self>) -> Box<dyn Any>;
}

impl<T: Any + Clone + Send + Sync + fmt::Debug> ExtValue for T {
    fn clone_box(&self) -> Box<dyn ExtValue> {
        Box::new(self.clone())
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}

/// A typed map: at most one value per type. Cloned with the conversation; not persisted (a track
/// that needs persistence stores its own rows).
#[derive(Default)]
pub struct Extensions {
    map: HashMap<TypeId, Box<dyn ExtValue>>,
}

impl Extensions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get<T: ExtValue>(&self) -> Option<&T> {
        self.map.get(&TypeId::of::<T>()).and_then(|b| b.as_ref().as_any().downcast_ref())
    }

    pub fn get_mut<T: ExtValue>(&mut self) -> Option<&mut T> {
        self.map.get_mut(&TypeId::of::<T>()).and_then(|b| b.as_mut().as_any_mut().downcast_mut())
    }

    /// The value of type `T`, inserting `T::default()` first if there is none.
    pub fn get_or_default<T: ExtValue + Default>(&mut self) -> &mut T {
        self.map.entry(TypeId::of::<T>()).or_insert_with(|| Box::new(T::default()));
        self.get_mut::<T>().expect("just inserted")
    }

    /// Stores `value`, returning the previous one of the same type.
    pub fn insert<T: ExtValue>(&mut self, value: T) -> Option<T> {
        let old = self.map.insert(TypeId::of::<T>(), Box::new(value))?;
        old.into_any().downcast::<T>().ok().map(|b| *b)
    }

    pub fn remove<T: ExtValue>(&mut self) -> bool {
        self.map.remove(&TypeId::of::<T>()).is_some()
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

impl Clone for Extensions {
    fn clone(&self) -> Self {
        Self { map: self.map.iter().map(|(k, v)| (*k, v.as_ref().clone_box())).collect() }
    }
}

impl fmt::Debug for Extensions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.map.values()).finish()
    }
}
