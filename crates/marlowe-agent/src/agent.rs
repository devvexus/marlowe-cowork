//! The agent and its turn loop.
//!
//! Besides the model, tools and middleware, a turn reads three things the conductor's session
//! keeps (design 3.4): the project sections in `Conversation::ext` (instructions, skills index,
//! output style; fitted to the prefix budget for the model's window), the [`Controls`] service
//! (`/compact`, `/add-dir`, `/memory` queued by the daemon, applied at the next step boundary)
//! and the [`ModelSwitch`] service (`/model` mid-session: the next call uses the new model, the
//! history is kept). A reply stuck repeating itself is cut ([`guard`]).

pub mod guard;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use marlowe_model::{
    CancelToken, ChatRequest, Completion, CompletionBuilder, Effort, Message, ModelInfo, Provider, ProviderError,
    StopReason, StreamEvent, ToolCall,
};

use crate::approval::{ApprovalGate, DenyAll};
use crate::budget::{Budget, Spent};
use crate::commands::CommandGrant;
use crate::context::{self, ContextConfig, PrefixBudget, TailReminders};
use crate::convo::Conversation;
use crate::events::{AgentEvent, TurnOutcome};
use crate::memory::{MemoryHook, NoMemory};
use crate::middleware::{FinalFlow, ModelCall, StepInfo, ToolMiddleware, TurnFlow, TurnObserver};
use crate::run_tools;
use crate::steer::Steer;
use crate::subagent::{ModelResolver, ResolvedModel};
use crate::system::{Environment, PromptSections, SystemPrompt};
use crate::tools::{Services, ToolHost};
use crate::TurnStop;

/// Model calls per turn before the loop stops with `TurnStop::StepLimit` (design 3: real
/// refactors take hundreds of steps; tokens, cost and wall-clock bound a turn too).
pub const MAX_STEPS: u32 = 400;

/// Waits before retrying a retriable provider error that arrived before any output.
const RETRY_BACKOFF: [Duration; 2] = [Duration::from_millis(500), Duration::from_millis(2_000)];

/// A session control the daemon queues for the loop's next step boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Control {
    /// `/compact [focus]`: summarise now.
    Compact(Option<String>),
    /// `/add-dir`: another workspace root (its instructions and rules load).
    AddDir(PathBuf),
    /// `/memory`: re-read the instruction files.
    ReloadInstructions,
}

/// The session's control queue (a service; the daemon pushes, the loop drains).
#[derive(Clone, Default)]
pub struct Controls(Arc<Mutex<Vec<Control>>>);

impl Controls {
    pub fn push(&self, c: Control) {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).push(c);
    }

    pub fn drain(&self) -> Vec<Control> {
        std::mem::take(&mut *self.0.lock().unwrap_or_else(|p| p.into_inner()))
    }
}

/// The session's model when the user switched it (`/model`); `None` keeps the agent's own.
#[derive(Clone)]
pub struct ModelSwitch(pub Arc<dyn Fn() -> Option<ResolvedModel> + Send + Sync>);

/// The conductor's context policy (design 3.4), installed by the daemon: per-turn memory and
/// environment ride on the turn's user message and history masks only at compaction, so every
/// request extends the previous one byte for byte (the provider's prefix cache holds).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextPolicy {
    pub masking: context::Masking,
    pub volatile_tail: bool,
}

impl ContextPolicy {
    /// Append-only history with the volatile tail.
    pub const CACHE_STABLE: ContextPolicy = ContextPolicy { masking: context::Masking::AppendOnly, volatile_tail: true };
}

/// A reply cut for repeating itself this many times in one turn ends the turn.
const MAX_LOOP_CUTS: u32 = 2;

/// Resolves a command's or skill's `model:` for the turn it runs in.
#[derive(Clone)]
pub struct Resolver(pub Arc<dyn ModelResolver>);

/// Everything one turn needs. Cheap to clone; one per session runner.
#[derive(Clone)]
pub struct Agent {
    pub provider: Arc<dyn Provider>,
    /// The model to call (its `id` goes in `ChatRequest::model`).
    pub model: ModelInfo,
    pub effort: Effort,
    pub tools: Arc<dyn ToolHost>,
    pub approvals: Arc<dyn ApprovalGate>,
    pub memory: Arc<dyn MemoryHook>,
    pub system: SystemPrompt,
    pub budget: Budget,
    /// Wall-clock limit for one turn (`None`: none).
    pub max_wall: Option<Duration>,
    pub context: ContextConfig,
    /// Around every turn, tool call and step, in order (see [`crate::middleware`]). `new` seeds
    /// it with the tool host's own ([`ToolHost::middleware`]); the daemon appends its per-track
    /// list (`marlowe-daemon/src/stack.rs`).
    pub middleware: Vec<Arc<dyn ToolMiddleware>>,
    /// Told about every model call and tool batch (F2's per-call turn rows). Empty by default.
    pub observers: Vec<Arc<dyn TurnObserver>>,
    /// Daemon services lent to every call through [`crate::CallCtx::scope`]. Empty by default.
    pub services: Services,
}

