//! The model call, and everything the loop needs from the world outside it.
//!
//! The loop is a state machine over injected ports. That is what makes "one loop, many
//! capability profiles" testable: a scripted driver and a real provider client are the same
//! shape, and consolidation is a profile rather than a second `while`.
//!
//! **No provider client lives here.** M2's later sessions add one; this file pins what it must
//! satisfy, including the part a provider client is most likely to get wrong —
//! [`CallLimits::max_output_tokens`] is a **hard cap that must be passed to the provider**, not
//! a hint the harness checks afterwards. See `budget`.

use marlowe_contract::escalation::{
    ArtifactHandle, EscalationCategory, EscalationSeverity, OptionLabel, ValidatedSentence,
};
use marlowe_contract::EscalationId;
use marlowe_permission::{ArgValue, Args, BlastRadius};
use marlowe_tools::{ExposedSet, ResultSummary, ToolId};
use serde::{Deserialize, Serialize};

use crate::budget::{Budget, BudgetShare, CallLimits};
use crate::escalation::{EscalationRefused, EscalationRoute};
use crate::profile::{Disposition, ModelRoute};
use crate::context::ContextView;
use crate::run::{OrphanPolicy, OutputContract, RunId};
use crate::turn::TurnEvent;

/// What one model call cost. Folded into the run's `spent` immediately after the call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub micros_usd: u64,
    pub wall_ms: u64,
}

impl Usage {
    pub fn as_budget(&self) -> Budget {
        Budget {
            tokens: self.prompt_tokens.saturating_add(self.completion_tokens),
            wall_ms: self.wall_ms,
            tool_calls: 0,
            subagents: 0,
            depth: 0,
            micros_usd: self.micros_usd,
        }
    }
}

/// The model's request to write a belief.
///
/// **A request, not a write.** CONTRACTS §3.5 names the hazard: the model-visible tool is
/// called `remember`, which reads like a write, and it is not. This type is deliberately not
/// §3.5's `Claim` — it is what the *model* supplies, and the harness turns it into a `Claim`
/// after resolving derivation, computing effective trust, signing and appending. Naming them
/// the same type is how a bypass "for performance" gets written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimRequest {
    pub text: String,
    pub payload_kind: String,
    pub derived_from: Vec<String>,
}

/// §10.1: ad-hoc spawning, no predeclared graph. Every field here is **declared at spawn**.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnRequest {
    /// A self-contained brief. The child gets a fresh window and does not know its siblings
    /// exist, so anything not in here is not available to it.
    pub task: String,
    pub contract: OutputContract,
    /// Declared, never inferred. Recorded and unused at M2; M3 is what makes it mean something.
    pub orphan: OrphanPolicy,
    pub share: BudgetShare,
    /// An **explicit token grant**, deducted from the parent's pool. `None` means *decide for me*
    /// and takes `share` of the parent's original budget.
    ///
    /// M3-DESIGN §4: *"A master with 200k spends it or hands it down."* A parent that knows the
    /// size of the job says so; `Budget::grant` refuses by name, with both numbers, if the amount
    /// exceeds what remains.
    pub grant_tokens: Option<u64>,
    /// A **narrowing** of the parent's set. There is no widening path.
    pub tools: Vec<ToolId>,
    /// **Whether the model actually SAID which tools the child gets.**
    ///
    /// `tools` alone cannot answer it: an omitted `exposed_tools` and a deliberate empty one both
    /// arrive as an empty vec, and they mean opposite things -- "I did not think about it" versus
    /// "this child reasons from its task and needs nothing".
    ///
    /// ADR-057 defaulted it to empty. Watched live 2026-08-26 that produced a child with no tools,
    /// asked to summarise a tool it had no way to look up: 12,332 tokens of reasoning, no result.
    /// ADR-057's own argument is why the default was wrong -- it justified defaults as *"a fixed
    /// constant that does not vary with the task"*, and a child's tool set varies with the task by
    /// definition. Section 5 says **declared at spawn, never inferred**.
    pub tools_declared: bool,
    /// Sets the quarantined-reader profile, which forces the tool set empty. A spawn asking
    /// for both is a load-time error — see `profile`.
    pub reads_untrusted: bool,
    /// **Which model tier serves the child, and it is a TARGET under ADR-023.**
    ///
    /// Read by `Engine::spawn`, which hands it to `CapabilityProfile::new` in place of the
    /// `ModelRoute::Worker` that used to be hardcoded there; from the child's profile it reaches
    /// `Budget::call_limits`, `CallLimits::route`, and `OllamaDriver::request_body`'s
    /// `routing.model_for(..)` -- the `"model"` field of the bytes that go out. Also read by
    /// `composes_spawn_targets`, which is what makes it a Target in practice rather than in a
    /// manifest.
    ///
    /// Default `Worker`: the **cheap** end, so a model that forgets the field costs nothing and
    /// the receipt tells it what it got.
    ///
    /// **The ladder is a rendering of this enum, not a second type.** `DECISIONS.md`'s
    /// 2026-08-30 entry names four tiers -- Secretary, Agent-High, Agent-Medium, Agent-Low --
    /// and `ModelRoute` has three columns; which tier fills which column is the Agent
    /// Registration Window's, and the human's. `from_args` therefore accepts only this enum's
    /// own three spellings: inventing `high`/`medium`/`low` here would be an implementer
    /// choosing a table that is not his to choose, and a word nobody chose is how a default
    /// becomes a decision.
    pub role: ModelRoute,
    /// **§1.1's one question -- *can one agent do this alone?* -- and it is a TARGET.**
    ///
    /// Read by `AgentLevel::child_of` in `Engine::spawn`, **whose two arms return different
    /// values**; the level that comes back is what `CapabilityProfile::new` refuses `run` at.
    /// So a `Work` child cannot hold the create grant, and that is enforced by construction
    /// rather than by a second check somewhere.
    ///
    /// Default `Work`: the narrow end.
    pub disposition: Disposition,
}

