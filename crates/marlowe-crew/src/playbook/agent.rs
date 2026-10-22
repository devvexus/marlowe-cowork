//! The agent-backed [`StepRunner`]. Each playbook step is one `marlowe_agent::Agent` turn built
//! from the step's role card: the card's instructions as the system core, the card's effort, its
//! tool allowlist (narrowed by the step, never widened, never `dispatch`) over the run's sandboxed
//! tool host, and the model its slot resolves to. Independent steps run on scoped threads, at most
//! the provider's concurrency limit at once.
//!
//! What surrounds a call — checkpoints, the run budget, pause/cancel, live events, the step
//! record — belongs to the supervisor and comes in through [`StepHooks`].

use std::io::Read;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use marlowe_agent::{
    Agent, AgentEvent, ApprovalGate, Budget, Conversation, DenyAll, ModelCall, Steer, StepInfo, SystemPrompt, ToolHost,
    TurnObserver, TurnStop,
};
use marlowe_model::{
    CancelToken, ChatRequest, Completion, Message, ModelInfo, ModelRef, Provider, ProviderError, ProviderKind, Role, StreamEvent, ToolCall,
    ToolResult, Usage,
};
use marlowe_proto::{RunId, SandboxKind, SessionId};

use super::{ShellResult, StepResult, StepRunner, StepSpec};
use crate::role::{RoleCard, Roles};
use crate::sandbox::Sandbox;
use crate::tools::{CrewToolFactory, CrewTools};
use crate::CrewError;

/// Continuation turns a step gets after stopping at its per-turn step or budget limit.
pub const MAX_CONTINUES: u32 = 3;

/// The prompt of a continuation turn.
pub const CONTINUE_PROMPT: &str =
    "You stopped at a step limit. Continue from where you stopped and finish the task; end with your answer.";

/// The prompt a step resumed from its per-call checkpoints continues with.
pub const RESUME_PROMPT: &str = "The run restarted while you were on this step. Your work so far is above and nothing in it \
    will run again. A tool call marked as interrupted never reported back, so check the state it may have changed before \
    you repeat it. Continue the task and end with your answer.";

/// The result a tool call gets on resume when the run stopped before it reported back.
pub const INTERRUPTED_CALL: &str =
    "interrupted: the run stopped before this call reported back. It may or may not have taken effect; re-check the state before repeating it.";

/// A shell command's wall-clock limit.
pub const SHELL_TIMEOUT: Duration = Duration::from_secs(20 * 60);

/// Characters of combined output a shell result keeps (the tail).
pub const SHELL_OUTPUT_CHARS: usize = 16_000;

/// The model a role's step runs on.
#[derive(Clone)]
pub struct StepModel {
    pub provider: Arc<dyn Provider>,
    pub model: ModelInfo,
}

/// Resolves a role card to its model: the card's slot chain, with the GPU fallback applied.
pub type ModelResolver = Arc<dyn Fn(&RoleCard) -> Result<StepModel, CrewError> + Send + Sync>;

/// Resolves a model a workflow's `agent(model=...)` names for `card`: the model when it is in the
/// library or Ollama's tags and may run beside what is loaded, else why not.
pub type NamedModelFn = Arc<dyn Fn(&RoleCard, &str) -> Result<StepModel, String> + Send + Sync>;

/// Runs a command in the sandbox. The default is [`sandbox_shell`].
pub type ShellFn = Arc<dyn Fn(&Sandbox, &str, &CancelToken) -> Result<ShellResult, CrewError> + Send + Sync>;

/// Tokens kept free in the window beside a no-tool step's prompt and reply (estimates are rough).
pub const OUTPUT_MARGIN: u32 = 1_024;
/// The smallest reply cap a step is given, however long its prompt.
pub const MIN_OUTPUT: u32 = 1_024;

/// A step that gave no answer: its spend and how its last call stopped.
#[derive(Debug, Clone, PartialEq)]
pub struct StepFailure {
    pub why: String,
    pub usage: Usage,
    /// The last call stopped at the output limit (usually: all of it went to thinking).
    pub truncated: bool,
    /// The reply cap the step ran with, when it set one.
    pub max_output: Option<u32>,
}

/// The reply cap for a step asking for `want` tokens: at most the room the prompt leaves in a
/// `window`-token context, never under [`MIN_OUTPUT`].
pub fn output_cap(want: u32, window: u32, prompt_tokens: u32) -> u32 {
    let room = window.saturating_sub(prompt_tokens).saturating_sub(OUTPUT_MARGIN);
    want.min(room).max(MIN_OUTPUT)
}

