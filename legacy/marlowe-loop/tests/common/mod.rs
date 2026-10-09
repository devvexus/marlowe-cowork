//! Scripted ports for driving the one loop in a test.
//!
//! The driver is a **queue**, and that is sound rather than lucky: at M2 a spawn blocks the
//! parent, so parent and child steps interleave in exactly one order. When M3 makes runs
//! concurrent this file needs a per-run script, and the fact that it does is a useful signal
//! that the lifecycle actually changed.

#![allow(dead_code)]

use std::collections::VecDeque;

use marlowe_contract::TrustClass;
use marlowe_loop::{
    ApprovalGate, ClaimRequest, ClockSource, ContextView, MemoryHost, ModelCall, ModelDriver,
    ModelStep, ProviderError, RunId, Summarizer, ToolBody, ToolHost, ToolOutcome, TurnEvent,
    TurnSink, Usage,
};
use marlowe_permission::{Adjudication, Args, BlastRadius};
use marlowe_tools::{ExposedSet, Metric, ResultSummary, ToolId};

/// A model that says what it was told to say, and records what it was allowed to return.
pub struct ScriptDriver {
    pub steps: VecDeque<ModelCall>,
    /// Every `max_output_tokens` the loop handed over, in order. This is the evidence that the
    /// cap is derived from what remains rather than being a constant.
    pub limits_seen: Vec<u64>,
    /// The rendered view for each call, so a test can assert on what the model could see.
    pub views_seen: Vec<String>,
    pub failover_ok: bool,
}

impl ScriptDriver {
    pub fn new(steps: Vec<ModelCall>) -> Self {
        Self {
            steps: steps.into(),
            limits_seen: Vec::new(),
            views_seen: Vec::new(),
            failover_ok: false,
        }
    }
}

impl ModelDriver for ScriptDriver {
    fn call(
        &mut self,
        view: &ContextView,
        _tools: &ExposedSet,
        limits: marlowe_loop::CallLimits,
    ) -> Result<ModelCall, ProviderError> {
        self.limits_seen.push(limits.max_output_tokens);
        self.views_seen.push(view.rendered());
        match self.steps.pop_front() {
            Some(c) => Ok(c),
            None => Err(ProviderError {
                detail: "the script ran out of steps".into(),
                retriable: false,
            }),
        }
    }

    fn failover(&mut self, _error: &ProviderError) -> bool {
        self.failover_ok
    }
}

pub fn say(text: &str, tokens: u64) -> ModelCall {
    ModelCall {
        usage: Usage { completion_tokens: tokens, ..Usage::default() },
        step: ModelStep::Say(text.into()),
    }
}

pub fn step(step: ModelStep, tokens: u64) -> ModelCall {
    ModelCall { usage: Usage { completion_tokens: tokens, ..Usage::default() }, step }
}

/// A summarizer that returns **nothing**.
///
/// Deliberately hostile. A cooperative summarizer would let a test pass against an
/// implementation that merely asked the model to preserve governance constraints.
#[derive(Default)]
pub struct EmptySummarizer;

impl Summarizer for EmptySummarizer {
    fn summarize(&mut self, _view: &ContextView) -> String {
        String::new()
    }
}

/// A summarizer that returns a short marker, for tests that need to see the seam.
pub struct MarkerSummarizer(pub String);

impl Summarizer for MarkerSummarizer {
    fn summarize(&mut self, _view: &ContextView) -> String {
        self.0.clone()
    }
}

#[derive(Default)]
pub struct ScriptedTools {
    pub calls: Vec<(String, Args)>,
    pub body: Option<String>,
    pub trust: Option<TrustClass>,
}

impl ToolHost for ScriptedTools {
    /// A scripted host answers for whatever it was scripted with, plus the builtins tests drive.
    fn executes(&self) -> Vec<marlowe_tools::ToolId> {
        marlowe_tools::BUILTIN_TOOLS.iter().map(|t| marlowe_tools::ToolId::new(*t)).collect()
    }