/// The default contract description, when the parent names none. ADR-057 §1.
pub const DEFAULT_SPAWN_CONTRACT: &str = "what you found";

impl SpawnRequest {
    /// Build a spawn from the arguments a model supplied to `run`. **ADR-057 §1.**
    ///
    /// # Why this lives in the loop and not in the provider adapter
    ///
    /// `SpawnRequest` is defined here and `Engine::spawn` enforces here, so a second provider
    /// that wrote its own mapping would be a second definition of the contract — the
    /// two-sides-silently-disagree shape this project keeps recording. The adapter's job is to
    /// recognise that the model named `run`; deciding what a `run` call *means* is the loop's.
    ///
    /// # It is total, and that is the decision rather than an omission
    ///
    /// Every field has a fixed rule (ADR-057 §1) and an unrecognised value takes it. The rule is
    /// safe because it is the conservative end of each field **and** because `Engine::spawn`
    /// echoes what it granted into the parent's window — a default a model cannot see is the
    /// "defaults that make a mismatch unobservable" family, and the receipt is what closes it.
    ///
    /// `share` and `reads_untrusted` are not read from `args` at all. ADR-057 §5: they are
    /// withheld structurally, so a model cannot reach them by naming them.
    pub fn from_args(args: &Args) -> Self {
        let text = |k: &str| args.get(k).and_then(ArgValue::as_text).unwrap_or_default().trim();

        let description = {
            let d = text("output_contract");
            if d.is_empty() { DEFAULT_SPAWN_CONTRACT.to_string() } else { d.to_string() }
        };

        SpawnRequest {
            task: text("task").to_string(),
            contract: OutputContract::new(description, &["findings"]),
            orphan: parse_orphan(text("orphan_policy")),
            // ADR-057 §5. Not `args`-derived, and deliberately not a parameter.
            share: BudgetShare::Standard,
            grant_tokens: parse_grant(args),
            tools: parse_tools(text("exposed_tools")),
            // Presence, not content: an empty string is a declaration, an absent key is not.
            tools_declared: args.get("exposed_tools").is_some(),
            // ADR-057 §5. Layer 1 decides what is quarantined; a model does not ask to be.
            reads_untrusted: false,
            role: parse_role(text("role")),
            disposition: parse_kind(text("kind")),
        }
    }
}

/// The model tier a spawner named. **Total, with the cheap end as the default.**
///
/// Only `ModelRoute`'s own three spellings are recognised. A word this does not know -- a
/// misspelling, or one of `DECISIONS.md`'s tier names, `high` / `medium` / `low`, whose mapping
/// onto these three columns is the human's -- takes `Worker`, and `Engine::spawn`'s receipt says
/// so in the parent's window. That receipt is what makes a total default legitimate rather than
/// silent: the "defaults that make a mismatch unobservable" family is answered by saying what
/// was granted, not by refusing a model that guessed.
fn parse_role(s: &str) -> ModelRoute {
    match s.trim().to_ascii_lowercase().as_str() {
        "orchestrator" => ModelRoute::Orchestrator,
        "summarizer" => ModelRoute::Summarizer,
        _ => ModelRoute::Worker,
    }
}

/// M3-DESIGN §1.1's `master | worker`. **Total, with the narrow end as the default.**
///
/// `master` is the only word that buys anything, and it buys a create grant -- so an unrecognised
/// value must land on `Work`, never on `Manage`.
fn parse_kind(s: &str) -> Disposition {
    match s.trim().to_ascii_lowercase().as_str() {
        "master" | "manage" | "manager" => Disposition::Manage,
        _ => Disposition::Work,
    }
}

/// **Two words, not three.** `OrphanPolicy::Adopt { by }` names a run id, the model has no way to
/// name one, and a policy whose argument cannot be supplied cannot be declared — so the word is
/// not accepted rather than accepted and quietly turned into something else. ADR-057 §2.
fn parse_orphan(s: &str) -> OrphanPolicy {
    match s.trim().to_ascii_lowercase().as_str() {
        "detach" | "detached" => OrphanPolicy::Detach,
        _ => OrphanPolicy::Terminate,
    }
}