/// An unfinished step as its per-call checkpoints left it: the conversation, the compaction state
/// that lives outside its messages, and whether it had read untrusted content.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PartialStep {
    pub messages: Vec<Message>,
    /// `Conversation::summary` after the last compaction.
    pub summary: Option<String>,
    /// The pinned working set kept beside the summary ([`marlowe_agent::context::PinnedSnapshot`]).
    pub pinned: Option<String>,
    pub tainted: bool,
}

/// One per-call checkpoint of a running step.
#[derive(Debug, Clone, Copy)]
pub struct CallCheckpoint<'a> {
    pub messages: &'a [Message],
    /// `messages` is the whole conversation (compaction rewrote it), not what one call added.
    pub full: bool,
    /// The compaction summary and pinned working set (stored with `full` rows).
    pub summary: Option<&'a str>,
    pub pinned: Option<&'a str>,
    /// The conversation has read untrusted content.
    pub tainted: bool,
}

/// What the supervisor wraps around each agent call. Every method has a do-nothing default.
pub trait StepHooks: Send + Sync {
    /// The stored result of a completed step `key`; a resumed run skips the call.
    fn completed(&self, _key: &str) -> Option<StepResult> {
        None
    }

    /// Called before every agent call, and again before a continuation turn. `Err` stops the
    /// run (cancelled, paused, budget extension refused); it may block (pause, budget approval).
    fn admit(&self, _spec: &StepSpec) -> Result<(), CrewError> {
        Ok(())
    }

    /// The per-turn limits for the next call: what is left of the run budget.
    fn remaining(&self) -> Budget {
        Budget::default()
    }

    /// Before every model call of a step (`local`: the model, when it runs on this machine). It
    /// may block (a budget approval, a local-model slot); `Err` ends the step with that error.
    fn before_call(&self, _spec: &StepSpec, _local: Option<&ModelRef>) -> Result<(), CrewError> {
        Ok(())
    }

    /// After every call `before_call` admitted; `usage` is `None` when the call did not finish.
    fn after_call(&self, _spec: &StepSpec, _local: Option<&ModelRef>, _usage: Option<&Usage>) {}

    /// The run has read untrusted content before this step.
    fn tainted(&self) -> bool {
        false
    }

    /// Live events of a running step.
    fn event(&self, _spec: &StepSpec, _ev: &AgentEvent) {}

    /// A step finished: its whole conversation and its result. The checkpoint goes here.
    fn finished(&self, _spec: &StepSpec, _messages: &[Message], _result: &StepResult) {}

    /// A step ended without an answer: what it spent and why, for the record. Nothing is
    /// checkpointed, so a resumed run asks again.
    fn failed(&self, _spec: &StepSpec, _messages: &[Message], _failure: &StepFailure) {}

    /// What an unfinished step `key` had reached, from its per-call checkpoints (design 3.7). A
    /// resumed step continues from it instead of starting over; empty when there is none.
    fn partial(&self, _key: &str) -> PartialStep {
        PartialStep::default()
    }

