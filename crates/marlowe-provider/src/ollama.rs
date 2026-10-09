//! The driver. Speaks Ollama's `/api/chat`, returns a [`ModelStep`].
//!
//! # Degrading honestly (ADR-028 requirement 1, invariant 4)
//!
//! An absent endpoint is **not** a crash and **not** a silent fallback. [`Availability::probe`]
//! answers three distinguishable questions — is anything listening, does it speak Ollama, is the
//! routed model present — because the remedies differ and a user cannot act on "unavailable".
//! Each answer carries the command that fixes it.

use std::time::Duration;

use marlowe_loop::SourceKind;
use marlowe_loop::{
    CallLimits, ClaimRequest, CondensedResult, ContextView, DegradedPath, ModelCall, ModelDriver,
    ModelStep, ProviderError, Usage,
};
use marlowe_permission::{ArgValue, Args};
use marlowe_tools::{ExposedSet, ToolId, ToolRegistry};

use crate::capability::ModelCapability;
use crate::http::{self, HttpError, LocalEndpoint};
use crate::routing::Routing;

/// The pinned default, chosen on **measured tool-call reliability** (ADR-028 requirement 2).
///
/// **Measured 2026-08-08, `tests/tool_call_probe.rs`, 12 trials: 12/12 well-formed, 12/12 with
/// the correct target, median 1666 ms.**
///
/// **And the honest qualification, because a point estimate is not an interval.** 12/12 is a
/// perfect score on twelve trials; the 95% Clopper-Pearson lower bound is ≈0.74, which is *below*
/// [`crate::capability::MIN_RATE`]. So this clears the bar **on the point estimate and not with
/// its interval** — the same distinction K1's amendment turns on, and it is stated here rather
/// than rounded away. Twelve trials is thin; the number to raise is
/// [`crate::capability::MIN_TRIALS`], and raising it costs only probe time.
///
/// It is a constant so the choice is one line to change and one line to review.
pub const DEFAULT_MODEL: &str = "qwen3.5:9b";

/// What [`DEFAULT_MODEL`] was measured to do, so a run can disclose it without re-measuring.
///
/// A build that changes `DEFAULT_MODEL` and not this is a build that discloses one model's
/// number under another model's name — so the pair is asserted in `tests`.
/// What is measured about `model` — and **nothing is measured about any model but the default.**
///
/// # This is the measurement-transfer family, closed at the one place it would happen
///
/// [`default_capability`] carries `12/12 well-formed, measured 2026-08-08` — a reading taken on
/// `qwen3.5:9b` and on nothing else. The moment a user can choose their own model, the obvious
/// implementation hands that same struct back for every choice, and `--status` reports one model's
/// measured reliability under another's name. The number would be correct, the reasoning sound, and
/// it would be about a different system.
///
/// So a non-default model gets [`ModelCapability::unmeasured`], which renders **"tool-call
/// reliability NOT MEASURED on this machine"**. That is a worse-looking status line and a true one,
/// and it is the disclosure ADR-028 asks for: the user picked the model, so the user is the one who
/// needs to know nobody has measured it.
pub fn capability_for(model: &str) -> ModelCapability {
    if model == DEFAULT_MODEL {
        default_capability()
    } else {
        ModelCapability::unmeasured(model)
    }
}

pub fn default_capability() -> ModelCapability {
    ModelCapability {
        model: DEFAULT_MODEL.to_string(),
        parameters: Some("9B".into()),
        tool_calls: Some(crate::capability::ToolCallReport {
            trials: 12,
            well_formed: 12,
            correct_target: 12,
            measured_on: "2026-08-08".into(),
            median_ms: 1666,
        }),
    }
}

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(180);

/// What is wrong, specifically enough to fix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    Ready { models: Vec<String> },
    /// Nothing is listening.
    EndpointDown { endpoint: String },
    /// Something answered but it is not Ollama.
    NotOllama { endpoint: String, detail: String },
    /// Ollama is up; the routed model is not pulled.
    ModelMissing { model: String, available: Vec<String> },
}

impl Availability {
    pub fn is_ready(&self) -> bool {
        matches!(self, Availability::Ready { .. })
    }

    /// The line the user sees. **Names the remedy**, because a degraded state a user cannot act
    /// on is a crash with better manners.
    pub fn remedy(&self) -> String {
        match self {
            Availability::Ready { .. } => "ready".into(),
            Availability::EndpointDown { endpoint } => format!(
                "no model available — nothing is listening on {endpoint}. Start it with \
                 `ollama serve`"
            ),
            Availability::NotOllama { endpoint, detail } => format!(
                "no model available — something is listening on {endpoint} but it is not Ollama \
                 ({detail})"
            ),
            Availability::ModelMissing { model, available } => {
                let have = if available.is_empty() {
                    "none are pulled".to_string()
                } else {
                    format!("available: {}", available.join(", "))
                };
                format!("no model available — `{model}` is not pulled. Run `ollama pull {model}` ({have})")
            }
        }
    }

    /// §B5's status band carries this, in amber. Invariant 4: the flag is on the run, so silent
    /// degradation is not representable.
    pub fn degraded_path(&self) -> Option<DegradedPath> {
        (!self.is_ready()).then_some(DegradedPath::ModelUnavailable)
    }

    pub fn probe(endpoint: &LocalEndpoint, routing: &Routing) -> Self {
        let tags = match http::get_json(endpoint, "/api/tags", Duration::from_secs(5)) {
            Ok(v) => v,
            Err(HttpError::Unreachable { .. }) => {
                return Availability::EndpointDown { endpoint: endpoint.to_string() }
            }
            Err(e) => {
                return Availability::NotOllama {
                    endpoint: endpoint.to_string(),
                    detail: e.to_string(),
                }
            }
        };
        let Some(models) = tags.get("models").and_then(|m| m.as_array()) else {
            return Availability::NotOllama {
                endpoint: endpoint.to_string(),
                detail: "no `models` array in /api/tags".into(),
            };
        };
        let names: Vec<String> = models
            .iter()
            .filter_map(|m| m.get("name").and_then(|n| n.as_str()).map(str::to_string))
            .collect();

        for wanted in routing.models() {
            if !names.iter().any(|n| n == wanted || n == &format!("{wanted}:latest")) {
                return Availability::ModelMissing {
                    model: wanted.to_string(),
                    available: names,
                };
            }
        }
        Availability::Ready { models: names }
    }
}

/// Restore the pre-fix placement of injected memory: chained onto the leading system message.
///
/// **An instrument, and the default is the fixed behaviour.** See the call site in
/// `build_request` for why one binary has to be able to produce both arms. Read once per
/// request, which is thousands of times cheaper than the model call it precedes and keeps the
/// switch honest — a value cached at startup would silently ignore a mid-run change and make a
/// mis-set arm look like a null result.
fn memory_at_head() -> bool {
    std::env::var("MARLOWE_MEMORY_AT_HEAD").is_ok_and(|v| v != "0" && !v.is_empty())
}

/// The context window used when none is given, in tokens.
///
/// **Declared and always sent, never inferred.** Ollama's own default is 2048 whatever the model
/// supports; a request that omits `num_ctx` gets it silently. Measured on this machine before
/// pinning — see STATE.md.
pub const DEFAULT_CONTEXT_TOKENS: u32 = 32_768;

/// What `qwen3.5:9b` reports it supports, recorded so a model swap has something to compare to.
///
/// **Not the default**: the whole window is a KV-cache commitment and 262k on a 9B model is far
/// more memory than a terminal session should take. Recorded, not used.
pub const MODEL_CONTEXT_CEILING: u32 = 262_144;

/// A sink for raw provider frames, before any interpretation.
///
/// **`--dev` only.** A slow turn and a hung one are indistinguishable from the outside, and the
/// pre-parse frames are the only view that separates *the model is emitting slowly* from *nothing
/// is arriving*. That is a diagnostic, not conversation, so it does not go near the transcript.
pub type RawFrameSink = Box<dyn FnMut(&serde_json::Value) + Send>;