/// An explicit token grant. **Zero is not a grant of zero — it is no grant at all.**
///
/// `Budget::exhausted` compares `spent >= budget`, so a child granted zero tokens pauses before its
/// first model call and returns nothing, and the parent reads a failure whose stated reason is not
/// the real one. That is instance #17, in the one function where a model can type the number.
/// A model that means "as little as possible" is answered by the default share, which
/// `Budget::grant` floors at 1 while the parent still has any.
fn parse_grant(args: &Args) -> Option<u64> {
    let n = match args.get("budget_tokens")? {
        ArgValue::Integer(n) => *n,
        ArgValue::Amount(a) => *a as i64,
        ArgValue::Text(s) => s.trim().parse::<i64>().ok()?,
        ArgValue::Boolean(_) => return None,
    };
    if n <= 0 {
        return None;
    }
    Some(n as u64)
}

/// A tool list as a model actually writes it: `read, grep`, `["read","grep"]`, `read grep`.
///
/// **Nothing here decides whether a tool may be given away.** `Engine::spawn` checks every id
/// against the parent's exposed set and refuses by name; this only turns a string into ids. A
/// filter here would be a second gate that could silently disagree with the first.
fn parse_tools(s: &str) -> Vec<ToolId> {
    s.split([',', ' ', '\t', '\n', '[', ']', '"', '\''])
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(ToolId::new)
        .collect()
}

/// One call inside a [`ModelStep::ToolCall`] batch.
///
/// **`id` is assigned by the HARNESS, never read from the model.** It is what a result is
/// attributed to: `/api/chat` carries `tool_calls[].id` on the assistant message and
/// `tool_call_id` on each result, and without it a batch of three calls to the same tool comes
/// back as three results the model cannot tell apart — which is the failure that makes a partial
/// batch failure unreadable. Taking the model's own id would let it collide two results
/// deliberately.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolInvocation {
    pub id: String,
    pub tool: ToolId,
    pub args: Args,
}

/// ARCHITECTURE §3's `match step`. Closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelStep {
    /// Prose for the user. Streams to the surface as `TurnEvent::TextDelta`.
    Say(String),
    /// One or more tool calls the model emitted **in a single message**.
    ///
    /// **No taint field.** Provenance is computed by the harness from the context view; a
    /// model that could label its own arguments would be the security boundary.
    ///
    /// # Why a batch is one step rather than several
    ///
    /// The model emitted every call in this vector **before seeing any of their results**. That is
    /// not an implementation detail — it is what makes computing taint **once for the batch**
    /// correct rather than a shortcut. No call here can have been shaped by another call's output,
    /// because none of that output existed when the model composed them.
    ///
    /// It also makes the batch the right unit for the latch: the results all enter the view
    /// together, the floor latches from the **worst** of them at the top of the next iteration, and
    /// the next batch is adjudicated against the lowered floor. See `Engine::tool_batch`.
    ToolCall { calls: Vec<ToolInvocation> },
    MemoryWrite(ClaimRequest),
    Spawn(SpawnRequest),
    /// **`ask`: the conversational run puts a question to the user and waits.** The run does not
    /// hold a channel open; it pauses and resumes on an answer.
    ///
    /// The line above used to read *"escalate with a decision package"*, which is now the
    /// neighbouring variant's job and was never this one's.
    ///
    /// **Unchanged and unrepurposed.** M3-DESIGN §3.1's *"a worker can never address Marlowe"* is
    /// enforced against this variant in `Engine`'s arm, which refuses it from any run whose
    /// [`crate::profile::AgentLevel`] is not `Secretary`. It is not the escalation channel and
    /// must not become one: an agent's question is a typed record, not a paragraph.
    Ask(String),
    /// M3-DESIGN §3's typed upward record. **No free paragraph, and no recipient.**
    ///
    /// The recipient is `crate::escalation::escalation_route`, which reads the run tree. A model
    /// that could name its own recipient would be the routing decision.
    Escalate(EscalationRequest),
}