    /// A per-call checkpoint: the messages one model call or one tool batch added to the step's
    /// conversation, or (`full`) the whole conversation and its compaction state after
    /// compaction rewrote its history.
    fn checkpoint_call(&self, _spec: &StepSpec, _checkpoint: &CallCheckpoint<'_>) {}

    /// The run's one-line current action.
    fn progress(&self, _action: &str) {}

    /// An agent did not run on the model its script named; kept with the run and shown.
    fn model_note(&self, _note: &str) {}
}

/// Hooks that do nothing: no checkpoints, no budget, no events.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoHooks;

impl StepHooks for NoHooks {}

/// Wraps a step's tool host before the agent sees it (a workflow agent's `structured_output`).
pub type StepToolsFn = Arc<dyn Fn(&StepSpec, Arc<dyn ToolHost>) -> Arc<dyn ToolHost> + Send + Sync>;

/// A [`StepRunner`] that runs every step as an agent turn. Fields are public so the supervisor
/// can set what it owns; [`AgentRunner::new`] fills the rest with safe defaults. Cloning shares
/// everything (a workflow gives each agent a clone with its own cancel token).
#[derive(Clone)]
pub struct AgentRunner {
    pub run: RunId,
    /// The cards the run started with.
    pub roles: Roles,
    pub models: ModelResolver,
    /// The run's inner tool host (from the `CrewToolFactory`), rooted in the sandbox.
    pub tools: Arc<dyn ToolHost>,
    /// Builds a tool host for a step that runs in a sandbox of its own ([`StepSpec::sandbox`]).
    /// Without one, such a step fails rather than run with tools rooted somewhere else.
    pub tool_factory: Option<CrewToolFactory>,
    pub sandbox: Sandbox,
    /// The original workspace, read-only to the crew.
    pub workspace: Option<PathBuf>,
    pub approvals: Arc<dyn ApprovalGate>,
    pub cancel: CancelToken,
    /// Steers for the run; whichever running step reaches a step boundary first takes them.
    pub inbox: Receiver<Steer>,
    pub hooks: Arc<dyn StepHooks>,
    pub shell: ShellFn,
    /// Concurrent steps per cloud provider.
    pub per_provider: usize,
    /// Concurrent steps on a local model: the crew's share of its slots.
    pub local_slots: Arc<dyn Fn(&ModelRef) -> usize + Send + Sync>,
    /// Why a local model runs fewer agents at once than planned, said plainly (the context
    /// planner's `slots_note`); `None` when it runs what it plans.
    pub slots_note: Arc<dyn Fn(&ModelRef) -> Option<String> + Send + Sync>,
    /// Wraps each step's tool host; `None` keeps it.
    pub step_tools: Option<StepToolsFn>,
    /// Finds the model a script's `agent(model=...)` names; `None` runs every agent on the
    /// role's model.
    pub named_model: Option<NamedModelFn>,
}

impl AgentRunner {
    /// A runner that denies approvals, has no hooks, no steers, and runs shells with
    /// [`sandbox_shell`]; limits 2 per cloud provider and 1 per local model.
    pub fn new(
        run: RunId,
        roles: Roles,
        models: ModelResolver,
        tools: Arc<dyn ToolHost>,
        sandbox: Sandbox,
        workspace: Option<PathBuf>,
    ) -> AgentRunner {
        AgentRunner {
            run,
            roles,
            models,
            tools,
            tool_factory: None,
            sandbox,
            workspace,
            approvals: Arc::new(DenyAll),
            cancel: CancelToken::new(),
            inbox: crossbeam_channel::never(),
            hooks: Arc::new(NoHooks),
            shell: Arc::new(sandbox_shell),
            per_provider: 2,
            local_slots: Arc::new(|_| 1),
            slots_note: Arc::new(|_| None),
            step_tools: None,
            named_model: None,
        }
    }

    /// The model `role` resolves to now.
    pub fn model_for(&self, role: &str) -> Result<StepModel, CrewError> {
        (self.models)(self.card(role)?)
    }

    /// One model call that turns an agent's final text into an object matching `schema`, with
    /// decoding constrained to it (`ChatRequest::format`) and no tools (WF3's finalisation for an
    /// agent that ended without a valid `structured_output` call). Metered like any step call.
    pub fn finalise(&self, spec: &StepSpec, answer: &str, schema: &serde_json::Value) -> Result<(serde_json::Value, Usage), CrewError> {
        let card = self.card(&spec.role)?;
        let model = (self.models)(card)?;
        let failed = Arc::new(Mutex::new(None));
        let metered = Metered {
            local: local_ref(&model),
            inner: model.provider,
            hooks: self.hooks.clone(),
            spec: spec.clone(),
            failed: failed.clone(),
        };
        let mut req = ChatRequest::new(
            model.model.id.clone(),
            vec![marlowe_model::Message::user(format!(
                "The agent's final report:\n\n{answer}\n\nReturn it as one JSON object matching the schema. Use only what the report says; \
                 an empty list where it found nothing."
            ))],
        );
        req.system = vec![marlowe_model::SystemBlock {
            text: "You convert an agent's final report into a single JSON object that matches the schema you are given. Output the JSON only.".into(),
            cache: false,
        }];
        req.format = Some(schema.clone());
        req.effort = marlowe_model::Effort::Low;
        req.max_output_tokens = Some(4_096);
        let c = metered.stream(&req, &self.cancel, &mut |_| {});
        if let Some(e) = failed.lock().unwrap_or_else(|e| e.into_inner()).take() {
            return Err(e);
        }
        let c = c.map_err(|e| CrewError::Step { step: spec.key.clone(), message: format!("the finalisation call failed: {e}") })?;
        let text = c.text.trim();
        let body = text.trim_start_matches("```json").trim_start_matches("```").trim_end_matches("```").trim();
        let value: serde_json::Value = serde_json::from_str(body)
            .map_err(|e| CrewError::Step { step: spec.key.clone(), message: format!("the finalisation call did not return JSON: {e}") })?;
        Ok((value, c.usage))
    }