/// A sink for the **outbound request body**, as it goes over the wire.
///
/// # This closes a blind spot a source-level test cannot see
///
/// `persona_emission.rs` asserts that `request_body` places the persona in the system role. It
/// passes whether the deployed binary is current or six commits stale — and in M2 C2e it did
/// exactly that: the persona was correctly wired, the test was green, and the daemon had been
/// serving pre-persona code for an hour because the release binary was never rebuilt.
///
/// **Source-verified is weaker than deployment-verified.** The only reading that settles "is the
/// persona reaching the model" is the bytes the running process sent, so this exists to produce
/// them. Fourth instance of one shape; see CLAUDE.md's table.
pub type RequestSink = Box<dyn FnMut(&serde_json::Value) + Send>;

pub struct OllamaDriver {
    endpoint: LocalEndpoint,
    routing: Routing,
    registry: ToolRegistry,
    capability: ModelCapability,
    timeout: Duration,
    raw_frames: Option<RawFrameSink>,
    request_dump: Option<RequestSink>,
    /// Whether the model is asked to separate its chain of thought into the `thinking` channel.
    ///
    /// **A declared setting, not an inherited default.** Ollama decides this when the field is
    /// absent, and its answer varies by model and by version — so leaving it out means reasoning
    /// silently moves into `content` on some upgrade, and lands in the transcript as Marlowe's
    /// answer. That is the `num_ctx` shape again: a default that makes a mismatch unobservable.
    ///
    /// Off is a legitimate choice — a non-reasoning model gains nothing, and a user who wants
    /// no thinking block at all is asking for something reasonable. It is a setting rather than a
    /// constant so both are reachable and both are stated.
    thinking: bool,
    /// The context window, in tokens, carried on **every** request as `num_ctx`.
    ///
    /// **Ollama defaults this to 2048 regardless of what the model supports**, and until M2 C2e
    /// nothing set it — so a 262,144-token model ran in a 2,048-token window and silently
    /// truncated history, injected memory and tool results. Worse, the assembler was packing to
    /// 32,000: two numbers, disagreeing, with §6's compaction trigger computed from the wrong one.
    ///
    /// **This is the single source.** `Engine`'s assembler window is derived from the same value;
    /// see `marlowe-daemon`'s `Daemon::ask` and `tests/context_window.rs`.
    context_tokens: u32,
}

impl OllamaDriver {
    pub fn new(endpoint: LocalEndpoint, routing: Routing, registry: ToolRegistry) -> Self {
        let capability = ModelCapability::unmeasured(routing.model_for(
            marlowe_loop::ModelRoute::Orchestrator,
        ));
        Self {
            endpoint,
            routing,
            registry,
            capability,
            timeout: DEFAULT_TIMEOUT,
            raw_frames: None,
            request_dump: None,
            thinking: true,
            context_tokens: DEFAULT_CONTEXT_TOKENS,
        }
    }

    pub fn with_capability(mut self, capability: ModelCapability) -> Self {
        self.capability = capability;
        self
    }

    pub fn capability(&self) -> &ModelCapability {
        &self.capability
    }

    pub fn availability(&self) -> Availability {
        Availability::probe(&self.endpoint, &self.routing)
    }

    /// The exposed tools, in Ollama's function schema.
    ///
    /// **One definition, in [`crate::wire::openai_tool_schema`].** Ollama's `/api/chat` follows the
    /// OpenAI function schema for this field, so the array is byte-identical to what the hosted and
    /// llama.cpp adapters send — and it was written out three times, with the two copies of
    /// `param_description` already disagreeing about whether a parameter's own words reach the
    /// model. See that module's header.
    fn tool_schema(&self, exposed: &ExposedSet) -> serde_json::Value {
        crate::wire::openai_tool_schema(&self.registry, exposed)
    }
}