/// What a model may supply when it raises. **The recipient is not here.**
///
/// Every field names the function that reads it, or it is not here — CLAUDE.md instance #16. The
/// table is in ADR-065 §3; the two that are easiest to get wrong:
///
/// * `severity` is read by the desk (stored on the record), by `Notice::EscalationRaised`'s
///   producer and by the overlay's header row. **It gates nothing.** A model inflating every
///   escalation to `Critical` costs attention and no capability.
/// * `sentence` is read by `EscalationDesk::raise`, which stores it, and by
///   `EscalationDesk::view_for`, which puts it on `EscalationView::sentence` — the overlay's
///   header third row, through `chrome::prepare_model_text`. **It is NOT refused by an A8 arm
///   here, and ADR-065's table says it is.** §9.1's 2026-08-30 amendment moved where A8's arms
///   vary to `Engine::spawn`'s note match, which this change does not touch; a refusal written
///   here would be a second place the arm is consulted. The field has a reader either way, which
///   is what #16 asks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EscalationRequest {
    pub severity: EscalationSeverity,
    pub category: EscalationCategory,
    /// §3.4's attacker-controlled half. Bounded and normalised at construction — each label is an
    /// [`OptionLabel`], which cannot contribute a line or move the cursor.
    ///
    /// **The harness's own TERMINATE row is not in here and there is nowhere to put it.** That is
    /// §3.4's structural invisibility expressed as an absence rather than as a flag: a producer
    /// cannot supply it, suppress it, label it, style it or reorder it, because this type has no
    /// field for any of those.
    pub options: Vec<OptionLabel>,
    /// A journal-addressed handle the surface turns into a path. **Not a `ContentRef`** — that
    /// type carries a summary, and a summary is attacker prose crossing upward inside the record
    /// §2.3 calls typed.
    pub artifact: Option<ArtifactHandle>,
    /// M3-DESIGN §9.1 A8's middle arm only.
    pub sentence: Option<ValidatedSentence>,
}

/// How an escalation leaves the loop.
///
/// **This is the seam, and its absence is what made the first design unbuildable.** `Ports` had no
/// escalation member and `marlowe-loop` cannot see `marlowe-daemon`, so a `ModelStep::Escalate`
/// arm had a record and no destination — two individually coherent halves and nothing between
/// them, which is the shape of the `done`-to-tool-host failure CLAUDE.md records.
///
/// `route` is passed rather than recomputed: the loop is the only thing holding the `Run`, and a
/// desk that derived the route from a `RunId` it looked up would be a second answer to *"who is
/// above this run"*.
pub trait EscalationPort {
    fn raise(
        &mut self,
        raised_by: RunId,
        route: EscalationRoute,
        req: EscalationRequest,
    ) -> Result<EscalationId, EscalationRefused>;
}

// **There is no `NoEscalation` null object, and ADR-065 §2.3 proposed one.**
//
// `Ports::escalations` is `Option<&mut dyn EscalationPort>`, mirroring `Ports::memory` — the field
// beside it, and the one that is `None` for exactly the run this design must refuse. A
// `NoEscalation` returning `Err(NoDesk)` would then have no caller anywhere in the workspace,
// which is instance #16 committed inside the fix for the seam. `None` is the same fail-closed
// answer with one definition instead of two, and `Engine`'s arm reads it in the same shape the
// `MemoryWrite` arm twelve lines away already uses.

impl ModelStep {
    /// The single-call case, which is most of them. Exists so a batch of one does not have to be
    /// spelled out at every construction site, and so there is **one** representation of a tool
    /// call rather than a scalar variant and a vector variant that drift.
    pub fn one_call(tool: ToolId, args: Args) -> Self {
        ModelStep::ToolCall {
            calls: vec![ToolInvocation { id: "call_1".to_string(), tool, args }],
        }
    }
}

// **There is no `Done` variant, and that is M2 C2e's correction.**
//
// A run used to end only when the model emitted `done`. Measured against the shipped model, that
// is not a contract a small model honours: `marlowe --ask "Hello marlowe"` produced **100 model
// calls** and stopped at the token budget, twice, by two different routes — once emitting `done`
// as plain prose that the adapter did not recognise, once never emitting it at all.
//
// **Completion is now the ABSENCE of an action.** A turn that produces prose and calls no tool is
// finished; that is what "the model answered you" means, and it needs nothing from the model
// except the answer it was already giving. `done` asked a 9B model to remember a control token in
// order for the loop to stop, and made forgetting it indistinguishable from working.
//
// `ask`, `remember` and `run` remain control steps: each *does* something beyond ending, so
// naming it is the only way to express it.

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{detail}")]
pub struct ProviderError {
    pub detail: String,
    /// Whether failover to another provider could plausibly help. A non-retriable error fails
    /// the run loudly rather than cycling through providers that will all refuse.
    pub retriable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelCall {
    pub usage: Usage,
    pub step: ModelStep,
}

/// The provider port.
pub trait ModelDriver {
    fn call(
        &mut self,
        view: &ContextView,
        tools: &ExposedSet,
        limits: CallLimits,
    ) -> Result<ModelCall, ProviderError>;

    /// The same call, handing each text chunk to `on_delta` **as it arrives**.
    ///
    /// # Why this is a second method with a default rather than a change to `call`
    ///
    /// `call` returns a completed `ModelCall`, so a provider that streams has nowhere to put the
    /// partial text — which is why M2's turns arrived as one block however well the transport
    /// streamed. Adding a parameter to `call` would break every implementation for the benefit of
    /// the one that can stream; a defaulted sibling breaks none, and a provider that cannot stream
    /// is *correct* to deliver its text once at the end.
    ///
    /// **The contract is that `on_delta` receives exactly the text that ends up in the returned
    /// `ModelStep::Say`, in order.** A caller that emits the deltas must therefore not also emit
    /// the finished string — see `engine.rs`, where doing both would double every reply.
    fn call_streaming(
        &mut self,
        view: &ContextView,
        tools: &ExposedSet,
        limits: CallLimits,
        _on_delta: &mut dyn FnMut(&str),
    ) -> Result<ModelCall, ProviderError> {
        self.call(view, tools, limits)
    }

