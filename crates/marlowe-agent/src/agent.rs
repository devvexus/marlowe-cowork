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
use marlowe_proto::{PhaseEvent, PhaseKind};
use marlowe_model::{
    CancelToken, ChatRequest, Completion, CompletionBuilder, Effort, Message, ModelInfo, Provider, ProviderError,
    Part, ProviderKind, Role, StopReason, StreamEvent, ToolCall, ToolResult, Usage,
};

use crate::approval::{ApprovalGate, DenyAll};
use crate::budget::{Budget, Spent};
use crate::commands::CommandGrant;
use crate::context::{self, ContextConfig, PrefixBudget, TailReminders};
use crate::convo::Conversation;
use crate::events::{AgentEvent, PhaseTimer, TurnOutcome};
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

/// Longest a streaming phase goes between two progress events.
const PROGRESS_EVERY: Duration = Duration::from_millis(8);

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
    /// Caps every reply of this agent (thinking included), overriding the catalog's
    /// `max_output`. A no-tool step sizes it to the room its prompt leaves in the window.
    pub max_output: Option<u32>,
    /// Whose conversation this agent carries: the daemon marks the user's own as the conductor's,
    /// so an engine with several slots keeps its prompt cache apart from the crew's.
    pub lane: marlowe_model::Lane,
}