impl OllamaDriver {
    /// The outbound request, built and returned rather than sent.
    ///
    /// **Extracted so the persona can be asserted where it actually matters.** Addendum C §C6 puts
    /// the persona in the stable tier of the system prompt, and the only check worth having is
    /// that it reaches *this* value — the bytes that go to the provider. A test that loaded
    /// `persona/v1.md` and asserted it was non-empty would prove the file loaded and nothing else.
    ///
    /// That is the same distinction as `get_providers()` reporting *registered* execution
    /// providers versus where nodes actually ran (M0c Session L), and as a pipe-tested hook
    /// matcher versus an observed permission prompt. Three subsystems, one shape.
    pub fn request_body(
        &self,
        view: &ContextView,
        tools: &ExposedSet,
        limits: CallLimits,
    ) -> serde_json::Value {
        // **The call's own column, not a constant.** This line read
        // `model_for(ModelRoute::Orchestrator)` until M3 Session C, which is why
        // `CapabilityProfile::model_route` had zero readers and the entire subagent tree ran on
        // the secretary's model whatever any profile declared.
        //
        // **It changes no byte in the shipped product today, and that is the honest reading.**
        // Every production `Routing` is `Routing::uniform`, so all three columns answer one name
        // and a green test here proves nothing about the running daemon. A live `--dev` outbound
        // dump under a three-tag routing is the only instrument that can — the
        // persona-loaded-versus-in-the-request-body lesson, one subsystem over.
        let model = self.routing.model_for(limits.route).to_string();

        // **Roles are derived from the block's origin, not from its tier.**
        //
        // Every non-stable block used to go out as `role: "user"` — including `History` blocks
        // holding MARLOWE'S OWN PRIOR REPLIES. The model therefore read its own last answer as
        // something the user had just said, and answered it. Observed at the terminal:
        //
        //     Hello. What do you need?   →   Nothing in particular.   →   Nothing yet either.
        //
        // A conversation with itself, ended only by the budget. `/api/chat` has `system`,
        // `user`, `assistant` and `tool` precisely so this cannot happen, and collapsing three of
        // them into one threw away the distinction the endpoint exists to carry.
        //
        // `TrustClass` is origin-bound and never derived from content (§3.3), which makes it the
        // right discriminator: what the user asserted is `user`, what the agent inferred is
        // `assistant`, and a tool's output is `tool`.
        let mut messages = Vec::new();

        // ── ONE system message, at position 0. This is a chat-template constraint. ──────
        //
        // **Reported live against a qwen3-next model:** Ollama returned HTTP 400 with
        // `Jinja Exception: System message must be at the beginning`. Templates differ in what
        // they tolerate, and several of the newer ones accept **exactly one** system message and
        // require it first. This emitted one per stable block and one per context block — a
        // dozen — and then a further one mid-conversation for injected memory (below). qwen3.5's
        // template accepted all of it, which is why it went unnoticed.
        //
        // `/api/chat` itself is fine with the old shape; the *template* is not, and the template is
        // the provider's. So the wire is built to the strictest common shape rather than to what
        // one model happens to allow — the alternative is a per-model branch, and a wire format
        // that varies by model is the thing that makes a model swap a debugging session.
        //
        // Blocks keep their identity in the view; this only concatenates their **text** for
        // transport. Order is stable-then-context, unchanged, so the persona still leads and the
        // prefix is still cache-friendly.
        let system: Vec<&str> = view
            .stable
            .iter()
            // Project files, skills and tool schemas describe the world rather than speak in it,
            // so they belong here too: context the assistant has, not turns anybody took.
            .chain(view.context.iter())
            // **Injected memory joins the system message rather than sitting mid-conversation.**
            // It lives in the volatile tier because it is trimmable, but on the wire it is context,
            // not a turn — the old code already said so by giving it `role: system`, and that is
            // precisely what put a system message after the user's turn and broke the template.
            // Attributing it to the user instead would make a recalled fact indistinguishable from
            // something they just said, and §B1 keeps memory out of the interface, which starts
            // with not pretending it was spoken.
            .map(|b| b.text.as_str())
            .filter(|t| !t.trim().is_empty())
            .collect();
        // **INJECTED MEMORY IS NO LONGER CHAINED IN HERE — see the tail of this function.**
        //
        // This is the leading system message, at **position 0**. Ollama reuses its prompt cache
        // only for a byte-identical prefix, and memory is re-retrieved every turn — so a single
        // changed fact invalidated this message AND every turn of conversation behind it.
        // Measured on this machine: a **148-byte** change at the front cost **1.45 seconds**, in an
        // ordinary run nobody was provoking; over ten turns prompt evaluation climbed
        // **689 → 1,320 ms**, monotone in six passes. The counterfactual — the same content after
        // the history — was **flat at ~275 ms regardless of turn count**.
        let recalled: Vec<&str> = view
            .volatile
            .iter()
            // Skills hits ride the same tail as memory: both are "the harness found this for THIS
            // turn", both are query-dependent, and both must stay out of the cached prefix.
            .filter(|b| matches!(b.source, SourceKind::InjectedMemory | SourceKind::Skills))
            .map(|b| b.text.as_str())
            .filter(|t| !t.trim().is_empty())
            .collect();
        if !system.is_empty() {
            messages.push(serde_json::json!({
                "role": "system",
                "content": system.join("\n\n"),
            }));
        }
        for block in view.volatile.iter() {
            let role = match block.source {
                // A child's return, on the wire, IS a tool result: the assistant turn
                // `Engine::spawn` pushes announces it by id, and `unorphan_tool_messages` demotes
                // it to `user` if that turn is ever missing. What it must never be is
                // `assistant` -- that is the parent answering itself. See `Engine::spawn`.
                SourceKind::ToolResults | SourceKind::ChildResults => "tool",
                // The brief is what the PARENT said to this run. It is the child's user
                // turn, whatever trust class it carries -- see `SourceKind::Brief`.
                SourceKind::Brief => "user",
                SourceKind::History => {
                    match block.trust {
                        // The one the bug turned on.
                        marlowe_contract::TrustClass::AgentInferred => "assistant",
                        _ => "user",
                    }
                }
                // Already emitted, in the single leading system message. Skipped here rather than
                // given a role, because a `system` message at this position is what broke a
                // qwen3-next template with `System message must be at the beginning`.
                // Both ride the trailing system message -- see the tail of this function. Without
                // this arm they were emitted TWICE: once here as `user` via the catch-all, and
                // again at the tail as `system`. Measured in a live dump.
                SourceKind::InjectedMemory | SourceKind::Skills => continue,
                _ => "user",
            };
            let mut msg = serde_json::json!({ "role": role, "content": block.text });

            // ── THE STRUCTURE PROSE CANNOT CARRY ────────────────────────────────────
            //
            // `/api/chat` puts `tool_calls` on the **assistant** message, `tool_name` on the
            // **tool** message, and reasoning in `thinking`. Sending only `content` throws all
            // three away, and the dump of a real run showed what that costs:
            //
            //     [  5] tool  54 chars  tool_calls=0  tool_name=""  "983 lines · ref 225bfe…"
            //     [  6] tool  54 chars  tool_calls=0  tool_name=""  "983 lines · ref 225bfe…"
            //
            // Five results, nothing that called them, no way to tell them apart. Measured against
            // a live model: given that shape it abandons the task and narrates; given this one it
            // acts on the result.
            if let Some(w) = &block.wire {
                if let Some(t) = &w.thinking {
                    if !t.is_empty() {
                        msg["thinking"] = serde_json::Value::String(t.clone());
                    }
                }
                if !w.tool_calls.is_empty() {
                    msg["tool_calls"] = serde_json::Value::Array(
                        w.tool_calls
                            .iter()
                            .map(|c| {
                                // `id` pairs this call with the `tool_call_id` on its result.
                                // Without it a batch of three calls to the same tool returns
                                // three results the model cannot tell apart, so a partial
                                // failure reads as a total one.
                                serde_json::json!({
                                    "id": c.id,
                                    "function": { "name": c.name, "arguments": c.arguments }
                                })
                            })
                            .collect(),
                    );
                }
                if let Some(n) = &w.tool_name {
                    msg["tool_name"] = serde_json::Value::String(n.clone());
                }
                if let Some(id) = &w.tool_call_id {
                    msg["tool_call_id"] = serde_json::Value::String(id.clone());
                }
            }
            messages.push(msg);
        }

        // **A `tool` message that answers no call is refused by a strict endpoint**, and the
        // quarantined reader's window is nothing but such messages — it never called anything and
        // structurally never can. Ollama's own template already showed the damage before any
        // hosted provider did: *"a MALFORMED conversation: no assistant `tool_calls`, no
        // `tool_name`, so the template left a block open and the model continued it in
        // `content`."* See `crate::wire`.
        // ── INJECTED MEMORY, AT THE TAIL ────────────────────────────────────────────────────
        //
        // **A SECOND `system` MESSAGE, and here that is safe where it was not before.** The comment
        // this replaces records a real failure: a `system` message placed after the user's turn
        // broke a qwen3-next Jinja template with `System message must be at the beginning`. Ollama
        // serving qwen3.5 does not use that template -- it applies a BUILT-IN Go renderer for the
        // architecture and ignores the `TEMPLATE` field entirely, which this project established
        // with a BANANA control after announcing the opposite conclusion and being wrong.
        //
        // So the constraint that forced memory into position 0 is a property of one template on one
        // driver, and it is not this one. Keeping it here cost a full prompt re-evaluation every
        // time a memory changed.
        //
        // A `system` role rather than `user`: a recalled fact must not be indistinguishable from
        // something the person just said. Rather than `assistant`: the model did not say it.
        //
        // **If a future Ollama model does refuse a trailing system message, the symptom is loud** --
        // an immediate template error on every turn, not a silent degradation -- and the fix is to
        // move it onto the assistant+tool rails the llama.cpp driver uses.
        if !recalled.is_empty() {
            if memory_at_head() {
                // ── THE OLD PLACEMENT, REACHABLE ON PURPOSE ─────────────────────────────────
                //
                // **A measurement scaffold, not a product option**, and it exists because of a
                // rule this project keeps relearning: a fix is not verified until the thing it
                // fixed is measured on the SAME machine, in the SAME session, against the SAME
                // binary. Building "before" and "after" as two binaries makes every difference
                // between the two builds a candidate explanation for the gap.
                //
                // Off unless the variable is set, so the shipped path is the fixed one and a
                // forgotten flag cannot silently restore the regression. It is deliberately an
                // environment variable rather than a config field: config is a surface users see
                // and this is an instrument.
                let head = recalled.join("\n\n");
                if let Some(first) = messages.first_mut() {
                    if first.get("role").and_then(|r| r.as_str()) == Some("system") {
                        let joined = format!(
                            "{}\n\n{head}",
                            first.get("content").and_then(|c| c.as_str()).unwrap_or("")
                        );
                        first["content"] = serde_json::Value::String(joined);
                    }
                }
            } else {
                messages.push(serde_json::json!({
                    "role": "system",
                    "content": recalled.join("\n\n"),
                }));
            }
        }

        crate::wire::unorphan_tool_messages(&mut messages);

        // ── SLIDING-WINDOW REASONING ────────────────────────────────────────────────
        //
        // Only the most recent assistant turn keeps its `thinking`. Older reasoning is not merely
        // useless, it **compounds**: three turns of "let me check one more thing" replayed
        // together read as a standing instruction to keep checking, and that is the tool-farming
        // spiral. Reasoning is bound to the chain in progress and expires when a new turn begins.
        let newest_assistant = messages
            .iter()
            .rposition(|m| m.get("role").and_then(|r| r.as_str()) == Some("assistant"));
        for (i, m) in messages.iter_mut().enumerate() {
            if Some(i) != newest_assistant {
                if let Some(obj) = m.as_object_mut() {
                    obj.remove("thinking");
                }
            }
        }

        let mut body = serde_json::json!({
            "model": model,
            "messages": messages,
            "stream": true,
            // Declared, never inherited — see `OllamaDriver::thinking`.
            "think": self.thinking,
            "options": {
                // The budget's hard cap, handed to the provider. `budget` is explicit that this
                // is the mechanism and the top-of-loop check is only the backstop.
                // **Capped against the window.** The budget's `max_output_tokens` was reaching
                // the provider as 200,000 against a 32,768-token window — six times more output
                // than the context can hold, which is unbounded generation with extra steps and
                // is most of why a reasoning model appeared to run forever. A quarter of the
                // window leaves room for the prompt it has to answer from.
                "num_predict": limits
                    .max_output_tokens
                    .min((self.context_tokens / 4) as u64)
                    .min(i32::MAX as u64) as i64,
                // **Never omitted.** Omitting it is Ollama's 2048, which is the permissive
                // default this project keeps deleting — and it was live the whole time.
                "num_ctx": self.context_tokens,
            }
        });

        // **`tools` is omitted, never sent empty.** `[]` is a schema violation in this dialect,
        // not "no tools" — and the two things that produce it are both deliberate: layer 1's
        // empty `ExposedSet` for the quarantined reader, and `FARMING_HARD_STOP` withholding
        // tools from the parent for one call. See `crate::wire::tools_field`.
        if let Some(tools) = crate::wire::tools_field(self.tool_schema(tools)) {
            body["tools"] = tools;
        }
        body
    }
}