    /// As [`Self::call_streaming`], but reasoning chunks go to `on_reasoning` and answer chunks to
    /// `on_delta`. A provider that does not distinguish them sends everything to `on_delta`.
    ///
    /// # `on_reasoning` carries a TOKEN COUNT, and only a provider can supply one
    ///
    /// The second argument is how many tokens **this chunk** cost — not its length, not an
    /// estimate from it. A streamed frame is one token on both local engines, and the frame is
    /// visible here and nowhere above: by the time a chunk reaches the loop it is a `String`, and
    /// a `String` cannot be tokenized without the tokenizer that produced it.
    ///
    /// **The count is a delta and it may be `0`.** A provider that holds text back while it waits
    /// to learn which channel it belongs to counted those frames when they arrived; releasing the
    /// buffer later must not count them twice. Receivers add.
    ///
    /// `on_retract` carries the same quantity for the speech being taken back, because the text
    /// moves into the thinking block and its cost moves with it.
    fn call_streaming_split(
        &mut self,
        view: &ContextView,
        tools: &ExposedSet,
        limits: CallLimits,
        on_delta: &mut dyn FnMut(&str),
        _on_reasoning: &mut dyn FnMut(&str, u64),
        // Called when a `</think>` proves every chunk handed to `on_delta` this turn was
        // reasoning. See `marlowe_provider::ThinkSplitter`.
        _on_retract: &mut dyn FnMut(u64),
    ) -> Result<ModelCall, ProviderError> {
        self.call_streaming(view, tools, limits, on_delta)
    }

    /// Whether this driver actually streams. **Announced, not inferred**: the engine has to know
    /// whether the deltas it saw were the whole reply or nothing at all, and guessing from "did I
    /// receive any" would be wrong for an empty response.
    fn streams(&self) -> bool {
        false
    }

    /// Whether another provider can take this run. Returning `true` means run state is
    /// preserved across the switch — invariant 4's *degrade, never break*.
    fn failover(&mut self, _error: &ProviderError) -> bool {
        false
    }
}

/// The compaction summarizer. Separate from [`ModelDriver`] because ADR-008 routes it to the
/// cheapest model, and because the assembler must be able to compact without the run's own
/// provider being available.
pub trait Summarizer {
    fn summarize(&mut self, view: &ContextView) -> String;
}

/// What a tool execution produced. The loop never sees raw bytes above the tool's
/// `inline_threshold_bytes`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutcome {
    pub summary: ResultSummary,
    pub body: ToolBody,
    /// Origin-bound at capture, immutable. §2.8's second axis.
    pub trust: marlowe_contract::TrustClass,
    pub failed: bool,
    pub wall_ms: u64,
    /// Head and tail of a body too large to inline.
    ///
    /// **A reference the model cannot dereference is not a result.** `read` has no parameter that
    /// accepts a hash, so a 69 KB file came back as `ref 225bfe8df7bbc044` and the model called
    /// `read` five times getting the same hash. Until the content store and the reference-reading
    /// path land at M2 D, this is what actually reaches attention.
    ///
    /// `None` when the body was inlined whole — there is nothing to preview.
    pub preview: Option<String>,
}

impl ToolOutcome {
    /// The body as the model receives it, without the §B6 summary in front of it.
    ///
    /// Hoisted out of `Engine::finish_call` so that **one** expression produces both the text
    /// that goes to the model and the text that goes to the screen. They were two expressions and
    /// only one of them existed: the screen's was `ResultSummary::render()`, which is metrics.
    pub fn body_text(&self) -> String {
        match &self.body {
            ToolBody::Inline(s) => s.clone(),
            // A reference the model cannot dereference is not a result — see `finish_call`, where
            // this wording was written and where the live failure that produced it is recorded.
            ToolBody::Reference { hash, bytes } => match &self.preview {
                Some(p) => format!("{bytes} B total, ref {hash}\n{p}"),
                None => format!("ref {hash} ({bytes} B)"),
            },
        }
    }