    /// The sandbox a step runs in and the tool host rooted in it: the run's own unless the step
    /// names another.
    fn step_sandbox(&self, spec: &StepSpec) -> Result<(Sandbox, Arc<dyn ToolHost>), CrewError> {
        match &spec.sandbox {
            None => Ok((self.sandbox.clone(), self.tools.clone())),
            Some(sb) if *sb == self.sandbox => Ok((self.sandbox.clone(), self.tools.clone())),
            Some(sb) => match &self.tool_factory {
                Some(make) => Ok((sb.clone(), make(sb, self.workspace.as_deref()))),
                None => Err(CrewError::Step {
                    step: spec.key.clone(),
                    message: "the step names its own sandbox but the run has no tool factory to root its tools there".into(),
                }),
            },
        }
    }

    pub(crate) fn card(&self, role: &str) -> Result<&RoleCard, CrewError> {
        self.roles.get(role).ok_or_else(|| CrewError::UnknownRole(role.to_string()))
    }

    /// The card's allowlist, narrowed to `narrow` when the step gives one.
    fn allowlist(card: &RoleCard, narrow: Option<&[String]>) -> Vec<String> {
        card.tools
            .iter()
            .filter(|t| narrow.is_none_or(|n| n.iter().any(|x| x == *t)))
            .cloned()
            .collect()
    }

    /// The system core for one step: the card, then a few lines on where the step runs.
    fn system(&self, card: &RoleCard, spec: &StepSpec, sandbox: &Sandbox, allow: &[String]) -> SystemPrompt {
        let mut ctx = format!(
            "\n\n## This run\n- Crew run {}; you are the {}, on the step `{}`.\n",
            self.run.short(),
            card.name,
            spec.label
        );
        let root = sandbox.root.display();
        match sandbox.kind {
            SandboxKind::Worktree => ctx.push_str(&format!(
                "- Your git worktree (yours to change, branch `{}`): {root}\n",
                sandbox.branch.as_deref().unwrap_or("crew")
            )),
            SandboxKind::Scratch => ctx.push_str(&format!("- Your scratch directory (yours to write): {root}\n")),
            SandboxKind::None => ctx.push_str("- You have no writable directory on this step.\n"),
        }
        if let Some(ws) = &self.workspace {
            ctx.push_str(&format!("- The original workspace, read-only: {}\n", ws.display()));
        }
        if allow.is_empty() {
            ctx.push_str("- You have no tools on this step; answer from what you are given.\n");
        } else {
            ctx.push_str(&format!("- Tools: {}\n", allow.join(", ")));
        }
        SystemPrompt::with_core(format!("{}{ctx}", card.instructions.trim_end()))
    }

    /// The concurrency limit for a batch: the tightest limit among the providers it uses.
    fn batch_limit(&self, specs: &[StepSpec]) -> usize {
        let mut limit = usize::MAX;
        for spec in specs {
            let Some(card) = self.roles.get(&spec.role) else { continue };
            let Ok(m) = (self.models)(card) else { continue };
            limit = limit.min(match local_ref(&m) {
                Some(model) => (self.local_slots)(&model),
                None => self.per_provider,
            });
        }
        if limit == usize::MAX {
            self.per_provider.max(1)
        } else {
            limit.max(1)
        }
    }