impl OllamaDriver {
    /// Attach a raw-frame sink. `--dev` wires this; nothing else does.
    pub fn with_raw_frames(mut self, sink: RawFrameSink) -> Self {
        self.raw_frames = Some(sink);
        self
    }

    /// Attach an outbound-request sink. `--dev` wires this; nothing else does.
    pub fn with_request_dump(mut self, sink: RequestSink) -> Self {
        self.request_dump = Some(sink);
        self
    }

    /// Set the context window. **The same number must reach the assembler** — see
    /// [`DEFAULT_CONTEXT_TOKENS`].
    /// Ask the model to separate reasoning from its answer, or not.
    pub fn with_thinking(mut self, on: bool) -> Self {
        self.thinking = on;
        self
    }

    pub fn thinking(&self) -> bool {
        self.thinking
    }

    pub fn with_context_tokens(mut self, tokens: u32) -> Self {
        self.context_tokens = tokens;
        self
    }

    pub fn context_tokens(&self) -> u32 {
        self.context_tokens
    }
}

impl ModelDriver for OllamaDriver {
    fn call(
        &mut self,
        view: &ContextView,
        tools: &ExposedSet,
        limits: CallLimits,
    ) -> Result<ModelCall, ProviderError> {
        self.call_streaming(view, tools, limits, &mut |_| {})
    }

    /// This driver streams, so the engine must not re-emit the assembled reply.
    fn streams(&self) -> bool {
        true
    }

    fn call_streaming(
        &mut self,
        view: &ContextView,
        tools: &ExposedSet,
        limits: CallLimits,
        on_delta: &mut dyn FnMut(&str),
    ) -> Result<ModelCall, ProviderError> {
        self.call_streaming_split(view, tools, limits, on_delta, &mut |_, _| {}, &mut |_| {})
    }