    /// What §B6's *"Enter for full output in place"* shows when a person opens this call's line.
    ///
    /// # Derived here, at one site, rather than set by each executor
    ///
    /// `ResultSummary::detail` is `None` at 24 of the 38 construction sites under `crates/*/src`,
    /// and where it is set at all it holds a harness constant — `"html · id"`, `"400 -> host"` —
    /// never the bytes. So a wire field alone would have carried nothing: expanding a `read` would
    /// still have shown an empty pane, which is the reported symptom, unchanged.
    ///
    /// The detail a person wants is the **body**. Deriving it from the outcome means a thirteenth
    /// tool cannot forget to opt in, which is the argument ADR-039 makes for keying the quarantine
    /// on the trust class rather than on a list of tool names.
    ///
    /// **The reason and the body are both kept when both exist.** `bash` sets `failed: code != 0`
    /// with a full stdout body and no `detail`; `failed()` sets a `detail` and an empty body. A
    /// rule that took one or the other would be right for one of those and silent for the other.
    pub fn screen_detail(&self) -> Option<String> {
        let body = self.body_text();
        let reason = self
            .summary
            .detail
            .as_deref()
            .map(str::trim)
            .filter(|d| !d.is_empty());
        match (reason, body.trim().is_empty()) {
            (Some(r), true) => Some(r.to_string()),
            // Already quoted inside the body: `web`'s detail is a description of the same bytes.
            (Some(r), false) if body.contains(r) => Some(body),
            (Some(r), false) => Some(format!("{r}\n{body}")),
            (None, true) => None,
            (None, false) => Some(body),
        }
    }
}

/// §2.8's first axis: inline vs. reference, driven by **size**. Independent of trust.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolBody {
    Inline(String),
    /// Content-addressed. The bytes are in the content store; the loop holds the hash.
    Reference { hash: String, bytes: u64 },
}

/// One already-adjudicated call, handed to [`ToolHost::execute_batch`].
///
/// **Carrying the `Adjudication` is what makes this safe.** A host receives these only after the
/// permission layer has allowed each one individually; the batch is a scheduling unit, never a
/// permission unit. There is no path by which a host can execute something that was not
/// adjudicated on its own.
pub struct BatchItem<'a> {
    pub tool: &'a ToolId,
    pub args: &'a Args,
    pub adjudication: &'a marlowe_permission::Adjudication,
}

/// The execution port. Implementations land in Session C.
pub trait ToolHost {
    fn execute(
        &mut self,
        tool: &ToolId,
        args: &Args,
        adjudication: &marlowe_permission::Adjudication,
    ) -> ToolOutcome;

    /// Execute a whole adjudicated batch, **returning outcomes in input order**.
    ///
    /// # Why this exists, and why it is defaulted
    ///
    /// The model emits every call in a batch *before seeing any of their results* — see
    /// [`ModelStep::ToolCall`], where that is established as a correctness property rather than an
    /// observation. Calls in one batch therefore cannot have influenced one another, which is
    /// exactly the condition under which running them concurrently changes nothing about what
    /// they compute.
    ///
    /// The loop used to call [`ToolHost::execute`] in a `for` loop, so thirty `web` fetches cost
    /// thirty sequential round trips. Measured on the real `Engine`: an 8-call batch of 250 ms
    /// calls took **2008 ms with a maximum of 1 execution in flight**.
    ///
    /// **The default implementation is the old behaviour, exactly.** A host that does not override
    /// this is serial and correct, so no existing implementor changes meaning by the trait growing
    /// this method — and a host overrides it only when it can say something specific about which
    /// of *its* tools are safe to overlap. That judgement belongs to the host, which knows what
    /// its executors touch; it does not belong to the loop, which does not.
    ///
    /// # The contract an override must honour
    ///
    /// 1. **Return exactly `items.len()` outcomes, in input order.** The loop attributes results
    ///    to calls positionally.
    /// 2. **Never reorder observable side effects that could conflict.** Concurrency is a promise
    ///    the host makes about its own executors, not one the loop makes on its behalf.
    /// 3. **Never execute an item the loop did not hand over.** Every item here was adjudicated.
    fn execute_batch(&mut self, items: &[BatchItem<'_>]) -> Vec<ToolOutcome> {
        items
            .iter()
            .map(|i| self.execute(i.tool, i.args, i.adjudication))
            .collect()
    }

    /// Every tool this host has an executor for.
    ///
    /// **Required, with no default.** A default of "all" would restore exactly the situation this
    /// exists to make impossible, and a default of "none" would be a lie every host has to
    /// remember to correct. See [`crate::profile::UnrunnableTools`].
    fn executes(&self) -> Vec<ToolId>;
}

/// Tools that are **loop control**, not tool-host executions.
///
/// ARCHITECTURE §3's match handles these as `ModelStep` variants: they escalate, request a memory
/// write, and spawn a child. They are representable without the host having an executor, which is
/// why they are exempt from the check below.
pub const CONTROL_TOOLS: [&str; 3] = ["ask", "remember", "run"];

/// The memory port. Wired to `marlowe-memory` in Session D.
pub trait MemoryHost {
    /// Adjudicate, stamp, sign, append — or reject. There is no other write path.
    ///
    /// # `run_floor` is required, and passing it is the whole of ADR-038
    ///
    /// A model-authored claim is written at `min(AgentInferred, run_floor)`. `trust_for_channel`
    /// cannot answer this — it is total over `Channel` with no default arm, and a model is not a
    /// channel, correctly, because §3.3 binds trust to *origin* and the model is not an origin.
    ///
    /// **Bare `AgentInferred` is a laundering path and it is reachable today.** A run that fetches
    /// a page and then calls `remember` would write attacker-shaped text one full class above
    /// `UntrustedContent`, with the page's origin nowhere in the record, and that memory would then
    /// compete for rank 1 at injection on equal terms with everything else. `profile.rs`'s
    /// `reads_untrusted && may_write_memory` guard does not cover it: that closes the *quarantined
    /// reader*, and an ordinary `interactive()` run reads untrusted content and may write memory,
    /// both by design.
    ///
    /// **It is a parameter rather than something the implementation reaches for**, for the same
    /// reason `Provenance::taint_for` takes `latched` rather than deriving it: the run owns the
    /// latch, the latch is monotonic, and a host that re-derived the floor could derive a higher
    /// one. Writing a memory composes a *durable* target out of run content — the longest-lived
    /// composition the system performs — so this is the one place the latch must not be computed
    /// and then discarded.
    ///
    /// The implementation applies the `min`, not the caller: the trust computation belongs in the
    /// crate that owns §3.3, and a loop that handed down a finished class would be a second
    /// implementation of it.
    /// # `session` is required, and without it a written claim is unreachable
    ///
    /// Retrieval scopes candidates by `e.source_session_id == session_id`. A claim written with no
    /// session — or with a placeholder — is a belief that exists, is signed, is durable, and can
    /// **never be retrieved**. That failure is silent in exactly the wrong way: `remember` returns
    /// a receipt, the journal shows the write, and every later query behaves as though the memory
    /// were not there.
    ///
    /// Taken as a parameter rather than held on the host because one host serves every session.
    /// It mirrors `Recorder::append`, which takes run **and** session for the same reason.
    /// # `now_ms` comes from the run's clock, and a host must never read its own
    ///
    /// A belief's `created_at` and its maturation deadline are the same time base the run is
    /// journalled against. A memory host reading a system clock would put the write at a different
    /// instant from the `MemoryWritten` event recording it — two components disagreeing about now,
    /// in a log whose whole value is that it can be replayed. §4.5's rule is scoped to the §4.1/4.6
    /// paths; this is the same discipline applied where it is not strictly required, because the
    /// alternative is a second clock nobody declared.
    fn remember(
        &mut self,
        run: crate::run::RunId,
        session: crate::run::SessionId,
        claim: &ClaimRequest,
        run_floor: marlowe_contract::TrustClass,
        now_ms: i64,
    ) -> Result<String, String>;