    fn run_step(&self, spec: &StepSpec) -> Result<StepResult, CrewError> {
        if let Some(done) = self.hooks.completed(&spec.key) {
            return Ok(done);
        }
        let card = self.card(&spec.role)?;
        let allow = Self::allowlist(card, spec.tools.as_deref());
        let model = (self.models)(card)?;
        let (sandbox, inner_tools) = self.step_sandbox(spec)?;
        // The run's taint is read live: a message that reaches the run's inbox mid-step taints
        // the shells that follow it, not only the next step's.
        let hooks = self.hooks.clone();
        let tools: Arc<dyn ToolHost> =
            Arc::new(CrewTools::new(inner_tools, allow.clone(), sandbox.clone()).with_live_taint(Arc::new(move || hooks.tainted())));
        let tools = match &self.step_tools {
            Some(wrap) => wrap(spec, tools),
            None => tools,
        };
        let failed = Arc::new(Mutex::new(None));
        let metered = Metered {
            local: local_ref(&model),
            inner: model.provider,
            hooks: self.hooks.clone(),
            spec: spec.clone(),
            failed: failed.clone(),
        };
        let system = self.system(card, spec, &sandbox, &allow);
        let prompt_tokens = marlowe_agent::context::estimate_tokens(&system.render_text())
            + marlowe_agent::context::estimate_tokens(&spec.prompt);
        let mut agent = Agent::new(Arc::new(metered), model.model, tools.clone(), system);
        agent.effort = spec.effort.unwrap_or(card.effort);
        agent.approvals = self.approvals.clone();
        agent.middleware.push(marlowe_agent::agent::guard::LoopGuard::for_crew());
        // A sized step gets the room its prompt leaves, not the provider's default (Ollama's is
        // a quarter of the window, which a thinking model can spend before it answers).
        if let Some(want) = spec.max_output {
            let cap = output_cap(want, agent.context.window_tokens, prompt_tokens);
            agent.max_output = Some(cap);
            agent.context.reserve_output = cap;
        }

        let session = SessionId::from(format!("crew:{}", self.run).as_str());
        let mut convo = Conversation::new(session, Some(sandbox.root.clone()));
        convo.exposure.tainted = self.hooks.tainted();
        // A step the run stopped in continues from its per-call checkpoints: completed model
        // calls and tool batches are never repeated.
        let seeded = self.hooks.partial(&spec.key);
        let resumed = !seeded.messages.is_empty();
        let stored = seeded.messages.len();
        if resumed {
            convo.next_turn = seeded.messages.iter().filter(|m| m.role == Role::User).count() as u32;
            convo.messages = close_dangling(seeded.messages);
            convo.summary = seeded.summary.clone();
            if let Some(p) = seeded.pinned {
                convo.ext.insert(marlowe_agent::context::PinnedSnapshot(p));
            }
            // What the step read before the run stopped still counts: from its checkpoints, and
            // from the calls in the history it resumes with.
            convo.exposure.tainted |= seeded.tainted;
            convo.recompute_exposure(tools.as_ref());
        }
        agent.observers.push(Arc::new(CallRecorder {
            hooks: self.hooks.clone(),
            spec: spec.clone(),
            state: Mutex::new(RecorderState { cursor: stored, summary: convo.summary.clone() }),
        }));

        self.hooks.progress(&spec.label);
        let mut usage = Usage::default();
        let mut prompt = if resumed { RESUME_PROMPT.to_string() } else { spec.prompt.clone() };
        let mut finished = false;
        let mut truncated = false;
        // Reasoning tokens of the model call in flight: a reply of reasoning alone leaves nothing in
        // the history, so this is the only record of it.
        let reasoned = std::cell::Cell::new(0u64);
        for _ in 0..=MAX_CONTINUES {
            self.hooks.admit(spec)?;
            if self.cancel.is_cancelled() {
                return Err(CrewError::Cancelled);
            }
            agent.budget = self.hooks.remaining();
            let out = agent.run_turn(&mut convo, &prompt, &self.cancel, &self.inbox, &mut |ev| {
                match &ev {
                    AgentEvent::CallStarted { .. } => reasoned.set(0),
                    AgentEvent::ReasoningDelta { tokens, .. } => reasoned.set(reasoned.get() + tokens),
                    _ => {}
                }
                self.hooks.event(spec, &ev)
            });
            usage.add(&out.usage);
            // A call the hooks refused (budget refused, paused, cancelled) ends the step with why.
            if let Some(e) = failed.lock().unwrap_or_else(|e| e.into_inner()).take() {
                return Err(e);
            }
            truncated = out.truncated;
            match out.stop {
                TurnStop::Done => {
                    finished = true;
                    break;
                }
                TurnStop::Interrupted => return Err(CrewError::Cancelled),
                TurnStop::Error => {
                    return Err(CrewError::Step {
                        step: spec.key.clone(),
                        message: out.error.unwrap_or_else(|| "the model call failed".into()),
                    })
                }
                TurnStop::StepLimit | TurnStop::Budget => prompt = CONTINUE_PROMPT.to_string(),
            }
        }
        let text = last_assistant_text(&convo.messages);
        if !finished || text.trim().is_empty() {
            let why = if !finished {
                "the step did not finish".to_string()
            } else {
                match reasoned.get() {
                    0 => "the agent gave no answer".to_string(),
                    n => format!("stopped with reasoning only ({n} tokens) after a nudge"),
                }
            };
            let failure = StepFailure { why: why.clone(), usage, truncated, max_output: agent.max_output };
            self.hooks.failed(spec, &convo.messages, &failure);
            return Err(CrewError::Step { step: spec.key.clone(), message: why });
        }
        let result = StepResult { text, usage, tainted: convo.exposure.tainted, truncated };
        self.hooks.finished(spec, &convo.messages, &result);
        Ok(result)
    }
}

/// A stored conversation made well-formed for the model: tool calls of the last assistant
/// message that never got results get [`INTERRUPTED_CALL`] (they are not run again).
pub fn close_dangling(mut messages: Vec<Message>) -> Vec<Message> {
    let Some(last) = messages.last() else { return messages };
    if last.role != Role::Assistant {
        return messages;
    }
    let calls: Vec<String> = last.tool_calls().map(|c| c.id.clone()).collect();
    if calls.is_empty() {
        return messages;
    }
    messages.push(Message::tool_results(calls.into_iter().map(|call_id| ToolResult {
        call_id,
        content: INTERRUPTED_CALL.into(),
        is_error: true,
        parts: Vec::new(),
    })));
    messages
}

/// Writes a step's conversation as the loop produces it: one checkpoint per model call (its
/// answer, with whatever was appended since the last write) and one per tool batch (its results).
/// A run killed mid-step loses at most the model call in flight.
struct CallRecorder {
    hooks: Arc<dyn StepHooks>,
    spec: StepSpec,
    state: Mutex<RecorderState>,
}

struct RecorderState {
    /// Messages of the conversation below this are stored.
    cursor: usize,
    /// The compaction summary as last stored; a change means compaction rewrote the history.
    summary: Option<String>,
}

impl CallRecorder {
    /// Stores what the conversation gained since the last write, plus `next` (the message the
    /// loop pushes right after the observer returns). After a compaction (the summary changed)
    /// it stores the whole conversation with the summary and the pinned working set instead.
    fn write(&self, convo: &Conversation, next: Option<Message>) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let len = convo.messages.len();
        let full = convo.summary != st.summary || st.cursor > len;
        let mut messages: Vec<Message> = convo.messages[if full { 0 } else { st.cursor }..].to_vec();
        let pushed = next.is_some();
        messages.extend(next);
        if !messages.is_empty() || full {
            let pinned = convo.ext.get::<marlowe_agent::context::PinnedSnapshot>().map(|p| p.0.as_str());
            self.hooks.checkpoint_call(
                &self.spec,
                &CallCheckpoint {
                    messages: &messages,
                    full,
                    summary: convo.summary.as_deref(),
                    pinned,
                    tainted: convo.exposure.tainted,
                },
            );
        }
        st.cursor = len + usize::from(pushed);
        st.summary = convo.summary.clone();
    }
}