/// How one model call ended.
enum CallEnd {
    Done(Completion),
    /// Interrupted; carries the text streamed before the interrupt.
    Cancelled(String),
    /// Cut by the harness because the reply (or its reasoning) was repeating itself; carries one
    /// copy of the text.
    Looped(String),
    Failed(ProviderError),
}

/// The model a step calls.
struct Live {
    provider: Arc<dyn Provider>,
    model: ModelInfo,
    effort: Effort,
    context: ContextConfig,
}

impl Agent {
    /// An agent with no memory, denying approvals, default budget and context sizing, and the
    /// tool host's own middleware.
    pub fn new(provider: Arc<dyn Provider>, model: ModelInfo, tools: Arc<dyn ToolHost>, system: SystemPrompt) -> Self {
        let context = ContextConfig::for_model(&model);
        let middleware = tools.middleware();
        Self {
            provider,
            model,
            effort: Effort::default(),
            tools,
            approvals: Arc::new(DenyAll),
            memory: Arc::new(NoMemory),
            system,
            budget: Budget::default(),
            max_wall: None,
            context,
            middleware,
            observers: Vec::new(),
            services: Services::default(),
        }
    }

    /// The model for the next call: a `/model` switch, else a command's `model:` for this turn,
    /// else the agent's own.
    fn live(&self, turn_model: &Option<ResolvedModel>) -> Live {
        let switched = self.services.get::<ModelSwitch>().and_then(|s| (s.0)());
        let mut live = match switched.or_else(|| turn_model.clone()) {
            Some(m) => Live { context: self.context.for_switch(&m.info), provider: m.provider, model: m.info, effort: m.effort },
            None => Live { provider: self.provider.clone(), model: self.model.clone(), effort: self.effort, context: self.context },
        };
        if let Some(p) = self.services.get::<ContextPolicy>() {
            live.context.masking = p.masking;
            live.context.volatile_tail = p.volatile_tail;
        }
        live
    }

    /// Fits the session's project sections into the prefix budget for `live`'s window and puts
    /// them in `system`; the report goes in `Conversation::ext` for `/context`.
    fn apply_sections(&self, system: &mut SystemPrompt, convo: &mut Conversation, spec_tokens: u32, window: u32) {
        let Some(sections) = convo.ext.get::<PromptSections>().cloned() else { return };
        let persona = context::estimate_tokens(&system.persona_text());
        let fit = context::fit_prefix(persona, spec_tokens, &sections, PrefixBudget::for_window(window));
        system.set_sections(fit.sections.clone());
        convo.ext.insert(fit);
    }