/// How one model call ended.
enum CallEnd {
    Done(Completion),
    /// Interrupted; carries what the model had produced before the interrupt (reasoning and
    /// text), which goes into the history exactly as generated.
    Cancelled(Completion),
    /// Cut by the harness because the reply (or its reasoning) was repeating itself. `kept` is
    /// one copy of the text (what the turn reports); `made` is everything generated up to the
    /// cut, which is what the history keeps, so the model's cache is extended, not rewritten.
    Looped { kept: String, made: Completion },
    /// The call failed; the flag says whether any reply text had been shown.
    Failed(ProviderError, bool),
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
        // The forgiving layer judges every call first, so hooks, the gate, taint and egress see
        // the call as the tool will run it.
        let mut middleware: Vec<Arc<dyn ToolMiddleware>> = vec![Arc::new(crate::args::ArgsLayer::new(tools.clone()))];
        middleware.extend(tools.middleware());
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
            max_output: None,
            lane: marlowe_model::Lane::Other,
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
        let prompt = PhaseTimer::start(PhaseKind::Prompt, sink);
        let mut text_in = user_text.to_string();
        for m in &self.middleware {
            if let TurnFlow::Block(reason) = m.before_turn(convo, &mut text_in) {
                // Context an earlier middleware added for this turn must not reach the next one.
                convo.pending_context.clear();
                return TurnOutcome {
                    stop: TurnStop::Error,
                    usage: Default::default(),
                    steps: 0,
                    text: String::new(),
                    error: Some(reason),
                    blocked: true,
                    truncated: false,
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

        prompt.end(sink);
        let recall = PhaseTimer::start(PhaseKind::Recall, sink);
        let recollection = self.memory.before_turn(&convo.session, said);
        recall.end(sink);
        let mut prompt = Some(PhaseTimer::start(PhaseKind::Prompt, sink));
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
        // `ultrathink` in the user's message raises this turn's effort to the top (C14).
        let ultra = convo.ext.get::<crate::directive::TurnDirectives>().is_some_and(|d| d.ultrathink);
        let mut live = self.live(&turn_model);
        if ultra {
            live.effort = Effort::Max;
        }
        // Fresh clock each turn; the caller's repo context is kept.
        let mut env = Environment::current(workspace, live.model.model_ref().to_string());
        env.repo = self.system.env().and_then(|e| e.repo.clone());
        system.set_environment(env);
        // Teams mode: its reminder rides on this turn's user message, whatever the tail policy.
        if let Some(note) = self.services.get::<crate::teams::TeamsNote>().and_then(|t| t.note(said)) {
            convo.ext.get_or_default::<context::TurnNotes>().0.push((user_index, note));
        }
        // The tool list is fixed for the conversation (until it is compacted): a tool that
        // appears later is announced on this message and joins the list at the next compaction.
        let specs = crate::toolset::for_turn(convo, self.tools.specs(), user_index);
        let spec_tokens = context::tool_spec_tokens(&specs);
        self.apply_sections(&mut system, convo, spec_tokens, live.context.window_tokens);
        context::freeze_system(convo, &system, false);
        // With the tail on the user's message, nothing per-turn sits in the system prompt: the
        // profile rides there too, on the turns where it is new or changed (a memory write used to
        // rewrite the system prompt and with it every token the model had cached).
        let profile = if live.context.volatile_tail {
            let mut blocks: Vec<String> = context::profile_update(&profile, context::profile_shown(convo).as_deref()).into_iter().collect();
            blocks.extend(memory.iter().cloned());
            let tail = context::tail_text(&system, &blocks);
            if !tail.is_empty() {
                convo.ext.get_or_default::<TailReminders>().0.push((user_index, tail));
            }
            String::new()
        } else {
            profile
        };

        let mut spent = Spent::default();
        let mut texts: Vec<String> = Vec::new();
        let mut error = None;
        let mut truncated = false;
        let mut loop_cuts = 0u32;
        let mut malformed_retried = false;
        // History was rewritten on purpose since the last request (compaction logs itself).
        let mut compacted = false;

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
                            // The permission layer frees file calls inside it as in the workspace.
                            if let Some(p) = self.services.get::<crate::approval::Permissions>() {
                                p.add_root(dir.clone());
                            }
                            convo.ext.get_or_default::<crate::instructions::WorkspaceRoots>().add(&dir);
                            reload = true;
                        }
                        Control::ReloadInstructions => reload = true,
                    }
                }
                if reload && convo.ext.get::<crate::instructions::InstructionState>().is_some() {
                    crate::instructions::refresh(convo);
                    self.apply_sections(&mut system, convo, spec_tokens, live.context.window_tokens);
                    context::freeze_system(convo, &system, true);
                    // The user asked for the instructions to be read again: the one deliberate
                    // change to the system prompt, like a compaction.
                    compacted = true;
                }
            }
            if let Some(dim) = self.budget.exhausted(&spent) {
                break if dim == "steps" { TurnStop::StepLimit } else { TurnStop::Budget };
            }
            if self.max_wall.is_some_and(|w| started.elapsed() >= w) {
                break TurnStop::Budget;
            }
            // A `/model` switch takes effect at the step boundary; the history is kept.
            let mut next = self.live(&turn_model);
            if ultra {
                next.effort = Effort::Max;
            }
            if next.model.id != live.model.id || next.model.provider != live.model.provider {
                live = next;
                self.apply_sections(&mut system, convo, spec_tokens, live.context.window_tokens);
                context::freeze_system(convo, &system, true);
            }
            let cfg = live.context;

            // The prompt-side work since recall (system, sections, steers) ends where fitting begins.
            if let Some(p) = prompt.take() {
                p.end(sink);
            }
            let fit = PhaseTimer::start(PhaseKind::Fit, sink);
            let mut asm = context::assemble_with(&system, &profile, &memory, convo, &cfg);
            let compact_now =
                forced_compact.is_some() || context::needs_compaction(context::scaled_tokens(asm.tokens + spec_tokens, convo), &cfg);
            fit.end_with(sink, |e| e.unit("tok").with_done(u64::from(asm.tokens + spec_tokens)));
            if compact_now {
                let compacting = PhaseTimer::start(PhaseKind::Compact, sink);
                let before_all = asm.tokens;
                let mut failed = false;
                let focus = forced_compact.clone().flatten();
                for m in &self.middleware {
                    m.before_compact(convo, focus.as_deref());
                }
                // Append-only: mask every eligible result in one chunk first; summarise only if
                // that is not enough (or the user asked for a summary).
                if forced_compact.is_none() && cfg.masking == context::Masking::AppendOnly {
                    let before = asm.tokens;
                    let pinned = crate::todo::pinned_ids(convo);
                    let keep = context::keep_within(&convo.messages, cfg.keep_tool_results, cfg.window_tokens / 4);
                    if context::mask_chunk(&mut convo.messages, keep, &pinned) > 0 {
                        crate::todo::note_compacted(convo);
                        compacted = true;
                        asm = context::assemble_with(&system, &profile, &memory, convo, &cfg);
                        sink(AgentEvent::Compacted { before_tokens: before, after_tokens: asm.tokens });
                    }
                }
                if forced_compact.is_some()
                    || context::needs_compaction(context::scaled_tokens(asm.tokens + spec_tokens, convo), &cfg)
                {
                    let pinned = crate::todo::pin_for_compaction(convo);
                    // Progress counts the summary as it streams; the cap is an upper bound (est).
                    let mut last: Option<Instant> = None;
                    let mut on_progress = |written: u32, cap: u32| {
                        if last.is_none_or(|t| t.elapsed() >= PROGRESS_EVERY) {
                            last = Some(Instant::now());
                            compacting.progress(u64::from(written), u64::from(cap), "tok", true, sink);
                        }
                    };
                    let r = context::compact_observed(
                        convo,
                        live.provider.as_ref(),
                        &live.model.id,
                        &cfg,
                        cancel,
                        focus.as_deref(),
                        &pinned,
                        &mut on_progress,
                    );
                    match r {
                        // Whatever the sizes, history changed when a summary was written: the
                        // request is assembled again from it.
                        Ok((before, after)) => {
                            compacted = true;
                            if after < before {
                                sink(AgentEvent::Compacted { before_tokens: before, after_tokens: after });
                            }
                            asm = context::assemble_with(&system, &profile, &memory, convo, &cfg);
                            // PostCompact hooks (track E) inform; their output is not used.
                            if let Some(r) = self.services.get::<Arc<crate::hooks::HookRunner>>() {
                                let _ = r.post_compact(if focus.is_some() { "manual" } else { "auto" });
                            }
                        }
                        Err(e) if e.is_cancelled() || cancel.is_cancelled() => {
                            compacting.end_with(sink, |e| e.detail("cancelled"));
                            break TurnStop::Interrupted;
                        }
                        // A failed summary leaves history as it was; the call below may still fit.
                        Err(_) => failed = true,
                    }
                }
                let after_all = asm.tokens;
                compacting.end_with(sink, |e| {
                    let e = e.unit("tok").with_done(u64::from(before_all)).with_total(u64::from(after_all));
                    if failed { e.detail("failed") } else { e }
                });
            }

            let max_output_tokens = self.max_output.or(live.model.max_output.map(|_| {
                context::reply_room(context::scaled_tokens(asm.tokens + spec_tokens, convo), &cfg)
            }));
            let est_prompt_tokens = asm.tokens + spec_tokens;
            let req = ChatRequest {
                model: live.model.id.clone(),
                effort: live.effort,
                max_output_tokens,
                system: asm.system,
                messages: asm.messages,
                tools: specs.clone(),
                temperature: None,
                format: None,
                lane: self.lane,
            };
            // A local model keeps its cache only for an exact prefix: say which section broke it.
            let local = matches!(live.model.provider, ProviderKind::Ollama | ProviderKind::LocalGguf);
            let rewrote = std::mem::take(&mut compacted) | convo.ext.remove::<context::Rewritten>();
            if local {
                let session = convo.session.to_string();
                if let Some(b) = marlowe_model::prefix::check(&session, &req, rewrote) {
                    if b.diverged() {
                        violation(b.line(&session, &req.model));
                    } else {
                        // The prompt extends the last one; another request used the model's slot.
                        eprintln!("{}", b.line(&session, &req.model));
                    }
                }
            }
            for o in &self.observers {
                o.on_request(convo, &req);
            }
            let call_started = Instant::now();
            sink(AgentEvent::CallStarted { step: spent.steps + 1, est_prompt_tokens });
            let mut wire = Some(PhaseTimer::start(PhaseKind::Wire, sink));
            let ended = call_model(live.provider.as_ref(), &req, cancel, &mut wire, (call_started, est_prompt_tokens), sink);
            if local {
                let session = convo.session.to_string();
                marlowe_model::prefix::made(&session, &req.model);
                // The engine's own template rendering of this request, checked against the last one.
                for what in marlowe_model::prefix::take_render_breaks(&req.model, marlowe_model::prefix::conversation_key(&req)) {
                    if let Some(inherent) = what.strip_prefix(marlowe_model::prefix::TEMPLATE_DROP) {
                        // The model's own template prints less of the earlier reasoning than it did:
                        // no switch keeps it, so the turn that just ended is read again. Said, not a bug.
                        eprintln!("marlowe: note: {}'s chat template drops earlier reasoning from the prompt (session {session}): {inherent}", req.model);
                    } else if !rewrote {
                        violation(format!("marlowe: prompt cache break in session {session} on {}: {what}", req.model));
                    }
                }
            }
            // A call that never produced a frame (failed, cancelled) still closes its phase.
            if let Some(w) = wire.take() {
                w.end_with(sink, |e| e.detail("no frame"));
            }
            let mut c = match ended {
                CallEnd::Done(c) => c,
                CallEnd::Cancelled(mut made) => {
                    spent.steps += 1;
                    // A call cut mid-way is not a call: only the reasoning and text are kept.
                    made.tool_calls.clear();
                    if let Some(m) = made.kept_message() {
                        convo.push(m);
                    }
                    if !made.text.is_empty() {
                        texts.push(made.text);
                    }
                    break TurnStop::Interrupted;
                }
                CallEnd::Looped { kept, mut made } => {
                    spent.steps += 1;
                    loop_cuts += 1;
                    made.tool_calls.clear();
                    if let Some(m) = made.kept_message() {
                        convo.push(m);
                    }
                    if !kept.trim().is_empty() {
                        texts.push(kept);
                    }
                    if loop_cuts > MAX_LOOP_CUTS {
                        error = Some("the model kept repeating itself".into());
                        break TurnStop::Error;
                    }
                    convo.push(Message::user(guard::REPEATING));
                    continue;
                }
                CallEnd::Failed(e, saw_text) => {
                    spent.steps += 1;
                    // The engine could not parse the model's tool call: tell it once, in the tail,
                    // and let it send the call again (only when no reply text was shown).
                    if e.is_malformed_call() && !saw_text && !malformed_retried {
                        malformed_retried = true;
                        let why = e.detail.strip_prefix("malformed tool call: ").unwrap_or(&e.detail);
                        convo.push(Message::user(format!("Your last tool call was malformed ({why}); send it again, well-formed.")));
                        continue;
                    }
                    error = Some(e.to_string());
                    break TurnStop::Error;
                }
            };
            spent.record(&c.usage);
            // What the prompt really came to, against what was estimated, for the next check.
            if local {
                if let Some(scale) = context::TokenScale::learned(c.usage.input, est_prompt_tokens) {
                    convo.ext.insert(scale);
                }
            }
            // A `+500k` target counts every generated token of the request (WF8).
            if let Some(pool) = self.services.get::<crate::directive::TokenPool>() {
                pool.charge(c.usage.output);
            }
            sink(AgentEvent::Usage(c.usage));
            truncated = c.stop == StopReason::MaxTokens;

            let interrupted = cancel.is_cancelled() || c.stop == StopReason::Cancelled;
            assign_ids(&mut c.tool_calls, turn, spent.steps);
            // What the model produced, kept for the history and the stored row exactly as it
            // wrote it (names, argument keys and their order): the prompt it reads next must
            // start with the one it just read plus this, or a local model re-reads it all.
            let produced = c.clone();
            if interrupted {
                c.tool_calls.clear();
            }
            // Names and the arguments container become canonical before the transcript, hooks
            // and approvals see them (the forgiving layer, `args.rs`); they run the canonical call.
            for call in &mut c.tool_calls {
                crate::args::prepare_call(self.tools.as_ref(), call);
            }
            let info = StepInfo { turn, step: spent.steps };
            if !self.observers.is_empty() {
                let call = ModelCall {
                    info,
                    request: &req,
                    completion: &produced,
                    latency_ms: call_started.elapsed().as_millis() as u64,
                };
                for o in &self.observers {
                    o.on_model_call(convo, &call);
                }
            }
            if !c.text.is_empty() {
                texts.push(c.text.clone());
            }
            if let Some(m) = produced.kept_message() {
                convo.push(m);
            }
            if interrupted {
                // Calls the model finished writing stay in the history, answered as not run, so
                // the next request extends this one.
                if !produced.tool_calls.is_empty() {
                    convo.push(Message::tool_results(
                        produced.tool_calls.iter().map(|call| ToolResult::text(call.id.clone(), run_tools::INTERRUPTED, true)),
                    ));
                }
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
            // A fork in this step starts from this very request (A14): leave it for the call.
            if c.tool_calls.iter().any(crate::fork::is_fork_call) {
                if let Some(assistant) = convo.messages.last().cloned() {
                    let model = ResolvedModel { provider: live.provider.clone(), info: live.model.clone(), effort: live.effort };
                    convo.ext.insert(crate::fork::ForkBase::capture(&req, assistant, model, host.clone()));
                }
            }
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
            convo.ext.remove::<crate::fork::ForkBase>();
            // A subagent that read untrusted content taints its caller through its report.
            if convo.ext.remove::<crate::subagent::ReportTainted>() {
                convo.exposure.tainted = true;
            }
            for o in &self.observers {
                o.on_tools_done(convo, info, &c.tool_calls, &results);
            }
            convo.push(Message::tool_results(results));
            // A subagent that handed its report back ends here, after the whole batch has run: a
            // `write` beside the `handback` completed, and what follows is dropped.
            if convo.ext.get::<crate::subagent_tools::Handback>().is_some() {
                break TurnStop::Done;
            }
            // A local model's template treats any user message as the start of a new question and
            // drops the thinking before it, so a reminder after a tool result rides on that result
            // (which no request has carried yet) instead of becoming a message of its own.
            let local = matches!(live.model.provider, ProviderKind::Ollama | ProviderKind::LocalGguf);
            for m in &self.middleware {
                for extra in m.after_step(info, convo) {
                    if !(local && fold_into_last_result(convo, &extra)) {
                        convo.push(extra);
                    }
                }
            }
        };

        // A command's grant lasts for its turn.
        convo.ext.remove::<CommandGrant>();
        convo.ext.remove::<crate::directive::TurnDirectives>();
        let text = texts.join("\n\n");
        if stop != TurnStop::Error {
            self.memory.after_turn(&convo.session, said, &text);
        }
        TurnOutcome { stop, usage: spent.usage, steps: spent.steps, text, error, blocked: false, truncated }
    }

    /// The first middleware that wants the turn to continue, and its reminder.
    fn before_final(&self, info: StepInfo, convo: &mut Conversation, text: &str) -> Option<String> {
        self.middleware.iter().find_map(|m| match m.before_final(info, convo, text) {
            FinalFlow::Accept => None,
            FinalFlow::Continue(reminder) => Some(reminder),
        })
    }
}