impl TurnObserver for CallRecorder {
    fn on_model_call(&self, convo: &Conversation, call: &ModelCall<'_>) {
        let c = call.completion;
        // The loop keeps an answer only when it has text or tool calls.
        let kept = !c.text.is_empty() || !c.tool_calls.is_empty();
        self.write(convo, kept.then(|| c.to_message()));
    }

    fn on_tools_done(&self, convo: &Conversation, _info: StepInfo, _calls: &[ToolCall], results: &[ToolResult]) {
        self.write(convo, Some(Message::tool_results(results.iter().cloned())));
    }
}

/// The text of the last assistant message that has any.
fn last_assistant_text(messages: &[Message]) -> String {
    messages
        .iter()
        .rev()
        .filter(|m| m.role == Role::Assistant)
        .map(Message::joined_text)
        .find(|t| !t.trim().is_empty())
        .unwrap_or_default()
}

/// The step's provider with the hooks around every model call (compaction included).
struct Metered {
    inner: Arc<dyn Provider>,
    hooks: Arc<dyn StepHooks>,
    spec: StepSpec,
    local: Option<ModelRef>,
    /// Why `before_call` refused, for `run_step` to return.
    failed: Arc<Mutex<Option<CrewError>>>,
}

/// The model when it runs on this machine: the key of its stream slots.
fn local_ref(m: &StepModel) -> Option<ModelRef> {
    (m.model.local || m.provider.kind().is_local()).then(|| m.model.model_ref())
}

/// Calls `after_call` once, also when the provider panics.
struct AfterCall<'a> {
    m: &'a Metered,
    usage: Option<Usage>,
}

impl Drop for AfterCall<'_> {
    fn drop(&mut self) {
        self.m.hooks.after_call(&self.m.spec, self.m.local.as_ref(), self.usage.as_ref());
    }
}