    /// # THE TOKEN COUNT, AND WHY A FRAME IS NOT A TOKEN HERE
    ///
    /// `daemon.rs` asserted *"one delta is one token — Ollama sends one frame per token"* and
    /// `STATE.md` recorded that nothing measured it. Measured now, against `llama-server
    /// /tokenize` on the same GGUF blob, and **it is false on the thinking channel**:
    ///
    /// | prompt | thinking frames | exact tokens |
    /// |---|---|---|
    /// | `Say hi.` | 116 | 120 |
    /// | `What is 2+2?` | 179 | 200 |
    /// | `Name one primary colour.` | 236 | 250 |
    ///
    /// Identical across three repetitions of one prompt, so it is not the client being slow:
    /// Ollama's own thinking parser buffers a whitespace-leading token and flushes it joined to
    /// the next one, so one frame carries two tokens' worth of text — visible in a raw frame dump
    /// as a newline pair arriving glued to the digit after it, and two spaces glued to the
    /// emphasis marker after them. `/v1/chat/completions` merges identically, so there is no
    /// endpoint to move to. On the **content** channel a frame is exactly one token (60 frames,
    /// `eval_count` 61, the extra being the stop token).
    ///
    /// So a frame count is a **lower bound**, and it is used as one: it ticks while the call runs
    /// because a number that moves is the point of the line, and when `done` arrives the block is
    /// corrected to the engine's own `eval_count`. The rule for that correction is
    /// [`ModelDriver::call_streaming_split`]'s and holds for every model: **a token the engine
    /// counted but never streamed belongs to the channel that was open.** No template constant is
    /// subtracted, because `</think>` costs a different number of tokens in a different vocabulary
    /// and a constant measured on one model is not a fact about another.
    fn call_streaming_split(
        &mut self,
        view: &ContextView,
        tools: &ExposedSet,
        limits: CallLimits,
        on_delta: &mut dyn FnMut(&str),
        on_reasoning: &mut dyn FnMut(&str, u64),
        on_retract: &mut dyn FnMut(u64),
    ) -> Result<ModelCall, ProviderError> {
        let body = self.request_body(view, tools, limits);

        // **The bytes that actually go out**, dumped before they are sent. Not the same claim as
        // a test asserting on a body built in a test process — see `RequestSink`.
        if let Some(sink) = self.request_dump.as_mut() {
            sink(&body);
        }

        // **Streamed, and the frames are folded as they land.** Layer 2 of the loop still emits
        // one `TextDelta` per `Say`, so this alone does not put tokens on screen — what it does
        // is make them exist incrementally at all, and feed `--dev`'s raw view.
        let mut stream = http::post_ndjson(&self.endpoint, "/api/chat", &body, self.timeout)
            .map_err(|e| ProviderError {
                detail: format!("{e}"),
                // Unreachable is worth a failover attempt; a malformed body is not — it will be
                // malformed on the next provider too.
                retriable: matches!(e, HttpError::Unreachable { .. }),
            })?;

        let mut text = String::new();
        // **Reasoning that arrives in `content` is split back out of it here**, not left for the
        // surface to guess at. A `</think>` reaching the screen is the loudest possible statement
        // that the channel split was wrong; see `crate::think`.
        let mut splitter = crate::think::ThinkSplitter::new();

        // -- NOTHING RENDERS AS SPEECH UNTIL THE THINK BLOCK IS KNOWN SHUT ---------------
        //
        // **The requirement, stated by the person who kept seeing it violated:** everything that
        // comes after `<think>` is in the think block, and it never leaves that container until
        // `</think>` is emitted.
        //
        // The earlier design streamed content as speech and **retracted** it when a late
        // `</think>` proved it wrong. That ends correct and still shows purple text on screen
        // first, which is the thing the rule forbids. There is no online signal that a closing tag
        // is coming, so satisfying the rule means holding.
        //
        // **What holding costs, measured rather than assumed.** `--dev`'s dump of a real
        // 848-frame turn: the `</think>` arrived at frame **846**, and the whole answer was frame
        // 847 -- seven bytes. Reasoning is where the wall time goes and it streams live
        // throughout; the answer was always going to arrive in one piece at the end.
        //
        // `closed` starts false because the OPENING tag is emitted by the chat template and
        // consumed before the stream begins: `<think>` never appears on the wire, only its close.
        let mut held = String::new();
        // **What "known shut" means, measured on this endpoint.**
        //
        // Ollama parses the think block itself and hands reasoning over in `message.thinking`.
        // The moment a `thinking` delta arrives, the block is *its* to close and `content` is
        // outside it — so content streams, and the rule is not violated because the model is not
        // in the container any more.
        //
        // The tagged case this held for — 454 content frames and a `</think>` at frame 846 — was
        // produced by a MALFORMED conversation: no assistant `tool_calls`, no `tool_name`, so the
        // template left a block open and the model continued it in `content`. With the wire shape
        // corrected, the same prompt measures:
        //
        //     call 0: native_thinking=235B  content=  0B  close_tag_in_content=false
        //     call 1: native_thinking=450B  content= 31B  close_tag_in_content=false
        //
        // Clean, both calls. Holding every byte to the end of the call made the reply appear in
        // one lump instead of streaming, which is what it was reported as.
        //
        // Starting `closed` at `!self.thinking` covers the other direction: with thinking off
        // there is no block to be inside, so nothing should ever be held. An explicit `<think>`
        // in content still puts the splitter `inside` regardless, and a bare `</think>` still
        // retracts — both backstops stay.
        let mut closed = !self.thinking;
        let mut tool_calls: Vec<serde_json::Value> = Vec::new();
        let mut usage = Usage { prompt_tokens: 0, completion_tokens: 0, micros_usd: 0, wall_ms: 0 };

        // ── THE TOKEN LEDGER FOR THIS CALL ──────────────────────────────────────────────
        //
        // Every streamed frame is one token spent, and it is spent on exactly one channel. The
        // three counters below and `usage.completion_tokens` are the whole accounting: what the
        // engine says it generated, minus what went to the answer, is what the thinking cost.
        //
        // `held_tokens` is the frames sitting in `held` — counted when they arrived, so that the
        // buffer's eventual release adds text without adding a second count for the same tokens.
        let mut reasoning_tokens: u64 = 0;
        let mut speech_tokens: u64 = 0;
        let mut held_tokens: u64 = 0;

        while let Some(frame) = stream.next_value() {
            // LOOP-EXEMPT: consuming a response stream, not a driving loop.
            let frame = frame.map_err(|e| ProviderError {
                detail: format!("{e}"),
                retriable: matches!(e, HttpError::Unreachable { .. }),
            })?;

            // **The raw frame, before any interpretation.** §B1 keeps memory out of the
            // interface; this is not memory, it is the provider's own wire, and it is the only
            // view that separates "the model is emitting slowly" from "nothing is arriving".
            // Behind `--dev` because it is diagnostic, not conversation.
            if let Some(sink) = self.raw_frames.as_mut() {
                sink(&frame);
            }

            if let Some(msg) = frame.get("message") {
                // **Reasoning models put their chain of thought here, not in `content`.**
                // `qwen3.5:9b` emitted 2,615 of 2,862 frames with an empty `content` — all of the
                // work was in `thinking`, and none of it was visible. Ollama has used both spellings.
                // **One frame is one token, charged once.** The loop reads three spellings of
                // the same channel; a frame carrying two of them is still one token, so the
                // charge goes on the first emit and every later one this frame carries `0`.
                let mut frame_charged = false;
                for field in ["thinking", "reasoning", "reasoning_content"] {
                    if let Some(r) = msg.get(field).and_then(|r| r.as_str()) {
                        if !r.is_empty() {
                            let charge = u64::from(!frame_charged);
                            frame_charged = true;
                            reasoning_tokens += charge;
                            on_reasoning(r, charge);
                            // The provider is separating the channels, so whatever arrives in
                            // `content` is outside the block. See `closed`'s header.
                            closed = true;
                        }
                    }
                }
                if let Some(c) = msg.get("content").and_then(|c| c.as_str()) {
                    if !c.is_empty() {
                        // This frame's one token, charged to whichever channel the splitter
                        // sends it to. A frame that straddles `</think>` pays once, on the
                        // reasoning side, because that is where the boundary token was produced.
                        let mut charge = u64::from(!frame_charged);
                        let split = splitter.feed(c);
                        if split.retract_speech {
                            // A `</think>` proved everything before it was reasoning.
                            if !held.is_empty() {
                                // Held, never rendered — route it and move on, nothing to undo.
                                // Its tokens were charged as the frames arrived; the release
                                // carries `0` so they are not counted a second time.
                                reasoning_tokens += held_tokens;
                                on_reasoning(&held, std::mem::take(&mut held_tokens));
                                held.clear();
                            }
                            if !text.is_empty() {
                                // **Already streamed, so it must be taken back.** Reachable when
                                // the channels looked separated and the model then closed a block
                                // in `content` anyway. Measured as not occurring once the wire
                                // shape was fixed — kept because "measured as not occurring" is a
                                // statement about one model on one day.
                                text.clear();
                                // The speech is moving into the thinking block, so its cost
                                // moves with it. Both ledgers, or the total stops adding up.
                                reasoning_tokens += speech_tokens;
                                on_retract(std::mem::take(&mut speech_tokens));
                            }
                            closed = true;
                        }
                        for seg in &split.segments {
                            match seg {
                                crate::think::Segment::Reasoning(r) => {
                                    reasoning_tokens += charge;
                                    on_reasoning(r, std::mem::take(&mut charge));
                                }
                                crate::think::Segment::Speech(t) => {
                                    if closed {
                                        // The block is known shut. This is the answer; it streams.
                                        speech_tokens += std::mem::take(&mut charge);
                                        on_delta(t);
                                        text.push_str(t);
                                    } else {
                                        // Undecided. Held, and NOT rendered.
                                        held_tokens += std::mem::take(&mut charge);
                                        held.push_str(t);
                                    }
                                }
                            }
                        }
                        // A frame the splitter swallowed whole — a partial tag, buffered inside
                        // `feed` — still cost a token. It is owed to whichever channel resolves
                        // it, and `held` is where unresolved text waits.
                        held_tokens += charge;
                    }
                }
                if let Some(calls) = msg.get("tool_calls").and_then(|t| t.as_array()) {
                    tool_calls.extend(calls.iter().cloned());
                }
            }
            // Ollama sends the counters on the final frame only.
            if frame.get("done").and_then(|d| d.as_bool()) == Some(true) {
                usage.prompt_tokens =
                    frame.get("prompt_eval_count").and_then(|v| v.as_u64()).unwrap_or(0);
                usage.completion_tokens =
                    frame.get("eval_count").and_then(|v| v.as_u64()).unwrap_or(0);
                usage.wall_ms = frame
                    .get("total_duration")
                    .and_then(|v| v.as_u64())
                    .map(|ns| ns / 1_000_000)
                    .unwrap_or(0);
            }
        }

        // A fragment held back in case it grew into a tag was ordinary text after all.
        // A fragment held back in case it grew into a tag was ordinary text after all. It joins
        // the buffer rather than bypassing it — a tail that streamed straight to the surface would
        // be the one path where unresolved content still reached the response colour.
        // **Charged `0`, and that is the accounting working rather than a gap in it.** These
        // segments are the splitter's own buffer, and every frame that filled it was charged as
        // it arrived. `held_tokens` is where those charges are, and it is settled below.
        for seg in &splitter.finish().segments {
            match seg {
                crate::think::Segment::Reasoning(r) => on_reasoning(r, 0),
                crate::think::Segment::Speech(t) => {
                    if closed {
                        on_delta(t);
                        text.push_str(t);
                    } else {
                        held.push_str(t);
                    }
                }
            }
        }

        // The call is over, so the buffer can be resolved.
        //
        // A call ending in a tool call has **not answered** -- completion is the absence of an
        // action -- so anything it said along the way is narration and belongs with the thinking.
        // A call with no tag anywhere is one Ollama parsed for us: reasoning went to the
        // `thinking` field and the buffer is the reply.
        if !held.is_empty() {
            if tool_calls.is_empty() && (closed || !splitter.saw_any_tag()) {
                speech_tokens += std::mem::take(&mut held_tokens);
                on_delta(&held);
                text.push_str(&held);
            } else {
                reasoning_tokens += held_tokens;
                on_reasoning(&held, std::mem::take(&mut held_tokens));
            }
            held.clear();
        }
        // Nothing claimed the buffer, so nothing was rendered from it either. The tokens were
        // still generated, and the rule sends an unclaimed token to the thinking side.
        reasoning_tokens += std::mem::take(&mut held_tokens);

        // ── THE CORRECTION, FROM THE ENGINE'S OWN COUNT ─────────────────────────────────
        //
        // `eval_count` is what Ollama says it generated. `speech_tokens` is the answer, and a
        // content frame is exactly one token — measured. Everything else the engine counted and
        // did not stream is thinking: the delimiters the parser ate, and the whitespace tokens it
        // merged into a later frame.
        //
        // Emitted as a reasoning chunk with **no text**, so the receiver's rule stays "add what
        // you are given" and no second event has to exist for a number that arrives late. It is
        // skipped when this call produced no reasoning at all: a run with a stop token and
        // nothing else must not conjure a thinking block that never existed.
        if reasoning_tokens > 0 {
            let streamed = reasoning_tokens.saturating_add(speech_tokens);
            let owed = usage.completion_tokens.saturating_sub(streamed);
            if owed > 0 {
                on_reasoning("", owed);
            }
        }

        // Reassembled into the same shape the non-streaming path produced, so `parse_step` is
        // unchanged and the four loop-control tools keep their routing.
        let mut message = serde_json::json!({ "content": text });
        if !tool_calls.is_empty() {
            message["tool_calls"] = serde_json::Value::Array(tool_calls);
        }
        let step = parse_step(&message);
        Ok(ModelCall { usage, step })
    }
}

