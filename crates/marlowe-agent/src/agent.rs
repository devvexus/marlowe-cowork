//! The agent and its turn loop.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use marlowe_model::{
    CancelToken, ChatRequest, Completion, CompletionBuilder, Effort, Message, ModelInfo, Provider, ProviderError,
    StopReason, StreamEvent, ToolCall,
};

use crate::approval::{ApprovalGate, DenyAll};
use crate::budget::{Budget, Spent};
use crate::context::{self, ContextConfig};
use crate::convo::Conversation;
use crate::events::{AgentEvent, TurnOutcome};
use crate::memory::{MemoryHook, NoMemory};
use crate::run_tools;
use crate::steer::Steer;
use crate::system::{Environment, SystemPrompt};
use crate::tools::ToolHost;
use crate::TurnStop;

/// Model calls per turn before the loop stops with `TurnStop::StepLimit`.
pub const MAX_STEPS: u32 = 40;

/// Waits before retrying a retriable provider error that arrived before any output.
const RETRY_BACKOFF: [Duration; 2] = [Duration::from_millis(500), Duration::from_millis(2_000)];

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
    pub context: ContextConfig,
    /// Caps every reply of this agent (thinking included), overriding the catalog's
    /// `max_output`. A no-tool step sizes it to the room its prompt leaves in the window.
    pub max_output: Option<u32>,
}

/// How one model call ended.
enum CallEnd {
    Done(Completion),
    /// Interrupted; carries the text streamed before the interrupt.
    Cancelled(String),
    Failed(ProviderError),
}

impl Agent {
    /// An agent with no memory, denying approvals, default budget and context sizing.
    pub fn new(provider: Arc<dyn Provider>, model: ModelInfo, tools: Arc<dyn ToolHost>, system: SystemPrompt) -> Self {
        let context = ContextConfig::for_model(&model);
        Self {
            provider,
            model,
            effort: Effort::default(),
            tools,
            approvals: Arc::new(DenyAll),
            memory: Arc::new(NoMemory),
            system,
            budget: Budget::default(),
            context,
            max_output: None,
        }
    }

    /// Runs one user turn to completion.
    ///
    /// Appends the user message, then loops: assemble -> stream -> append the assistant message
    /// (text kept alongside tool calls) -> run every tool call (independent calls in parallel,
    /// each gated by `consequence` and `approvals`, honouring `convo.session_allows`) -> append
    /// results -> drain `inbox` steers at the step boundary -> repeat. Stops when a message has
    /// no tool calls, at `budget`, or on `cancel`. Assigns ids to tool calls that lack one.
    pub fn run_turn(
        &self,
        convo: &mut Conversation,
        user_text: &str,
        cancel: &CancelToken,
        inbox: &Receiver<Steer>,
        sink: &mut dyn FnMut(AgentEvent),
    ) -> TurnOutcome {
        let turn = convo.next_turn;
        convo.next_turn += 1;
        convo.push(Message::user(user_text));

        let recollection = self.memory.before_turn(&convo.session, user_text);
        // Untrusted text entering the context taints the run before any tool runs.
        if recollection.untrusted {
            convo.exposure.tainted = true;
        }
        let profile = recollection.profile;
        let memory = recollection.blocks;
        let mut system = self.system.clone();
        let workspace = convo.workspace.as_ref().map(|p| p.display().to_string());
        // Fresh clock each turn; the caller's repo context is kept.
        let mut env = Environment::current(workspace, self.model.model_ref().to_string());
        env.repo = self.system.env().and_then(|e| e.repo.clone());
        system.set_environment(env);
        let specs = self.tools.specs();
        let spec_tokens = context::tool_spec_tokens(&specs);
        let max_output_tokens = self.max_output.or(self.model.max_output.map(|_| self.context.reserve_output));

        let mut spent = Spent::default();
        let mut texts: Vec<String> = Vec::new();
        let mut error = None;
        let mut truncated = false;

        let stop = loop {
            if cancel.is_cancelled() {
                break TurnStop::Interrupted;
            }
            apply_steers(convo, inbox, sink);
            if let Some(dim) = self.budget.exhausted(&spent) {
                break if dim == "steps" { TurnStop::StepLimit } else { TurnStop::Budget };
            }

            let mut asm = context::assemble_with(&system, &profile, &memory, convo, &self.context);
            if context::needs_compaction(asm.tokens + spec_tokens, &self.context) {
                match context::compact(convo, self.provider.as_ref(), &self.model.id, &self.context, cancel) {
                    Ok((before, after)) if after < before => {
                        sink(AgentEvent::Compacted { before_tokens: before, after_tokens: after });
                        asm = context::assemble_with(&system, &profile, &memory, convo, &self.context);
                    }
                    Ok(_) => {}
                    Err(e) if e.is_cancelled() || cancel.is_cancelled() => break TurnStop::Interrupted,
                    // A failed summary leaves history as it was; the call below may still fit.
                    Err(_) => {}
                }
            }

            let req = ChatRequest {
                model: self.model.id.clone(),
                effort: self.effort,
                max_output_tokens,
                system: asm.system,
                messages: asm.messages,
                tools: specs.clone(),
                temperature: None,
            };
            let mut c = match self.call_model(&req, cancel, sink) {
                CallEnd::Done(c) => c,
                CallEnd::Cancelled(partial) => {
                    spent.steps += 1;
                    if !partial.is_empty() {
                        convo.push(Message::assistant(partial.clone()));
                        texts.push(partial);
                    }
                    break TurnStop::Interrupted;
                }
                CallEnd::Failed(e) => {
                    spent.steps += 1;
                    error = Some(e.to_string());
                    break TurnStop::Error;
                }
            };
            spent.record(&c.usage);
            sink(AgentEvent::Usage(c.usage));
            truncated = c.stop == StopReason::MaxTokens;

            let interrupted = cancel.is_cancelled() || c.stop == StopReason::Cancelled;
            if interrupted {
                c.tool_calls.clear();
            }
            assign_ids(&mut c.tool_calls, turn, spent.steps);
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
                break TurnStop::Done;
            }

            let results = run_tools::run_calls(
                self.tools.as_ref(),
                self.approvals.as_ref(),
                &c.tool_calls,
                convo,
                turn,
                cancel,
                sink,
            );
            convo.push(Message::tool_results(results));
        };

        let text = texts.join("\n\n");
        if stop != TurnStop::Error {
            self.memory.after_turn(&convo.session, user_text, &text);
        }
        TurnOutcome { stop, usage: spent.usage, steps: spent.steps, text, error, truncated }
    }

    /// One model call, forwarding deltas to `sink`. Retries a retriable error only when nothing
    /// was streamed yet, so the UI never sees duplicated text.
    fn call_model(&self, req: &ChatRequest, cancel: &CancelToken, sink: &mut dyn FnMut(AgentEvent)) -> CallEnd {
        let mut attempt = 0;
        loop {
            let mut partial = CompletionBuilder::new();
            let mut emitted = false;
            let result = self.provider.stream(req, cancel, &mut |ev| {
                partial.push(&ev);
                if let Some(ae) = forward(ev) {
                    emitted = true;
                    sink(ae);
                }
            });
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