impl Provider for Metered {
    fn kind(&self) -> ProviderKind {
        self.inner.kind()
    }

    fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        self.inner.list_models()
    }

    fn stream(&self, req: &ChatRequest, cancel: &CancelToken, on: &mut dyn FnMut(StreamEvent)) -> Result<Completion, ProviderError> {
        if let Err(e) = self.hooks.before_call(&self.spec, self.local.as_ref()) {
            *self.failed.lock().unwrap_or_else(|e| e.into_inner()) = Some(e);
            return Err(ProviderError::cancelled());
        }
        let mut after = AfterCall { m: self, usage: None };
        let result = self.inner.stream(req, cancel, on);
        after.usage = result.as_ref().ok().map(|c| c.usage);
        result
    }
}

impl StepRunner for AgentRunner {
    fn step(&self, spec: StepSpec) -> Result<StepResult, CrewError> {
        self.run_step(&spec)
    }

    fn completed(&self, key: &str) -> Option<StepResult> {
        self.hooks.completed(key)
    }

    fn parallel(&self, specs: Vec<StepSpec>) -> Vec<Result<StepResult, CrewError>> {
        let n = specs.len();
        let limit = self.batch_limit(&specs).min(n.max(1));
        let slots: Vec<Mutex<Option<Result<StepResult, CrewError>>>> = (0..n).map(|_| Mutex::new(None)).collect();
        let next = AtomicUsize::new(0);
        std::thread::scope(|s| {
            for _ in 0..limit {
                s.spawn(|| loop {
                    let i = next.fetch_add(1, Ordering::SeqCst);
                    if i >= n {
                        break;
                    }
                    let spec = &specs[i];
                    let r = catch_unwind(AssertUnwindSafe(|| self.run_step(spec))).unwrap_or_else(|_| {
                        Err(CrewError::Step { step: spec.key.clone(), message: "the step panicked".into() })
                    });
                    *slots[i].lock().unwrap_or_else(|e| e.into_inner()) = Some(r);
                });
            }
        });
        slots
            .into_iter()
            .zip(&specs)
            .map(|(m, spec)| {
                m.into_inner().unwrap_or_else(|e| e.into_inner()).unwrap_or_else(|| {
                    Err(CrewError::Step { step: spec.key.clone(), message: "the step never ran".into() })
                })
            })
            .collect()
    }

    fn shell(&self, command: &str) -> Result<ShellResult, CrewError> {
        if self.cancel.is_cancelled() {
            return Err(CrewError::Cancelled);
        }
        (self.shell)(&self.sandbox, command, &self.cancel)
    }

    fn shell_in(&self, sandbox: &Sandbox, command: &str) -> Result<ShellResult, CrewError> {
        if self.cancel.is_cancelled() {
            return Err(CrewError::Cancelled);
        }
        (self.shell)(sandbox, command, &self.cancel)
    }

    fn sandbox(&self) -> &Sandbox {
        &self.sandbox
    }

    fn progress(&self, action: &str) {
        self.hooks.progress(action);
    }
}

// ---------------------------------------------------------------- shell

/// A process started for a crew shell: in a kill-on-close job where the OS has jobs.
enum Proc {
    Job(marlowe_os::JobChild),
    Plain(Child),
}

impl Proc {
    fn child(&mut self) -> &mut Child {
        match self {
            Proc::Job(j) => j.child_mut(),
            Proc::Plain(c) => c,
        }
    }

    fn kill_tree(&mut self) {
        match self {
            Proc::Job(j) => {
                let _ = j.kill_tree();
            }
            Proc::Plain(c) => {
                let _ = c.kill();
                let _ = c.wait();
            }
        }
    }
}

/// `cmd /C` on Windows, `sh -c` elsewhere.
fn shell_command(command: &str) -> Command {
    if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(command);
        c
    } else {
        let mut c = Command::new("sh");
        c.arg("-c").arg(command);
        c
    }
}

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
#[cfg(not(windows))]
const CREATE_NO_WINDOW: u32 = 0;