/// Appends a harness reminder (a lone text message) to the last tool result of the step that just
/// ran. False when `extra` is anything else or the step left no result to carry it.
fn fold_into_last_result(convo: &mut Conversation, extra: &Message) -> bool {
    let [Part::Text { text }] = extra.content.as_slice() else { return false };
    if extra.role != Role::User {
        return false;
    }
    let Some(last) = convo.messages.last_mut().filter(|m| m.role == Role::Tool) else { return false };
    let Some(result) = last.content.iter_mut().rev().find_map(|p| match p {
        Part::ToolResult(r) => Some(r),
        _ => None,
    }) else {
        return false;
    };
    result.content.push_str("

");
    result.content.push_str(text);
    true
}

/// A request that did not extend the last one outside a compaction: a bug, said loudly in the
/// daemon log and kept for the tests that fail on it ([`marlowe_model::prefix::violations`]).
fn violation(line: String) {
    eprintln!("marlowe: PROMPT CACHE VIOLATION (a bug: only a compaction may rewrite what the model has read): {line}");
    marlowe_model::prefix::record_violation(line);
}

/// One model call, forwarding deltas to `sink`. Retries a retriable error only when nothing
/// was streamed yet, so the UI never sees duplicated text. A reply or reasoning that starts
/// repeating itself is cut there ([`guard::RepetitionWatch`]).
fn call_model(
    provider: &dyn Provider,
    req: &ChatRequest,
    cancel: &CancelToken,
    wire: &mut Option<PhaseTimer>,
    (call_started, est_prompt): (Instant, u32),
    sink: &mut dyn FnMut(AgentEvent),
) -> CallEnd {
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
        let mut first_token = true;
        // The prompt read, from the request leaving until the engine reports what it read.
        let mut prefill: Option<PhaseTimer> = None;
        let result = provider.stream(req, &call_cancel, &mut |ev| {
            let arrived = Instant::now();
            match &ev {
                StreamEvent::Sent => {
                    // Whatever came before (the model's metadata, the body) was preparation.
                    let prep = arrived.saturating_duration_since(call_started).as_millis() as u64;
                    sink(AgentEvent::Phase(PhaseEvent::start(PhaseKind::Wire).stage("prepare")));
                    sink(AgentEvent::Phase(PhaseEvent::end(PhaseKind::Wire, prep).stage("prepare")));
                    let mut start = PhaseEvent::start(PhaseKind::Prefill).with_total(u64::from(est_prompt)).unit("tok");
                    start.est = true;
                    sink(AgentEvent::Phase(start));
                    prefill = Some(PhaseTimer::begun(PhaseKind::Prefill));
                    return;
                }
                StreamEvent::Usage(u) => {
                    if let Some(p) = prefill.take() {
                        p.end_with(sink, |e| read_end(e, u));
                    }
                }
                // The engine's own count of the read so far (llama.cpp reports it per batch): the
                // live read line shows it as it is, with nothing estimated.
                StreamEvent::ReadProgress { total, cached, processed } => {
                    if let Some(w) = wire.take() {
                        w.end(sink);
                    }
                    if prefill.is_some() {
                        let mut p = PhaseEvent::progress(PhaseKind::Prefill, *processed, *total).unit("tok");
                        p.cached = Some(*cached);
                        sink(AgentEvent::Phase(p));
                    }
                    return;
                }
                _ => {}
            }
            // The first frame of any kind ends the wire phase; the first text or reasoning is the
            // origin of the token-to-screen overhead.
            if let Some(w) = wire.take() {
                w.end(sink);
            }
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
                if std::mem::take(&mut first_token) {
                    sink(AgentEvent::FirstToken { at: arrived });
                }
                sink(ae);
            }
        });
        // A call that ended before the engine reported its read still closes the phase.
        if let Some(p) = prefill.take() {
            p.end_with(sink, |e| e.detail("no report"));
        }
        if let Some(kept) = looped.filter(|_| !cancel.is_cancelled()) {
            return CallEnd::Looped { kept, made: partial.finish() };
        }
        match result {
            Ok(c) => return CallEnd::Done(c),
            Err(e) if e.is_cancelled() || cancel.is_cancelled() => {
                return CallEnd::Cancelled(partial.finish());
            }
            Err(e) if e.retriable && !emitted && attempt < RETRY_BACKOFF.len() => {
                if !sleep_unless_cancelled(RETRY_BACKOFF[attempt], cancel) {
                    return CallEnd::Cancelled(Completion::default());
                }
                attempt += 1;
            }
            Err(e) => return CallEnd::Failed(e, !text.trim().is_empty()),
        }
    }
}