    fn execute(&mut self, tool: &ToolId, args: &Args, _a: &Adjudication) -> ToolOutcome {
        self.calls.push((tool.to_string(), args.clone()));
        ToolOutcome {
            summary: ResultSummary::new(vec![Metric::State("ok")]),
            body: ToolBody::Inline(self.body.clone().unwrap_or_else(|| "ok".into())),
            trust: self.trust.unwrap_or(TrustClass::AgentObserved),
            failed: false,
            wall_ms: 1,
            preview: None,
        }
    }
}

pub struct FixedApprovals(pub bool);

impl ApprovalGate for FixedApprovals {
    fn await_approval(&mut self, _radius: &BlastRadius) -> bool {
        self.0
    }
}

#[derive(Default)]
pub struct CollectingSink {
    pub events: Vec<TurnEvent>,
}

impl CollectingSink {
    /// Everything the surface was told, concatenated. Used to prove a child *did* say the thing
    /// a parent's context must not contain — without it, an assertion that the marker is absent
    /// from the parent would pass vacuously if the child never produced it.
    pub fn text(&self) -> String {
        self.events
            .iter()
            .filter_map(|e| match e {
                TurnEvent::TextDelta(t) => Some(t.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

impl TurnSink for CollectingSink {
    fn emit(&mut self, event: TurnEvent) {
        self.events.push(event);
    }
}

#[derive(Default)]
pub struct RecordingMemory {
    pub claims: Vec<ClaimRequest>,
    /// The run floor each claim arrived with. **Recorded rather than ignored**: ADR-038 makes the
    /// floor the deciding input to a claim's trust class, and a double that dropped it would let
    /// every write-path test pass while the loop handed down whatever it liked.
    pub floors: Vec<marlowe_contract::TrustClass>,
    /// The session each claim was scoped to. Recorded for the same reason as `floors`: a claim
    /// written to the wrong session is retrievable by nobody, and nothing else would notice.
    pub sessions: Vec<marlowe_loop::run::SessionId>,
    /// The clock reading each claim was written at. Recorded so a test can assert the memory write
    /// and the run share one time base rather than two.
    pub times: Vec<i64>,
    /// Every `ingest_external` call, as `(channel, reference, text)`.
    ///
    /// **The CHANNEL is the assertable thing here, never the class.** See the stub return in the
    /// impl below: this double does not derive a trust class and no test in this crate may read
    /// one from it. What a `marlowe-loop` test can honestly assert is *which origin the loop
    /// declared* — that a fetched page is ingested as `Channel::Web` and not as something the
    /// loop chose to be kinder about. Deriving the class from that origin is
    /// `marlowe-memory`'s job and is asserted against the real `ingest` in the daemon's probe.
    ///
    /// **Nothing calls `ingest_external` in this crate today** — not the loop, not a test. The
    /// field and the stub below are a tripwire for the day something does, and saying so is the
    /// point: a comment describing tests that do not exist is instance #16 inside a comment
    /// written to avoid instance #16. The rule the stub encodes is that no test may read the class
    /// from here; it is enforced by the return value, not by this sentence. See the impl.
    pub ingested: Vec<(marlowe_contract::Channel, Option<String>, String)>,
}

impl MemoryHost for RecordingMemory {
    fn remember(
        &mut self,
        _run: RunId,
        session: marlowe_loop::run::SessionId,
        claim: &ClaimRequest,
        run_floor: marlowe_contract::TrustClass,
        now_ms: i64,
    ) -> Result<String, String> {
        self.claims.push(claim.clone());
        self.floors.push(run_floor);
        self.sessions.push(session);
        self.times.push(now_ms);
        Ok(format!("m-{}", self.claims.len()))
    }

    fn ingest_external(
        &mut self,
        _run: RunId,
        _session: marlowe_loop::run::SessionId,
        content: &marlowe_loop::ExternalContent<'_>,
        _now_ms: i64,
    ) -> Result<marlowe_contract::TrustClass, String> {
        self.ingested.push((
            content.channel,
            content.reference.map(str::to_string),
            content.text.to_string(),
        ));
        // **A STUB, AND LABELLED ONE, BECAUSE THE ALTERNATIVES ARE BOTH WORSE.**
        //
        // Returning a class here means one of two things. Mirroring `trust_for_channel`'s table
        // would be a *second implementation* of §3.3 — the exact thing `trust.rs`'s module docs
        // forbid, and it would go on passing after the real table changed. Calling the real one
        // is not available: `marlowe-loop`'s tests do not depend on `marlowe-memory`, and adding
        // that dependency to reach a lookup table would invert the crate graph.
        //
        // So this returns a fixed value, and the rule is stated where someone would otherwise be
        // tempted to assert on it: **no test in this crate reads this value.** They assert on
        // `ingested`, which records what the loop *declared*. The derivation is asserted against
        // the real `ingest` in the daemon's probe, which is the only place it is evidence about
        // anything.
        //
        // # WHY THE VALUE IS `AgentInferred`, AND IT IS NOT AN ARBITRARY CHOICE
        //
        // It used to be `UntrustedContent`. That is instance #16's shape sitting in a test helper:
        // it is *exactly the answer a test would want*, so
        //
        //     assert_eq!(memory.ingest_external(..)?, TrustClass::UntrustedContent)
        //
        // passes — and goes on passing with `trust_for_channel` deleted, with
        // `marlowe_memory::ingest` deleted, and with `DaemonMemory::ingest_external` deleted,
        // because none of them is on the path. A green test asserting a double's constant, which
        // reads exactly like a green test asserting the security property.
        //
        // It was then `UserAsserted` for one commit, and a review caught the second-order problem:
        // `UserAsserted` is the TOP of the lattice, so `min(UserAsserted, x) == x` — it is the
        // identity element of the operation every propagation path is built from. A double whose
        // stub is the identity under `min` leaves a run untainted for free if a future loop path
        // ever folds this return into a floor, and the containment test then goes green *because
        // of the double*. Wrong-and-loud was the goal; that value is wrong-and-invisible.
        //
        // `AgentInferred` is the choice because it is the one variant `trust_for_channel` returns
        // for **no channel at all**: `Terminal`/`Voice` are `UserAsserted`, `ToolOutput` is
        // `AgentObserved`, and `Web`/`Email`/`Messaging`/`Mcp`/`File` are `UntrustedContent`. So
        // it is wrong for every channel a caller could pass, unlike `UserAsserted` (right for
        // `Terminal`) and `AgentObserved` (right for `ToolOutput`), and it is not the identity
        // under `min`.
        //
        // **The fixed value cannot be made impossible** — `TrustClass` has four inhabitants — so
        // this is the strongest available form of "a test cannot want it" rather than a proof. If
        // a channel is ever added that maps to `AgentInferred`, this constant stops doing its job
        // and the comment stops being true; change it then.
        Ok(marlowe_contract::TrustClass::AgentInferred)
    }
}

/// A clock that never moves, so wall-clock never becomes the dimension a budget test trips on
/// by accident.
pub struct FrozenClock(pub i64);

impl ClockSource for FrozenClock {
    fn now_ms(&mut self) -> i64 {
        self.0
    }
}

/// A tool call that carries no meaning beyond "keep going and add bulk to history".
///
/// **Why tests that grow context use this rather than `say`.** M2 C2e made completion the absence
/// of an action: a reply with no tool call ends the turn. That is the product's termination rule,
/// so a test that grew context with five consecutive `say` steps was exercising a loop that no
/// longer exists — it would now end on the first one.
///
/// A multi-step turn in the real product is a sequence of tool calls followed by one reply, and
/// that is what these helpers build. The bulk arrives as tool *results*, which is also where bulk
/// actually comes from.
pub fn work(tokens: u64) -> ModelCall {
    step(
        ModelStep::one_call(marlowe_tools::ToolId::new("read"), marlowe_permission::Args::new()
                .with("path", marlowe_permission::ArgValue::Text("./notes.md".into()))),
        tokens,
    )
}
