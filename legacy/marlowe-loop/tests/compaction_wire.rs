//! **What a compacted conversation looks like on the wire, taken from a real compaction.**
//!
//! # The failure this exists for
//!
//! Live, 2026-08-27, first compaction in a real profile. Journal seq 5196-5201:
//!
//! ```text
//! session_summarized  {"chars":15812}
//! session_spawned
//! model_step  11533
//! model_step  11560
//! run_completed  {"answer": ", using markdown"}
//! ```
//!
//! The entire reply was `", using markdown"` — a fragment beginning with a comma. `compact` was
//! four lines and the first was two defects at once:
//!
//! ```ignore
//! state.volatile = vec![Block::new(SourceKind::History, summary, TrustClass::AgentInferred)];
//! ```
//!
//! 1. The volatile tier is **replaced**, and the user's live turn is in it — the daemon pushes the
//!    triggering message there before the loop starts, and compaction fires at the top of the loop
//!    before the first model call. The question being answered was deleted unread.
//! 2. `History` + `AgentInferred` is `role: "assistant"` on both drivers, so the summary — by then
//!    the only block left — went out as the model's own words.
//!
//! So the model received a system message and 15,812 characters of assistant turn, with no user
//! turn at all, and continued its own sentence. That is the only thing a chat model can do with a
//! conversation that ends on its own message.
//!
//! # Why this is not a unit test on `compact`
//!
//! **A unit test asserting "the volatile tier holds a summary afterwards" passes on exactly the
//! broken build.** So does one asserting the block's `SourceKind`, or its trust class, or its
//! text. Every one of those is true of the four-line version that produced the fragment above.
//!
//! The property is in the **seam**: `compact` chooses a `SourceKind` and `request_body` derives a
//! role from it, and the two disagreed. Nothing that tests halves can see a seam — the fourth time
//! this project has written that sentence, after `done` routing to a tool host with no executor, a
//! persona that loaded but never reached a body, and a spawned child's brief.
//!
//! So: drive the real `Engine` until it really compacts, keep the real `ContextView` the driver
//! was handed **after** the boundary, give it to the real `OllamaDriver::request_body`, and read
//! the roles off the bytes.
//!
//! # What each test reads on the BROKEN build
//!
//! | test | on the broken build |
//! |---|---|
//! | `a_compacted_conversation_does_not_end_on_the_models_own_voice` | **FAILS** — the summary's role is `assistant` and it is the last message |
//! | `the_turn_being_answered_survives_compaction` | **FAILS** — `USER_QUESTION` is in no message at all; the tier was replaced |
//! | `the_old_shape_really_did_render_as_a_lone_assistant_turn` | **PASSES on both** — it is the negative control, and it is *supposed* to reproduce the old shape |
//! | `a_run_that_never_compacts_is_unaffected` | **PASSES on both** — the vacuity control for the two above |

#[path = "common/mod.rs"]
mod common;

use std::collections::VecDeque;

use common::*;
use marlowe_contract::TrustClass;
use marlowe_loop::{
    Assembler, Block, Budget, CallLimits, CapabilityProfile, ContextView, Engine, GovernanceConstraint,
    MemoryRecorder, ModelCall, ModelDriver, Ports, Provenance, ProviderError, Run, RunId, SessionId,
    SessionState, SourceKind,
};
use marlowe_permission::{Tier, Unavailable};
use marlowe_provider::{OllamaDriver, Routing};
use marlowe_tools::{builtin_registry, ExposedSet};

/// The turn the run is answering. Distinctive so its absence is unambiguous.
const USER_QUESTION: &str = "QUESTION-BEING-ANSWERED: which of the two options should I take?";

/// **Deliberately shaped like the one that shipped.** `PassthroughSummarizer` does not invent a
/// summary — it returns the last three volatile blocks verbatim — so a real summary ends
/// mid-sentence in the model's own prior prose. Ending this marker on a dangling clause is what
/// makes the `assistant` role catastrophic rather than merely wrong: there is a sentence to
/// finish, and the model finished it.
const SUMMARY: &str = "SUMMARY-MARKER: ... and then I laid the options out for you, using markdown";