/// Four of the eleven tools are LOOP CONTROL, not tool-host executions.
///
/// `done`, `ask`, `remember` and `run` are `ModelStep` variants in ARCHITECTURE §3's match —
/// they end a run, escalate, request a memory write, and spawn a child. A provider that mapped
/// them to `ModelStep::ToolCall` would send them to the tool host, which has no executor for
/// any of them.
///
/// **This was found by running it, not by a test.** The first real end-to-end question was
/// answered correctly in two steps and then looped for 155 seconds: the model called `done`, the
/// tool host reported "no executor", the model saw a failure it could not interpret, and tried
/// again. The budget paused it — the backstop worked — but the loop should have ended at `done`.
/// Every unit test passed throughout, because each half was correct in isolation.
/// Loop-control steps, which are `ModelStep` variants rather than tool-host executions.
///
/// **`done` is not here any more.** A run ends when the model produces prose and calls no tool —
/// see `ModelStep`'s header. Keeping `done` as an accepted alias would have preserved the failure
/// it caused: a model that emits it inconsistently would end some turns and not others, which is
/// harder to diagnose than never ending them.
const CONTROL_TOOLS: [&str; 3] = ["ask", "remember", "run"];

/// Turn one Ollama message into a [`ModelStep`].
///
/// **No taint is produced here and there is nowhere for it to go** — `ModelStep::ToolCall`
/// carries no provenance field (ADR-023). What the model says about where its arguments came
/// from is not evidence, and the harness computes taint from the context view instead.
pub fn parse_step(message: &serde_json::Value) -> ModelStep {
    if let Some(calls) = message.get("tool_calls").and_then(|c| c.as_array()) {
        // **Every call, not `calls.first()`.**
        //
        // Taking the first and discarding the rest was silent: a model emitting three calls had
        // two actions it believed it took that never happened, with nothing reporting the loss to
        // the model, the user, the journal or a test. That was closed first by a loud refusal and
        // is now closed properly by executing all of them.
        //
        // Ids are assigned HERE, by the harness, and never read from the model's own `id` field.
        // A model that could name its own calls could collide two results deliberately, and a
        // batch of three `read`s is exactly where a collision would be unreadable.
        let mut batch: Vec<marlowe_loop::ToolInvocation> = Vec::new();
        for (i, call) in calls.iter().enumerate() {
            let function = call.get("function");
            let name = function
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or_default();
            if !name.is_empty() {
                let mut args = Args::new();
                if let Some(obj) = function.and_then(|f| f.get("arguments")).and_then(|a| a.as_object())
                {
                    for (k, v) in obj {
                        let value = match v {
                            serde_json::Value::String(s) => ArgValue::Text(s.clone()),
                            serde_json::Value::Number(n) if n.is_i64() => {
                                ArgValue::Integer(n.as_i64().unwrap_or(0))
                            }
                            serde_json::Value::Bool(b) => ArgValue::Boolean(*b),
                            other => ArgValue::Text(other.to_string()),
                        };
                        args = args.with(k.clone(), value);
                    }
                }
                // **A structured `done` must never reach the tool host.**
                //
                // The tool no longer exists, so without this it becomes an ordinary `ToolCall`
                // for a tool with no executor — which is EXACTLY the defect CLAUDE.md records
                // from M2's first real run: the model called `done`, the host reported no
                // executor, and the model spent 155 seconds trying to act on a failure it could
                // not interpret. Removing the tool reintroduced the failure by a new route, and
                // the provider tests caught it.
                //
                // A model naming `done` is answering. The body becomes the reply, and a reply
                // with no tool call ends the turn.
                //
                // **A control tool ends or reshapes the run, so it is never batched.** `done`
                // answers, `ask` escalates, `run` spawns. If one appears alongside others the
                // control step wins and the siblings are reported rather than dropped — the
                // reporting is what makes this different from `calls.first()`.
                if name == "done" {
                    return ModelStep::Say(done_body(&args, message));
                }
                if CONTROL_TOOLS.contains(&name) {
                    return control_step(name, &args, message);
                }
                batch.push(marlowe_loop::ToolInvocation {
                    id: format!("call_{}", i + 1),
                    tool: ToolId::new(name),
                    args,
                });
            }
        }
        if !batch.is_empty() {
            return ModelStep::ToolCall { calls: batch };
        }
    }
    let content = message.get("content").and_then(|c| c.as_str()).unwrap_or_default();

    // **A control tool named in plain text is still a control tool.**
    //
    // Observed live: `marlowe --ask "Hello marlowe"` produced **101 model calls** and exactly one
    // content frame — the word `done`. The model was ending the run correctly and emitting it as
    // prose rather than as a structured `tool_calls` entry, so `parse_step` fell through to
    // `Say("done")`, the loop pushed it to history and asked again, and the turn ran until the
    // token budget stopped it.
    //
    // This is CLAUDE.md's recorded `done`-routing defect in a second form. That one was the
    // adapter mapping a structured control call to the tool host; this is the adapter not seeing
    // an unstructured one at all. Both end the same way — a model doing the right thing while the
    // harness fails to notice.
    //
    // **The match is exact and trimmed, never a substring.** `content.contains("done")` would
    // swallow "I'm done looking at that" and end a turn mid-sentence, which is a worse failure
    // than the one being fixed: it would be rare, silent, and look like the model stopping for no
    // reason. A whole message that is one control word is unambiguous; anything else is prose.
    let bare = content.trim();
    if CONTROL_TOOLS.contains(&bare) {
        return control_step(bare, &Args::new(), message);
    }

    // ── recover tool calls the model leaked as XML into its prose ───────────────────
    //
    // qwen in think mode sometimes emits the call as literal markup instead of taking the
    // structured `tool_calls` path:
    //
    //     <function=read><parameter=path>notes.md</parameter></function>
    //
    // Left unrecovered this is worse than a missing call. The markup becomes assistant history,
    // goes back to Ollama on the next request, and its template fails to parse it — observed as
    // `HTTP 500: XML syntax error on line 4: element <function> closed by </parameter>`. The
    // model then retries, and the run spends its budget on a call that never happened.
    if let Some(step) = recover_leaked_call(content) {
        return step;
    }

    // ── and defang whatever `recover_leaked_call` DECLINED ──────────────────────────
    //
    // The comment above says the failure exactly: unrecovered markup "becomes assistant history,
    // goes back to Ollama on the next request, and its template fails to parse it". Recovery is
    // deliberately narrow — it matches `<function=NAME>` and nothing else — so a leak in any other
    // dialect falls straight through to `Say` and wedges the NEXT request.
    //
    // **Observed 2026-08-25 in a live TUI turn:** the model leaked a BARE `<function>` — no `=`,
    // no inline name — `find("<function=")` declined it, and the turn after failed with
    // `HTTP 500: XML syntax error on line 2: element <function> closed by </parameter>`. One token
    // shape outside the parser, and the run died a turn later pointing at the wrong place.
    //
    // So this is the layer that does not need to know the dialect. Recovery stays narrow, because
    // a looser parser inventing calls out of prose is the worse failure. What changes is that
    // markup nobody could parse is no longer allowed to reach the provider as tags.
    ModelStep::Say(defang_tool_markup(content))
}

/// Make tool-call markup unparseable as XML while leaving it readable.
///
/// **Not a sanitizer and not a filter.** It rewrites four exact token shapes and nothing else, so
/// prose survives unchanged and a leak survives *visibly* — the model can still see what it wrote,
/// which is what lets it notice and retry structurally.
///
/// No harness prose is added. Appending "your call did not parse" here would put words in
/// Marlowe's voice on a `Say`, which ADR-030 forbids and which the comment at `control_step`'s
/// `run` arm already records as a defect this file paid for once.
fn defang_tool_markup(content: &str) -> String {
    if !content.contains("<function") && !content.contains("<parameter") {
        return content.to_string();
    }
    content
        .replace("</function>", "[/function]")
        .replace("</parameter>", "[/parameter]")
        .replace("<function", "[function")
        .replace("<parameter", "[parameter")
}