    /// Record content that arrived from **outside**, under the channel it arrived on.
    ///
    /// # Why this exists, and it is not a convenience over `remember`
    ///
    /// `remember` writes a *model-authored claim* at `min(AgentInferred, run_floor)`. That is
    /// correct for what it is, and it is **circular as a source of taint**: a claim is only
    /// `UntrustedContent` if the run's floor was already there, and the floor only gets there by
    /// reading something untrusted. No amount of `remember` can produce the first tainted belief.
    ///
    /// `marlowe_memory::ingest` is the non-circular path — `trust_for_channel` is total over
    /// `Channel` with no default arm, and a `Channel::Web` belief is `UntrustedContent` because
    /// of **where it came from**, not because of what a run had already read. Until this method
    /// existed `ingest` had exactly one caller in the workspace, `adapter.rs`'s `--eval-adapter`,
    /// and `Channel::` appeared nowhere in `marlowe-daemon`. Layer 3's latch was therefore
    /// **unreachable in the shipped product**: not broken, but with no live trigger, which is
    /// indistinguishable from working until something reaches it.
    ///
    /// # The class is DERIVED and returned, never supplied
    ///
    /// The caller passes an origin and gets back the class the harness computed. It cannot ask
    /// for one. This mirrors §4.6's rule that the wire declares `origin` and never `trust_class`,
    /// and it is what lets a probe assert on what the system decided rather than on what a test
    /// handed it.
    ///
    /// # What may be passed as `text`, and this is layer 1's boundary
    ///
    /// **The validated summary, never the raw bytes.** A belief is retrieved into a future
    /// window, so ingesting a fetched page verbatim would put attacker-controlled bytes in front
    /// of a run holding tools by a route that goes around the quarantined reader entirely —
    /// brief §8.2 defeated through the memory store. The condensed form has been through a child
    /// with an empty tool set and a validated output contract; what this records is that summary,
    /// under the origin of the page it describes.
    fn ingest_external(
        &mut self,
        run: crate::run::RunId,
        session: crate::run::SessionId,
        content: &ExternalContent<'_>,
        now_ms: i64,
    ) -> Result<marlowe_contract::TrustClass, String>;
}

/// One piece of content that arrived from outside the harness, with the origin it arrived under.
///
/// **`channel` is the whole of the trust decision** and is not derived from `text`. §3.3 binds a
/// class to the authority of the origin, never to the safety of the bytes, and a content signal
/// cannot survive derivation — which is the finding behind HP6.
#[derive(Debug, Clone, Copy)]
pub struct ExternalContent<'a> {
    pub channel: marlowe_contract::Channel,
    /// What produced it — a URL, a message id, a path. Recorded on the turn's `origin.ref`.
    ///
    /// **Not consulted by the trust computation.** It is provenance for a human reading the
    /// journal; a `ref` that could raise a class would be attacker-supplied authority.
    pub reference: Option<&'a str>,
    /// The text to remember. See [`MemoryHost::ingest_external`] on why this is the validated
    /// summary and never the raw bytes.
    pub text: &'a str,
}