/// A driver that keeps the **views themselves**. `ScriptDriver::views_seen` holds rendered
/// `String`s, and a `String` cannot be handed to `request_body` — a test that rebuilt a view from
/// one would be asserting about its own reconstruction, which is the half that was never wrong.
#[derive(Default)]
struct ViewRecorder {
    steps: VecDeque<ModelCall>,
    views: Vec<ContextView>,
}

impl ModelDriver for ViewRecorder {
    fn call(
        &mut self,
        view: &ContextView,
        _tools: &ExposedSet,
        _limits: CallLimits,
    ) -> Result<ModelCall, ProviderError> {
        self.views.push(view.clone());
        self.steps
            .pop_front()
            .ok_or(ProviderError { detail: "the script ran out of steps".into(), retriable: false })
    }

    fn failover(&mut self, _error: &ProviderError) -> bool {
        false
    }
}

/// A small window, so a handful of seeded turns actually crosses 70%.
fn engine() -> Engine<Unavailable> {
    Engine::new(
        builtin_registry().expect("the builtin manifests load"),
        Unavailable,
        4_000,
        400,
        std::path::PathBuf::from("/ws"),
        Tier::Act,
    )
}

/// The conversation a long dialogue leaves behind, ending on the user's live turn.
///
/// The order matters and it is the product's: the daemon pushes the user's message last, then the
/// loop assembles, then compaction fires — **before** the first model call.
fn seeded_state(session: SessionId, prior_turns: usize) -> SessionState {
    let mut s = SessionState::new(session, "Marlowe. Terminal-native.");
    s.assert_governance(GovernanceConstraint::asserted("never send mail without asking"));
    for i in 0..prior_turns {
        if i % 2 == 0 {
            s.push(Block::new(
                SourceKind::History,
                format!("TURN-{i} {}", "x".repeat(2_400)),
                TrustClass::UserAsserted,
            ));
        } else {
            s.push(Block::assistant_turn(format!("TURN-{i} {}", "y".repeat(2_400)), None, vec![]));
        }
    }
    s.push(Block::new(SourceKind::History, USER_QUESTION, TrustClass::UserAsserted));
    s
}

/// Drive a real run to completion and return every view the driver was actually handed.
fn run_and_record(prior_turns: usize) -> (Vec<ContextView>, SessionState) {
    let mut e = engine();
    let mut driver = ViewRecorder {
        steps: VecDeque::from(vec![say("Option B, and here is why.", 10)]),
        views: Vec::new(),
    };
    let mut summarizer = MarkerSummarizer(SUMMARY.to_string());
    let mut tools = ScriptedTools::default();
    let mut approvals = FixedApprovals(true);
    let mut sink = CollectingSink::default();
    let mut control = marlowe_loop::NoControl;
    let mut clock = FrozenClock(1_700_000_000_000);
    let mut recorder = MemoryRecorder::default();
    let mut run = Run::root(
        RunId::from_name("root"),
        SessionId::from_name("s0"),
        CapabilityProfile::interactive(),
        Budget::interactive(),
        marlowe_loop::OutputContract::answer(),
    );
    let mut state = seeded_state(run.session, prior_turns);
    let mut prov = Provenance::new();
    {
        let mut ports = Ports {
            escalations: None,
            driver: &mut driver,
            summarizer: &mut summarizer,
            tools: &mut tools,
            memory: None,
            approvals: &mut approvals,
            sink: &mut sink,
            control: &mut control,
            clock: &mut clock,
            recorder: &mut recorder,
        };
        e.run(&mut run, &mut state, &mut prov, &mut ports);
    }
    (driver.views, state)
}