    /// Runs one user turn to completion.
    ///
    /// Appends the user message, then loops: assemble -> stream -> append the assistant message
    /// (text kept alongside tool calls) -> run every tool call (independent calls in parallel,
    /// each gated by `consequence` and `approvals`, honouring `convo.session_allows`) -> append
    /// results -> drain `inbox` steers and queued [`Controls`] at the step boundary -> repeat.
    /// Stops when a message has no tool calls (and no middleware keeps the turn going), at
    /// `budget` or `max_wall`, or on `cancel`. Assigns ids to tool calls that lack one.
    ///
    /// Middleware's `before_turn` runs first: it may rewrite the text, or block the turn (then
    /// nothing is appended, no model is called, and the outcome is `blocked`). Memory always sees
    /// the user's own words, never the rewrite: a slash command's expansion (plugin text, pre-exec
    /// output, attached files) is not the user speaking.
    pub fn run_turn(
        &self,
        convo: &mut Conversation,
        user_text: &str,
        cancel: &CancelToken,
        inbox: &Receiver<Steer>,
        sink: &mut dyn FnMut(AgentEvent),
    ) -> TurnOutcome {
        let started = Instant::now();
        let mut text_in = user_text.to_string();
        for m in &self.middleware {
            if let TurnFlow::Block(reason) = m.before_turn(convo, &mut text_in) {
                return TurnOutcome {
                    stop: TurnStop::Error,
                    usage: Default::default(),
                    steps: 0,
                    text: String::new(),
                    error: Some(reason),
                    blocked: true,
                };
            }
        }
        let said = user_text;
        let user_text = text_in.as_str();
        let turn = convo.next_turn;
        convo.next_turn += 1;
        let user_index = convo.messages.len();
        convo.push(Message::user(user_text));
        convo.ext.insert(context::TurnQuery(user_index));

        let recollection = self.memory.before_turn(&convo.session, said);
        // Untrusted text entering the context taints the run before any tool runs.
        if recollection.untrusted {
            convo.exposure.tainted = true;
        }
        let profile = recollection.profile;
        let memory = recollection.blocks;
        let mut system = self.system.clone();
        let workspace = convo.workspace.as_ref().map(|p| p.display().to_string());
        // A command's `model:` applies to this turn only.
        let turn_model = convo
            .ext
            .get::<CommandGrant>()
            .and_then(|g| g.model.clone())
            .and_then(|m| self.services.get::<Resolver>().and_then(|r| r.0.resolve(Some(&m)).ok()));
        let mut live = self.live(&turn_model);
        // Fresh clock each turn; the caller's repo context is kept.
        let mut env = Environment::current(workspace, live.model.model_ref().to_string());
        env.repo = self.system.env().and_then(|e| e.repo.clone());
        system.set_environment(env);
        let specs = self.tools.specs();
        let spec_tokens = context::tool_spec_tokens(&specs);
        self.apply_sections(&mut system, convo, spec_tokens, live.context.window_tokens);
        if live.context.volatile_tail {
            let tail = context::tail_text(&system, &memory);
            if !tail.is_empty() {
                convo.ext.get_or_default::<TailReminders>().0.push((user_index, tail));
            }
        }

        let mut spent = Spent::default();
        let mut texts: Vec<String> = Vec::new();
        let mut error = None;
        let mut loop_cuts = 0u32;

        let stop = loop {
            if cancel.is_cancelled() {
                break TurnStop::Interrupted;
            }
            apply_steers(convo, inbox, sink);
            let mut forced_compact: Option<Option<String>> = None;
            if let Some(controls) = self.services.get::<Controls>() {
                let mut reload = false;
                for c in controls.drain() {
                    match c {
                        Control::Compact(focus) => forced_compact = Some(focus),
                        Control::AddDir(dir) => {
                            convo.ext.get_or_default::<crate::instructions::WorkspaceRoots>().add(&dir);
                            reload = true;
                        }
                        Control::ReloadInstructions => reload = true,
                    }
                }
                if reload && convo.ext.get::<crate::instructions::InstructionState>().is_some() {
                    crate::instructions::refresh(convo);
                    self.apply_sections(&mut system, convo, spec_tokens, live.context.window_tokens);
                }
            }
            if let Some(dim) = self.budget.exhausted(&spent) {
                break if dim == "steps" { TurnStop::StepLimit } else { TurnStop::Budget };
            }
            if self.max_wall.is_some_and(|w| started.elapsed() >= w) {
                break TurnStop::Budget;
            }
            // A `/model` switch takes effect at the step boundary; the history is kept.
            let next = self.live(&turn_model);
            if next.model.id != live.model.id || next.model.provider != live.model.provider {
                live = next;
                self.apply_sections(&mut system, convo, spec_tokens, live.context.window_tokens);
            }
            let cfg = live.context;

            let mut asm = context::assemble_with(&system, &profile, &memory, convo, &cfg);
            if forced_compact.is_some() || context::needs_compaction(asm.tokens + spec_tokens, &cfg) {
                let focus = forced_compact.clone().flatten();
                for m in &self.middleware {
                    m.before_compact(convo, focus.as_deref());
                }
                // Append-only: mask every eligible result in one chunk first; summarise only if
                // that is not enough (or the user asked for a summary).
                if forced_compact.is_none() && cfg.masking == context::Masking::AppendOnly {
                    let before = asm.tokens;
                    let pinned = crate::todo::pinned_ids(convo);
                    if context::mask_chunk(&mut convo.messages, cfg.keep_tool_results, &pinned) > 0 {
                        asm = context::assemble_with(&system, &profile, &memory, convo, &cfg);
                        sink(AgentEvent::Compacted { before_tokens: before, after_tokens: asm.tokens });
                    }
                }
                if forced_compact.is_some() || context::needs_compaction(asm.tokens + spec_tokens, &cfg) {
                    let pinned = crate::todo::pin_for_compaction(convo);
                    let r = context::compact_with(convo, live.provider.as_ref(), &live.model.id, &cfg, cancel, focus.as_deref(), &pinned);
                    match r {
                        // Whatever the sizes, history changed when a summary was written: the
                        // request is assembled again from it.
                        Ok((before, after)) => {
                            if after < before {
                                sink(AgentEvent::Compacted { before_tokens: before, after_tokens: after });
                            }
                            asm = context::assemble_with(&system, &profile, &memory, convo, &cfg);
                        }
                        Err(e) if e.is_cancelled() || cancel.is_cancelled() => break TurnStop::Interrupted,
                        // A failed summary leaves history as it was; the call below may still fit.
                        Err(_) => {}
                    }
                }
            }

            let max_output_tokens = live.model.max_output.map(|_| cfg.reserve_output);
            let req = ChatRequest {
                model: live.model.id.clone(),
                effort: live.effort,
                max_output_tokens,
                system: asm.system,
                messages: asm.messages,
                tools: specs.clone(),
                temperature: None,
            };
            let call_started = Instant::now();
            let mut c = match call_model(live.provider.as_ref(), &req, cancel, sink) {
                CallEnd::Done(c) => c,
                CallEnd::Cancelled(partial) => {
                    spent.steps += 1;
                    if !partial.is_empty() {
                        convo.push(Message::assistant(partial.clone()));
                        texts.push(partial);
                    }
                    break TurnStop::Interrupted;
                }
                CallEnd::Looped(kept) => {
                    spent.steps += 1;
                    loop_cuts += 1;
                    if !kept.trim().is_empty() {
                        convo.push(Message::assistant(kept.clone()));
                        texts.push(kept);
                    }
                    if loop_cuts > MAX_LOOP_CUTS {
                        error = Some("the model kept repeating itself".into());
                        break TurnStop::Error;
                    }
                    convo.push(Message::user(guard::REPEATING));
                    continue;
                }
                CallEnd::Failed(e) => {
                    spent.steps += 1;
                    error = Some(e.to_string());
                    break TurnStop::Error;
                }
            };
            spent.record(&c.usage);
            sink(AgentEvent::Usage(c.usage));

            let interrupted = cancel.is_cancelled() || c.stop == StopReason::Cancelled;
            if interrupted {
                c.tool_calls.clear();
            }
            assign_ids(&mut c.tool_calls, turn, spent.steps);
            let info = StepInfo { turn, step: spent.steps };
            if !self.observers.is_empty() {
                let call = ModelCall {
                    info,
                    request: &req,
                    completion: &c,
                    latency_ms: call_started.elapsed().as_millis() as u64,
                };
                for o in &self.observers {
                    o.on_model_call(convo, &call);
                }
            }
            if !c.text.is_empty() {
                texts.push(c.text.clone());
            }
            if !c.text.is_empty() || !c.tool_calls.is_empty() {
                convo.push(c.to_message());
            }
            if interrupted {
                break TurnStop::Interrupted;
            }
            if c.stop == StopReason::Error && c.tool_calls.is_empty() {
                error = Some("the model stream ended with an error".to_string());
                break TurnStop::Error;
            }
            if c.tool_calls.is_empty() {
                // A steer that arrived during the final answer gets its own reply.
                if !inbox.is_empty() {
                    continue;
                }
                // Middleware may keep the turn going (a Stop hook, open todos, the stop check);
                // the budget check at the top of the loop still bounds it.
                if let Some(reminder) = self.before_final(info, convo, &c.text) {
                    convo.push(Message::user(reminder));
                    continue;
                }
                break TurnStop::Done;
            }

            // A session in a worktree or with added roots runs its calls on tools rooted there.
            let host = crate::worktree::step_host(&self.tools, &self.services, convo);
            let results = run_tools::run_calls(
                host.as_ref(),
                &self.middleware,
                &self.services,
                self.approvals.as_ref(),
                &c.tool_calls,
                convo,
                turn,
                cancel,
                sink,
            );
            // A subagent that read untrusted content taints its caller through its report.
            if convo.ext.remove::<crate::subagent::ReportTainted>() {
                convo.exposure.tainted = true;
            }
            for o in &self.observers {
                o.on_tools_done(convo, info, &c.tool_calls, &results);
            }
            convo.push(Message::tool_results(results));
            for m in &self.middleware {
                for extra in m.after_step(info, convo) {
                    convo.push(extra);
                }
            }
        };

        // A command's grant lasts for its turn.
        convo.ext.remove::<CommandGrant>();
        let text = texts.join("\n\n");
        if stop != TurnStop::Error {
            self.memory.after_turn(&convo.session, said, &text);
        }
        TurnOutcome { stop, usage: spent.usage, steps: spent.steps, text, error, blocked: false }
    }