/// The surface port. Render-only: §2.14, surfaces hold no policy.
pub trait TurnSink {
    fn emit(&mut self, event: TurnEvent);
}

impl TurnSink for () {
    fn emit(&mut self, _event: TurnEvent) {}
}

/// Approvals are **enforced by the harness, not requested by the model** (§8.2).
pub trait ApprovalGate {
    /// **Whether a human is actually there to ask.**
    ///
    /// Defaulted `false`, which is the safe direction: a gate that cannot ask says so.
    ///
    /// # Why the loop needs this and `await_approval`'s bool is not enough
    ///
    /// `false` from `await_approval` conflates two situations that call for opposite behaviour
    /// from the model:
    ///
    /// - **Nobody could be asked.** Retrying anything in this class will fail identically, so the
    ///   right move is to say the tool is unavailable and carry on without it.
    /// - **Somebody was asked and said no.** A human is present and can approve a *different*
    ///   call. Giving up on the whole capability is wrong.
    ///
    /// Observed live the first time the TUI window shipped: declined once, the model was handed
    /// *"no interactive approval surface is attached to this run"* — a false statement, since the
    /// surface was on screen — and refused to attempt anything further, citing a hard block. The
    /// message was written for the unattended case and reused for the attended one.
    fn is_interactive(&self) -> bool {
        false
    }

    /// Why the last refusal was refused, when the human gave a reason.
    ///
    /// **Defaulted, so no existing gate had to change.** A gate that cannot collect a reason
    /// returns `None` and the model is told only that it was declined — which is what every gate
    /// did before this existed.
    ///
    /// It is a second method rather than a richer return type because the alternative was
    /// rewriting every implementation and every test double to carry a field almost all of them
    /// would leave empty. The reason belongs to the *refusal*, and asking for it after the answer
    /// is the shape that matches.
    fn decline_reason(&self) -> Option<String> {
        None
    }

    fn await_approval(&mut self, radius: &BlastRadius) -> bool;
}

/// Steering and cancellation. §10.1's mid-flight steer, at iteration boundaries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SteerMessage {
    pub text: String,
    pub urgency: Urgency,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Urgency {
    /// Applies at the next iteration.
    Advisory,
    /// Also interrupts in-flight work.
    Immediate,
}

pub trait Control {
    /// **Per run, as of M3 Session A.** It used to take no argument and answer for the whole
    /// stack, which was honest while the only addressable run was the one in front of the user.
    /// A depth-four tree needs `cancel(child)` not to kill the parent, and CONTRACTS §5's
    /// *"children outlive parents"* needs it not to kill a sibling either.
    fn cancelled(&self, run: RunId) -> bool {
        let _ = run;
        false
    }
    /// Guidance addressed to **this** run. See [`Control::cancelled`] for why the id is here:
    /// a broadcast queue cannot steer a child, because the parent asks first.
    fn take_steer(&mut self, run: RunId) -> Option<SteerMessage> {
        let _ = run;
        None
    }
    /// A user cutting in mid-turn.
    ///
    /// **Not addressed, and that is deliberate.** An interrupt is the person at the keyboard
    /// speaking to whatever is in front of them; there is one keyboard.
    fn take_interrupt(&mut self) -> Option<String> {
        None
    }
}

/// The default: nothing steers, nothing cancels, nothing interrupts. Used by children, which
/// at M2 are not independently addressable — that is M3.
#[derive(Debug, Default)]
pub struct NoControl;

impl Control for NoControl {}

/// The time base. §4.5's discipline applied outside §4: **never a system clock on a path a
/// test drives**, so a run at a fixed clock reproduces bit-identically.
pub trait ClockSource {
    fn now_ms(&mut self) -> i64;
}

/// A clock that advances by a fixed step on each read. Deterministic, and the one tests use.
#[derive(Debug, Clone)]
pub struct SteppingClock {
    now: i64,
    step: i64,
}

impl SteppingClock {
    pub fn new(start_ms: i64, step_ms: i64) -> Self {
        Self { now: start_ms, step: step_ms }
    }
}

impl ClockSource for SteppingClock {
    fn now_ms(&mut self) -> i64 {
        let now = self.now;
        self.now += self.step;
        now
    }
}