/// The prefill phase's end: what the call read, from the engine's own counts. An engine that
/// reports its cache (`cached_input`) gives the split. Ollama's `prompt_eval_count` is the whole
/// prompt, cached part included, so without its runner log there is no split here (`cached` unset)
/// and no rate: only the prompt's size and the time its read took.
fn read_end(e: PhaseEvent, u: &Usage) -> PhaseEvent {
    let mut e = e.unit("tok");
    match u.prompt_eval {
        Some(total) => e = e.with_done(total).with_total(total),
        None => {
            let total = u.input;
            e = e.with_done(total.saturating_sub(u.cached_input)).with_total(total).with_cached(u.cached_input);
        }
    }
    if let Some(ms) = u.prefill_ms {
        e.ms = Some(ms);
    }
    e
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

#[cfg(test)]
mod fold_tests {
    use super::*;
    use crate::convo::Conversation;
    use marlowe_proto::SessionId;

    fn convo_with_result() -> Conversation {
        let mut c = Conversation::new(SessionId::from("s"), None);
        c.push(Message::user("go"));
        c.push(Message::tool_results([ToolResult { call_id: "a".into(), content: "output".into(), is_error: false, parts: vec![] }]));
        c
    }

    #[test]
    fn a_reminder_after_a_tool_result_rides_on_that_result() {
        let mut c = convo_with_result();
        assert!(fold_into_last_result(&mut c, &Message::user("<system-reminder>stop repeating</system-reminder>")));
        assert_eq!(c.messages.len(), 2, "no message of its own");
        let got = c.messages[1].tool_results_iter().next().unwrap().content.clone();
        assert_eq!(got, "output

<system-reminder>stop repeating</system-reminder>");
    }

    #[test]
    fn nothing_folds_when_there_is_no_result_or_the_message_is_not_a_lone_user_text() {
        let mut c = Conversation::new(SessionId::from("s"), None);
        c.push(Message::user("go"));
        assert!(!fold_into_last_result(&mut c, &Message::user("reminder")), "the last message is the user's");
        let mut c = convo_with_result();
        assert!(!fold_into_last_result(&mut c, &Message::assistant("an answer")));
        assert_eq!(c.messages[1].tool_results_iter().next().unwrap().content, "output");
    }
}