/// The one adapter call. Built here rather than mocked, because the role mapping is the subject.
fn wire(view: &ContextView) -> serde_json::Value {
    let ollama = OllamaDriver::new(
        marlowe_provider::LocalEndpoint::default_ollama(),
        Routing::uniform(marlowe_provider::DEFAULT_MODEL).expect("a uniform route"),
        builtin_registry().expect("the builtin manifests load"),
    );
    ollama.request_body(
        view,
        &ExposedSet::new(vec![]).expect("an empty set is constructible"),
        CallLimits { max_output_tokens: 512, route: marlowe_loop::ModelRoute::Orchestrator },
    )
}

fn messages(body: &serde_json::Value) -> Vec<(String, String)> {
    body["messages"]
        .as_array()
        .expect("the request carries messages")
        .iter()
        .map(|m| {
            (
                m["role"].as_str().unwrap_or_default().to_string(),
                m["content"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

/// THE test. Both halves of the defect, on the bytes, from a real compaction.
#[test]
fn a_compacted_conversation_does_not_end_on_the_models_own_voice() {
    let (views, state) = run_and_record(4);

    // ── VACUITY CONTROLS FIRST ───────────────────────────────────────────────────────
    //
    // Every assertion below is about a post-compaction window. A run that never compacted would
    // satisfy all of them for the wrong reason, and the window size — not the code — decides
    // whether a compaction happens. `adr023_live.rs` shipped that exact mistake once already.
    assert!(
        state.compactions >= 1,
        "the window never filled, so nothing below is about compaction at all"
    );
    let after = views
        .iter()
        .find(|v| v.volatile.iter().any(|b| b.text.contains("SUMMARY-MARKER")))
        .unwrap_or_else(|| {
            panic!("no model call carried the summary; {} view(s) recorded", views.len())
        });

    let body = wire(after);
    let msgs = messages(&body);

    // ── HALF ONE: the summary is not the model's own voice ───────────────────────────
    let summary_msg = msgs
        .iter()
        .find(|(_, c)| c.contains("SUMMARY-MARKER"))
        .unwrap_or_else(|| panic!("the summary never reached the wire at all: {msgs:?}"));
    assert_ne!(
        summary_msg.0, "assistant",
        "the compaction summary went out as `assistant`. It is context ABOUT the conversation, \
         not a turn IN it, and a conversation ending on the model's own words leaves it nothing \
         to do but continue them. Live that produced the whole reply `, using markdown`: {msgs:?}"
    );

    // ── HALF TWO: there is something addressed to the model, and it is last ──────────
    assert_ne!(
        msgs.last().expect("at least one message").0,
        "assistant",
        "the post-compaction conversation ends on the model's own turn: {msgs:?}"
    );
    assert!(
        msgs.iter().any(|(r, _)| r == "user"),
        "the post-compaction conversation has no user turn in it at all — the shape the live \
         failure had: {msgs:?}"
    );
}

/// **The data-loss half, which is a defect on its own even if the role were right.**
///
/// The user's message is pushed as a volatile block before the loop starts. Compaction fires at
/// the top of the loop, before the first model call — so on the broken build the model was never
/// shown the question it was answering, on the very first turn of the conversation.
#[test]
fn the_turn_being_answered_survives_compaction() {
    let (views, state) = run_and_record(4);
    assert!(state.compactions >= 1, "nothing compacted; this test would prove nothing");

    let after = views
        .iter()
        .find(|v| v.volatile.iter().any(|b| b.text.contains("SUMMARY-MARKER")))
        .expect("a model call after the boundary");
    let msgs = messages(&wire(after));

    let question = msgs
        .iter()
        .find(|(_, c)| c.contains(USER_QUESTION))
        .unwrap_or_else(|| panic!("compaction deleted the turn being answered: {msgs:?}"));
    assert_eq!(question.0, "user", "the question came back as somebody else's words: {msgs:?}");
    assert_eq!(
        msgs.last().expect("at least one message").1,
        question.1,
        "the live turn is not the last thing the model sees, so the summary is what it is being \
         asked to respond to: {msgs:?}"
    );

    // And the conversation before the boundary is gone, which is the point of compacting. Without
    // this the test would pass against a loop that had quietly stopped compacting.
    assert!(
        !msgs.iter().any(|(_, c)| c.contains("TURN-0")),
        "pre-compaction history is still in the window: {msgs:?}"
    );
}

/// **THE NEGATIVE CONTROL.** This reconstructs, by hand, exactly what `compact` used to produce —
/// a volatile tier of one `History` + `AgentInferred` block — and asserts the wire renders it as
/// the journal recorded it: a system message, one assistant turn, and nothing else.
///
/// It passes on the broken build and on the fixed one, deliberately. Its job is to show that the
/// assertions in the two tests above have teeth: the predicate `role != "assistant"` really does
/// discriminate, and `request_body` really does turn that pair into the model's own voice. A
/// control that could not reproduce the failure would leave those assertions unfalsifiable.
#[test]
fn the_old_shape_really_did_render_as_a_lone_assistant_turn() {
    let mut s = SessionState::new(SessionId::from_name("s0"), "Marlowe.");
    s.assert_governance(GovernanceConstraint::asserted("never send mail without asking"));
    // The line that was there, verbatim in shape.
    s.push(Block::new(SourceKind::History, SUMMARY, TrustClass::AgentInferred));

    let view = Assembler::new(100_000, 10_000).assemble(&s);
    let msgs = messages(&wire(&view));

    let roles: Vec<&str> = msgs.iter().map(|(r, _)| r.as_str()).collect();
    assert_eq!(
        roles,
        vec!["system", "assistant"],
        "the pre-fix shape no longer reproduces, so the tests above are asserting against \
         nothing: {msgs:?}"
    );
    assert!(
        !roles.contains(&"user"),
        "the reproduction has a user turn in it, which the live failure did not: {msgs:?}"
    );

    // Same block, the new `SourceKind`, everything else identical — including the trust class,
    // which stays `AgentInferred` because promoting it would launder a class (layer 2).
    let mut fixed = SessionState::new(SessionId::from_name("s0"), "Marlowe.");
    fixed.push(Block::new(SourceKind::Summary, SUMMARY, TrustClass::AgentInferred));
    let fixed_msgs = messages(&wire(&Assembler::new(100_000, 10_000).assemble(&fixed)));
    assert_eq!(
        fixed_msgs.iter().map(|(r, _)| r.as_str()).collect::<Vec<_>>(),
        vec!["system", "user"],
        "`SourceKind` is what names the speaker, and only the `SourceKind` changed: {fixed_msgs:?}"
    );
    assert_eq!(
        Block::new(SourceKind::Summary, SUMMARY, TrustClass::AgentInferred).trust,
        TrustClass::AgentInferred,
        "the trust class is the origin's and a model composed this text"
    );
}

/// **The vacuity control for the pair above.** They read a window that has crossed the boundary;
/// this reads one that never did, and asserts the ordinary conversation is unchanged. If the
/// `SourceKind` split had broken the plain case, the two tests above could still pass.
#[test]
fn a_run_that_never_compacts_is_unaffected() {
    let (views, state) = run_and_record(0);
    assert_eq!(state.compactions, 0, "this control is only a control while nothing compacts");

    let first = views.first().expect("the model was called at least once");
    let msgs = messages(&wire(first));
    assert!(
        !msgs.iter().any(|(_, c)| c.contains("SUMMARY-MARKER")),
        "a run that never compacted carries a summary: {msgs:?}"
    );
    assert_eq!(
        msgs.last().expect("at least one message").0,
        "user",
        "an ordinary first turn is a system message and the user's question: {msgs:?}"
    );
    assert!(msgs.iter().any(|(_, c)| c.contains(USER_QUESTION)), "{msgs:?}");
}