    /// The first middleware that wants the turn to continue, and its reminder.
    fn before_final(&self, info: StepInfo, convo: &mut Conversation, text: &str) -> Option<String> {
        self.middleware.iter().find_map(|m| match m.before_final(info, convo, text) {
            FinalFlow::Accept => None,
            FinalFlow::Continue(reminder) => Some(reminder),
        })
    }
}

/// One model call, forwarding deltas to `sink`. Retries a retriable error only when nothing
/// was streamed yet, so the UI never sees duplicated text. A reply or reasoning that starts
/// repeating itself is cut there ([`guard::RepetitionWatch`]).
fn call_model(provider: &dyn Provider, req: &ChatRequest, cancel: &CancelToken, sink: &mut dyn FnMut(AgentEvent)) -> CallEnd {
    let mut attempt = 0;
    loop {
        let mut partial = CompletionBuilder::new();
        let mut emitted = false;
        // The call's own token: the turn's cancel reaches it, and so does a loop cut.
        let call_cancel = CancelToken::new();
        let _link = {
            let c = call_cancel.clone();
            cancel.on_cancel(move || c.cancel())
        };
        let (mut text, mut reasoning) = (String::new(), String::new());
        let (mut text_watch, mut reasoning_watch) = (guard::RepetitionWatch::default(), guard::RepetitionWatch::for_reasoning());
        let mut looped: Option<String> = None;
        let result = provider.stream(req, &call_cancel, &mut |ev| {
            if looped.is_some() {
                return;
            }
            match &ev {
                StreamEvent::TextDelta(t) => {
                    text.push_str(t);
                    if text_watch.is_looping(&text) {
                        looped = Some(guard::trim_repetition(&text));
                        call_cancel.cancel();
                    }
                }
                StreamEvent::ReasoningDelta { text: t, .. } => {
                    reasoning.push_str(t);
                    if reasoning_watch.is_looping(&reasoning) {
                        looped = Some(text.clone());
                        call_cancel.cancel();
                    }
                }
                _ => {}
            }
            partial.push(&ev);
            if let Some(ae) = forward(ev) {
                emitted = true;
                sink(ae);
            }
        });
        if let Some(kept) = looped.filter(|_| !cancel.is_cancelled()) {
            return CallEnd::Looped(kept);
        }
        match result {
            Ok(c) => return CallEnd::Done(c),
            Err(e) if e.is_cancelled() || cancel.is_cancelled() => {
                return CallEnd::Cancelled(partial.finish().text);
            }
            Err(e) if e.retriable && !emitted && attempt < RETRY_BACKOFF.len() => {
                if !sleep_unless_cancelled(RETRY_BACKOFF[attempt], cancel) {
                    return CallEnd::Cancelled(String::new());
                }
                attempt += 1;
            }
            Err(e) => return CallEnd::Failed(e),
        }
    }
}