/// Map a control tool onto its `ModelStep`. §12: the adapter normalizes what the model speaks.
fn control_step(name: &str, args: &Args, message: &serde_json::Value) -> ModelStep {
    let text = |key: &str| args.get(key).and_then(ArgValue::as_text).map(str::to_string);
    // A model may name the field `result`, `answer`, `content` or nothing at all. Falling back to
    // the message's own prose is what stops a well-formed `done` with an unexpected field name
    // from becoming a contract violation the model then has to guess its way out of.
    let body = text("result")
        .or_else(|| text("answer"))
        .or_else(|| text("content"))
        .or_else(|| text("text"))
        .or_else(|| {
            message
                .get("content")
                .and_then(|c| c.as_str())
                .filter(|s| !s.trim().is_empty())
                .map(str::to_string)
        })
        .unwrap_or_default();

    match name {
        // **`done` deliberately falls through to `Say`.** A model that still names it is
        // answering; the loop ends on prose-with-no-tool-call, so saying the body IS ending.
        // Mapping it to a control step would resurrect the token this design removed.
        "done" => ModelStep::Say(body),
        "ask" => ModelStep::Ask(text("question").unwrap_or(body)),
        "remember" => ModelStep::MemoryWrite(ClaimRequest {
            text: text("text").unwrap_or(body),
            payload_kind: text("payload_kind").unwrap_or_else(|| "fact".into()),
            derived_from: text("derived_from").into_iter().collect(),
        }),
        // **`run` SPAWNS, as of ADR-057.** For the whole of M2 this arm routed the call to a tool
        // host with no `run` executor so the model would read an ordinary refusal — the same route
        // every unbuilt tool takes, and it was the right holding pattern. What it was waiting for
        // was never plumbing: `Engine::spawn` has been complete since M2 Session A. It was waiting
        // for a decision about **who declares the six fields of the contract the model does not
        // supply**, because M3-DESIGN §5 requires them declared at spawn and never inferred.
        //
        // ADR-057 is that decision, and the mapping lives in `SpawnRequest::from_args` rather than
        // here. The adapter's job is to recognise that the model named `run`; what a `run` call
        // *means* belongs next to the type it produces and the engine that enforces it, or a
        // second provider would write a second answer to the same question.
        //
        // **Nothing is refused here**, and that is deliberate. An empty task, a tool the parent
        // does not hold, a grant larger than the pool — each is refused by `Engine::spawn`, by
        // name, into the model's own window. A refusal invented in the adapter would be a second
        // gate that could disagree with the first.
        "run" => ModelStep::Spawn(marlowe_loop::SpawnRequest::from_args(args)),
        _ => ModelStep::Say(body),
    }
}