/// Runs `command` with cwd = the sandbox root, API keys scrubbed, the sandbox's no-push git
/// environment, in a kill-on-close Job Object. Output is the bounded tail of stdout + stderr.
/// Cancel kills the tree and returns `Cancelled`; [`SHELL_TIMEOUT`] kills it and fails.
pub fn sandbox_shell(sandbox: &Sandbox, command: &str, cancel: &CancelToken) -> Result<ShellResult, CrewError> {
    let mut cmd = shell_command(command);
    cmd.current_dir(&sandbox.root).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    for k in marlowe_brains::scrubbed_env() {
        cmd.env_remove(k);
    }
    for (k, v) in sandbox.child_env() {
        cmd.env(k, v);
    }
    let mut proc = match marlowe_os::spawn_in_job_with_flags(&mut cmd, CREATE_NO_WINDOW) {
        Ok(j) => Proc::Job(j),
        Err(marlowe_os::OsError::Unsupported(_)) => Proc::Plain(cmd.spawn()?),
        Err(marlowe_os::OsError::Io(e)) => return Err(CrewError::Io(e)),
        Err(e) => return Err(CrewError::Io(std::io::Error::other(e.to_string()))),
    };
    let readers = [
        proc.child().stdout.take().map(|r| read_tail(Box::new(r))),
        proc.child().stderr.take().map(|r| read_tail(Box::new(r))),
    ];
    let start = Instant::now();
    let status = loop {
        if cancel.is_cancelled() {
            proc.kill_tree();
            return Err(CrewError::Cancelled);
        }
        if start.elapsed() > SHELL_TIMEOUT {
            proc.kill_tree();
            break None;
        }
        match proc.child().try_wait()? {
            Some(s) => break Some(s),
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    };
    // Background processes the command left behind die with the job, closing the pipes.
    proc.kill_tree();
    let mut output = String::new();
    for r in readers.into_iter().flatten() {
        let bytes = r.join().unwrap_or_default();
        let text = String::from_utf8_lossy(&bytes);
        if !text.trim().is_empty() {
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str(text.trim_end());
        }
    }
    let mut output = tail_chars(&output, SHELL_OUTPUT_CHARS);
    Ok(match status {
        Some(s) => ShellResult { ok: s.success(), exit_code: s.code(), output },
        None => {
            output.push_str(&format!("\n[killed after {} minutes]", SHELL_TIMEOUT.as_secs() / 60));
            ShellResult { ok: false, exit_code: None, output }
        }
    })
}

/// Reads a pipe to its end on a thread, keeping at most the last 256 KiB.
fn read_tail(mut r: Box<dyn Read + Send>) -> std::thread::JoinHandle<Vec<u8>> {
    const KEEP: usize = 256 * 1024;
    std::thread::spawn(move || {
        let mut out = Vec::new();
        let mut buf = [0u8; 8192];
        while let Ok(n) = r.read(&mut buf) {
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
            if out.len() > 2 * KEEP {
                out.drain(..out.len() - KEEP);
            }
        }
        out
    })
}

/// The last `max` characters of `text`, marked when cut.
pub fn tail_chars(text: &str, max: usize) -> String {
    let n = text.chars().count();
    if n <= max {
        return text.to_string();
    }
    let tail: String = text.chars().skip(n - max).collect();
    format!("[… {} earlier characters cut]\n{tail}", n - max)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A step stopped while its tool calls ran resumes with each call closed as interrupted (never
    /// run again); a stored result or a plain answer is left as it is.
    #[test]
    fn dangling_calls_close_as_interrupted() {
        let call = marlowe_model::ToolCall { id: "c1".into(), name: "bash".into(), args: serde_json::json!({"command": "make"}) };
        let mut asked = Message::assistant("running it");
        asked.content.push(marlowe_model::Part::ToolCall(call));
        let closed = close_dangling(vec![Message::user("go"), asked.clone()]);
        assert_eq!(closed.len(), 3);
        let r = closed[2].tool_results_iter().next().unwrap();
        assert_eq!((r.call_id.as_str(), r.content.as_str(), r.is_error), ("c1", INTERRUPTED_CALL, true));
        let done = vec![Message::user("go"), asked, closed[2].clone()];
        assert_eq!(close_dangling(done.clone()), done);
        assert_eq!(close_dangling(vec![Message::user("go")]).len(), 1);
    }

    /// dusk at 32k: a 9k-token prompt leaves room for a 22.7k reply, so a retry's doubled budget
    /// is honoured; a prompt that fills the window still gets a minimal reply.
    #[test]
    fn a_reply_cap_is_the_room_the_prompt_leaves() {
        assert_eq!(output_cap(12_288, 32_768, 9_000), 12_288);
        assert_eq!(output_cap(24_576, 32_768, 9_000), 32_768 - 9_000 - OUTPUT_MARGIN);
        assert_eq!(output_cap(12_288, 32_768, 31_500), MIN_OUTPUT);
    }
}