/// Maps provider stream events onto agent events. Tool-call deltas are not forwarded; the loop
/// reports tools when it runs them.
fn forward(ev: StreamEvent) -> Option<AgentEvent> {
    match ev {
        StreamEvent::TextDelta(t) if !t.is_empty() => Some(AgentEvent::TextDelta(t)),
        StreamEvent::ReasoningDelta { text, tokens } => Some(AgentEvent::ReasoningDelta { text, tokens }),
        StreamEvent::Retract { tokens } => Some(AgentEvent::TextRetracted { tokens }),
        _ => None,
    }
}

/// Folds every queued steer into the conversation; returns how many were applied.
fn apply_steers(convo: &mut Conversation, inbox: &Receiver<Steer>, sink: &mut dyn FnMut(AgentEvent)) -> usize {
    let mut n = 0;
    while let Ok(s) = inbox.try_recv() {
        convo.push(s.to_message());
        sink(AgentEvent::SteerApplied { text: s.text });
        n += 1;
    }
    n
}

/// Gives every call a non-empty id, unique within the message.
fn assign_ids(calls: &mut [ToolCall], turn: u32, step: u32) {
    let mut seen = std::collections::HashSet::new();
    for (i, c) in calls.iter_mut().enumerate() {
        if c.id.trim().is_empty() || !seen.insert(c.id.clone()) {
            c.id = format!("call_{turn}_{step}_{i}");
            seen.insert(c.id.clone());
        }
    }
}

/// Sleeps for `d` in short slices; false if cancelled meanwhile.
fn sleep_unless_cancelled(d: Duration, cancel: &CancelToken) -> bool {
    let end = Instant::now() + d;
    while Instant::now() < end {
        if cancel.is_cancelled() {
            return false;
        }
        std::thread::sleep(Duration::from_millis(25).min(end.saturating_duration_since(Instant::now())));
    }
    !cancel.is_cancelled()
}