#[cfg(test)]
mod tests {
    /// **THE REGRESSION FOR THE TURN THAT DIED A TURN LATE.**
    ///
    /// A bare `<function>` — no `=`, no inline name — is a dialect `recover_leaked_call` declines
    /// by design. Before this, it fell through to `Say`, entered assistant history verbatim, and
    /// the NEXT request came back `HTTP 500: XML syntax error ... element <function> closed by
    /// </parameter>`. Observed live 2026-08-25.
    #[test]
    fn markup_the_recovery_declined_cannot_reach_the_provider_as_tags() {
        let leaked = "I'll read it.
<function>read</function>
<parameter=path>x.rs</parameter>";
        // The control: recovery genuinely declines this shape, so the defang is what is being
        // tested rather than a path that never runs.
        assert!(
            super::recover_leaked_call(leaked).is_none(),
            "recovery now handles this dialect -- rewrite this test against one it still declines"
        );

        // **THROUGH `parse_step`, NOT THROUGH THE HELPER.** An earlier version of this called
        // `defang_tool_markup` directly and passed on a build where `parse_step` never called it --
        // the mutation proved it. What matters is the fate of the bytes on the way to the provider,
        // so the assertion has to start where the provider's message starts.
        let out = match super::parse_step(&serde_json::json!({ "content": leaked })) {
            ModelStep::Say(s) => s,
            other => panic!("a declined leak must become prose, not {other:?}"),
        };
        for tag in ["<function", "</function>", "<parameter", "</parameter>"] {
            assert!(!out.contains(tag), "`{tag}` survived and will wedge the next request:
{out}");
        }
        // Readable, not erased: the model must still be able to see what it wrote.
        assert!(out.contains("read"), "the call's content was destroyed:
{out}");
        assert!(out.contains("x.rs"), "the argument was destroyed:
{out}");
        assert!(out.contains("I'll read it."), "surrounding prose was damaged:
{out}");
    }

    /// **Prose is untouched.** The defang rewrites four token shapes; anything else must survive
    /// byte for byte, or ordinary answers start arriving mangled.
    #[test]
    fn ordinary_prose_passes_through_the_defang_unchanged() {
        for s in [
            "The function reads a file.",
            "Use a < b and c > d in your comparison.",
            "See `parameter` in the docs.",
            "",
        ] {
            assert_eq!(super::defang_tool_markup(s), s, "prose was rewritten: {s:?}");
        }
    }

    /// The defang must not steal from the recovery: a call in the dialect recovery DOES understand
    /// still becomes a real call, not defanged text.
    #[test]
    fn a_recoverable_call_is_still_recovered_and_never_defanged() {
        let good = "<function=read><parameter=path>notes.md</parameter></function>";
        assert!(
            super::recover_leaked_call(good).is_some(),
            "the narrow recovery stopped working, which the defang would silently paper over"
        );
    }

    use super::*;

    #[test]
    fn the_recorded_capability_describes_the_model_that_is_actually_default() {
        // A build that changed DEFAULT_MODEL and not `default_capability` would disclose one
        // model's measured rate under another model's name — a number quoted about the wrong
        // subject, which is the family this project has logged fourteen times.
        let c = default_capability();
        assert_eq!(c.model, DEFAULT_MODEL);
        assert!(c.fit_for_default(), "the pinned default must clear its own bar: {}", c.disclosure());
        assert!(c.disclosure().contains("12/12"), "{}", c.disclosure());
    }

    #[test]
    fn an_absent_endpoint_degrades_with_a_remedy_rather_than_crashing() {
        // Invariant 4, exercised against a port nothing listens on. The assertion is not that it
        // fails — it is that it fails into a DECLARED state carrying the command that fixes it.
        let endpoint = LocalEndpoint::new("127.0.0.1", 1).unwrap();
        let routing = Routing::uniform(DEFAULT_MODEL).unwrap();
        let a = Availability::probe(&endpoint, &routing);

        assert!(matches!(a, Availability::EndpointDown { .. }), "{a:?}");
        assert_eq!(a.degraded_path(), Some(DegradedPath::ModelUnavailable));
        assert!(a.remedy().contains("ollama serve"), "the remedy must be actionable: {}", a.remedy());
    }

    #[test]
    fn each_unavailability_names_a_different_remedy() {
        // Three distinguishable failures, because the fixes differ. Collapsing them into
        // "unavailable" would be a message the user cannot act on.
        let down = Availability::EndpointDown { endpoint: "http://127.0.0.1:11434".into() };
        let missing = Availability::ModelMissing {
            model: "qwen3.5:9b".into(),
            available: vec!["gemma3:12b".into()],
        };
        let wrong = Availability::NotOllama {
            endpoint: "http://127.0.0.1:11434".into(),
            detail: "HTTP 404".into(),
        };
        assert!(down.remedy().contains("ollama serve"));
        assert!(missing.remedy().contains("ollama pull qwen3.5:9b"));
        assert!(missing.remedy().contains("gemma3:12b"), "say what IS available");
        assert!(wrong.remedy().contains("not Ollama"));
        for a in [&down, &missing, &wrong] {
            assert_eq!(a.degraded_path(), Some(DegradedPath::ModelUnavailable));
        }
    }

    #[test]
    fn a_tool_call_parses_into_a_step_with_no_taint_field_to_fill() {
        let msg = serde_json::json!({
            "content": "",
            "tool_calls": [{
                "function": { "name": "read", "arguments": { "path": "src/main.rs" } }
            }]
        });
        match parse_step(&msg) {
            ModelStep::ToolCall { calls } => {
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].tool.as_str(), "read");
                assert_eq!(calls[0].args.get("path"), Some(&ArgValue::Text("src/main.rs".into())));
            }
            other => panic!("expected a tool call, got {other:?}"),
        }
    }

    #[test]
    fn prose_parses_as_say() {
        let msg = serde_json::json!({ "content": "Marlowe here." });
        assert_eq!(parse_step(&msg), ModelStep::Say("Marlowe here.".into()));
    }

    #[test]
    fn the_control_tools_become_loop_steps_not_tool_calls() {
        // The defect the first real end-to-end run exposed: control tools routed to the tool
        // host, which has no executor for them.
        //
        // `done` is no longer among them. M2 C2e made completion the absence of an action, so a
        // model that still names `done` is answering — and the body it named becomes the reply
        // that ends the turn. Mapping it back to a control step would resurrect the token the
        // design removed.
        let done = serde_json::json!({
            "content": "",
            "tool_calls": [{ "function": { "name": "done", "arguments": { "result": "42" } } }]
        });
        match parse_step(&done) {
            ModelStep::Say(t) => assert_eq!(t, "42"),
            other => panic!("`done` must become the reply that ends the turn, got {other:?}"),
        }

        let ask = serde_json::json!({
            "content": "",
            "tool_calls": [{ "function": { "name": "ask", "arguments": { "question": "which?" } } }]
        });
        assert_eq!(parse_step(&ask), ModelStep::Ask("which?".into()));

        let remember = serde_json::json!({
            "content": "",
            "tool_calls": [{ "function": { "name": "remember", "arguments": { "text": "x" } } }]
        });
        assert!(matches!(parse_step(&remember), ModelStep::MemoryWrite(_)));

        // ...and a real tool still routes to the tool host.
        let read = serde_json::json!({
            "content": "",
            "tool_calls": [{ "function": { "name": "read", "arguments": { "path": "a" } } }]
        });
        assert!(matches!(parse_step(&read), ModelStep::ToolCall { .. }));
    }

    #[test]
    fn done_with_an_unexpected_field_name_still_ends_the_run() {
        // A model that says `content` instead of `result` has still finished. Turning that into
        // a contract violation would make it guess its way out, which is the loop this defect
        // produced in the first place.
        let done = serde_json::json!({
            "content": "the answer",
            "tool_calls": [{ "function": { "name": "done", "arguments": {} } }]
        });
        match parse_step(&done) {
            ModelStep::Say(t) => assert_eq!(t, "the answer"),
            other => panic!("expected the reply that ends the turn, got {other:?}"),
        }
    }

    #[test]
    fn a_malformed_tool_call_falls_back_to_prose_rather_than_inventing_a_call() {
        // A model that emits `tool_calls: [{}]` has failed to call a tool. Synthesising a call
        // from the fragments would turn a model failure into a harness action.
        let msg = serde_json::json!({ "content": "hm", "tool_calls": [{ "function": {} }] });
        assert_eq!(parse_step(&msg), ModelStep::Say("hm".into()));
    }
}

#[cfg(test)]
mod control_text_tests {
    use super::*;

    /// The 101-call loop, as a test — and the reason it can no longer happen.
    #[test]
    fn a_bare_done_is_just_a_reply_and_a_reply_ends_the_turn() {
        // `--ask "Hello marlowe"` produced 100+ model calls twice: once the model emitted `done`
        // as prose the adapter did not recognise, once it never emitted it at all. Recognising
        // the word was the narrow fix; removing the requirement was the real one.
        //
        // A bare `done` is now a `Say`, and a `Say` with no tool call ends the turn — the same
        // outcome, by a mechanism the model cannot forget to trigger.
        assert!(matches!(parse_step(&serde_json::json!({ "content": "done" })), ModelStep::Say(_)));
        assert!(matches!(
            parse_step(&serde_json::json!({ "content": "anything at all" })),
            ModelStep::Say(_)
        ));
    }

    /// The failure the exact match exists to avoid, which would be worse than the one it fixes.
    #[test]
    fn prose_that_merely_mentions_a_control_word_stays_prose() {
        for text in [
            "I'm done looking at that.",
            "done: the tests pass",
            "Ask me anything.",
            "That run is done.",
        ] {
            let step = parse_step(&serde_json::json!({ "content": text }));
            assert!(
                matches!(step, ModelStep::Say(_)),
                "{text:?} was routed as control. A substring match would end turns mid-sentence — \
                 rare, silent, and indistinguishable from the model stopping for no reason."
            );
        }
    }

    /// A structured call still wins; this is a fallback, not a replacement.
    #[test]
    fn a_structured_tool_call_is_unaffected() {
        let msg = serde_json::json!({
            "content": "done",
            "tool_calls": [{ "function": { "name": "read", "arguments": { "path": "a.md" } } }]
        });
        assert!(matches!(parse_step(&msg), ModelStep::ToolCall { .. }));
    }
}

/// Parse a tool call the model wrote as markup rather than emitting structurally.
///
/// Deliberately narrow: it matches `<function=NAME>` with `<parameter=KEY>VALUE</parameter>`
/// children and nothing else. A looser parser here would start interpreting prose about tools as
/// calls, which is the failure mode that cannot be debugged from a transcript.
fn recover_leaked_call(content: &str) -> Option<ModelStep> {
    let start = content.find("<function=")?;
    let rest = &content[start + "<function=".len()..];
    let name_end = rest.find('>')?;
    let name = rest[..name_end].trim();
    if name.is_empty() {
        return None;
    }
    let body_end = rest.find("</function>").unwrap_or(rest.len());
    let body = &rest[name_end + 1..body_end];

    let mut args = Args::new();
    let mut cursor = body;
    while let Some(p) = cursor.find("<parameter=") {
        // LOOP-EXEMPT: scanning one string, not a driving loop.
        let after = &cursor[p + "<parameter=".len()..];
        let Some(key_end) = after.find('>') else { break };
        let key = after[..key_end].trim().to_string();
        let value_region = &after[key_end + 1..];
        let value_end = value_region.find("</parameter>").unwrap_or(value_region.len());
        let value = value_region[..value_end].trim().to_string();
        if !key.is_empty() {
            args = args.with(
                key,
                match value.parse::<i64>() {
                    Ok(n) => ArgValue::Integer(n),
                    Err(_) => ArgValue::Text(value),
                },
            );
        }
        cursor = &value_region[value_end.min(value_region.len())..];
        if cursor.is_empty() {
            break;
        }
        cursor = &cursor[cursor.find("</parameter>").map(|i| i + "</parameter>".len()).unwrap_or(0)..];
    }

    if CONTROL_TOOLS.contains(&name) {
        return Some(control_step(name, &args, &serde_json::Value::Null));
    }
    // A recovered leaked call is always a single call — the syntax cannot express a batch.
    Some(ModelStep::one_call(ToolId::new(name), args))
}

#[cfg(test)]
mod leaked_call_tests {
    use super::*;

    #[test]
    fn a_tool_call_written_as_markup_is_recovered() {
        let msg = serde_json::json!({
            "content": "<function=read><parameter=path>notes.md</parameter></function>"
        });
        match parse_step(&msg) {
            ModelStep::ToolCall { calls } => {
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].tool.as_str(), "read");
                assert_eq!(
                    calls[0].args.get("path").and_then(ArgValue::as_text),
                    Some("notes.md")
                );
            }
            other => panic!("leaked markup was not recovered: {other:?}"),
        }
    }

    #[test]
    fn prose_about_tools_is_not_a_tool_call() {
        // The parser is narrow on purpose. A looser one would read this as a call, and a
        // hallucinated tool invocation is not debuggable from a transcript.
        for text in [
            "I could use the read function on notes.md.",
            "The <function> element is XML.",
            "read(path=notes.md)",
        ] {
            let step = parse_step(&serde_json::json!({ "content": text }));
            assert!(matches!(step, ModelStep::Say(_)), "{text:?} was read as a call");
        }
    }
}

/// The prose a model attached to a `done` call, under whichever field name it chose.
///
/// It may name the field `result`, `answer`, `content`, or nothing at all and put the text in the
/// message body. Falling back through all of them is what stops a well-formed finish from
/// becoming an empty reply the model then has to guess its way out of.
fn done_body(args: &Args, message: &serde_json::Value) -> String {
    let text = |key: &str| args.get(key).and_then(ArgValue::as_text).map(str::to_string);
    text("result")
        .or_else(|| text("answer"))
        .or_else(|| text("content"))
        .or_else(|| text("text"))
        .or_else(|| {
            message
                .get("content")
                .and_then(|c| c.as_str())
                .filter(|s| !s.trim().is_empty())
                .map(str::to_string)
        })
        .unwrap_or_default()
}

