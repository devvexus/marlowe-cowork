//! **The seam: a real spawn's child window, on the real wire.**
//!
//! `child_brief_role.rs` pins the role mapping against a hand-built view. `spawn_from_a_model_reply.rs`
//! pins the spawn against a scripted driver that answers whatever it is shown. **Both were green
//! throughout the live failure they describe**, because the defect was in neither half:
//! `Engine::spawn` chose a `SourceKind` and `request_body` derived a role from it, and the two
//! disagreed. Every spawned child's conversation came out as
//!
//! ```text
//! system:    <identity, governance>
//! assistant: <the task>
//! ```
//!
//! with no user turn in it. Journal seq 4597–4601, 2026-08-26: four model calls, ~12k tokens, no
//! output, and the run failed with *"the model produced no reply and no tool call 3 times in a
//! row"*. Nothing that tests halves can see a seam — the third time in this project, after `done`
//! routed to a tool host with no executor and a persona that loaded but never reached a body.
//!
//! So this builds no view. It drives a spawn through `Engine`, records the `ContextView` the child
//! was actually called with, and hands that view to `OllamaDriver::request_body`.
//!
//! **It is also the control on `ollama.rs`'s `SourceKind::Brief => "user"` arm, which is
//! decorative**: `_ => "user"` already catches it, so deleting that arm fails nothing. What is
//! load-bearing is that `spawn` keeps the brief OUT of the `History | ChildResults` arm, and that
//! is what this crosses.

#[path = "../../marlowe-loop/tests/common/mod.rs"]
mod common;

use common::*;
use marlowe_contract::TrustClass;
use marlowe_loop::{
    Block, Budget, CallLimits, CapabilityProfile, ContextView, Engine, MemoryRecorder,
    ModelCall, ModelDriver, ModelStep, OutputContract, Ports, Provenance, ProviderError, Run, RunId,
    SessionId, SessionState, SourceKind, Usage,
};
use marlowe_permission::{Tier, Unavailable};
use marlowe_provider::ollama::parse_step;
use marlowe_provider::{OllamaDriver, Routing};
use marlowe_tools::{builtin_registry, ExposedSet};
use std::collections::VecDeque;

const TASK: &str = "Summarise this text: the quick brown fox jumped over the lazy dog.";

/// A `ScriptDriver` that keeps the **views themselves**, not their rendering. `views_seen` holds
/// `String`s, which cannot be handed to `request_body` — and a test that reconstructed a view from
/// a rendered string would be asserting about its own reconstruction.
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
        self.steps.pop_front().ok_or(ProviderError {
            detail: "the script ran out of steps".into(),
            retriable: false,
        })
    }

    fn failover(&mut self, _error: &ProviderError) -> bool {
        false
    }
}

fn reply(message: serde_json::Value, tokens: u64) -> ModelCall {
    ModelCall {
        usage: Usage { completion_tokens: tokens, ..Usage::default() },
        step: parse_step(&message),
    }
}

#[test]
fn a_real_spawn_puts_the_brief_on_the_wire_as_a_turn_the_child_can_answer() {
    let registry = builtin_registry().expect("the builtin manifests load");
    let mut e = Engine::new(
        registry,
        Unavailable,
        100_000,
        10_000,
        std::path::PathBuf::from("/ws"),
        Tier::Act,
    );

    // Straight from `/api/chat`'s shape through the shipped adapter. No `ModelStep` is constructed
    // here, so a regression in `control_step`'s routing fails this too.
    let mut driver = ViewRecorder {
        steps: VecDeque::from(vec![
            reply(
                serde_json::json!({
                    "content": "",
                    "tool_calls": [{ "function": { "name": "run", "arguments": {
                        "task": TASK,
                        "exposed_tools": "",
                        "output_contract": "a one-line summary",
                    }}}],
                }),
                100,
            ),
            reply(serde_json::json!({ "content": "A fox jumped over a dog." }), 100),
            reply(serde_json::json!({ "content": "Done." }), 100),
        ]),
        views: Vec::new(),
    };

    let mut summarizer = EmptySummarizer;
    let mut tools = ScriptedTools::default();
    let mut approvals = FixedApprovals(true);
    let mut sink = CollectingSink::default();
    let mut control = marlowe_loop::NoControl;
    let mut clock = FrozenClock(1_700_000_000_000);
    let mut recorder = MemoryRecorder::default();
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

    let mut run = Run::root(
        RunId::from_name("root"),
        SessionId::from_name("root-session"),
        CapabilityProfile::interactive(),
        Budget::interactive(),
        OutputContract::answer(),
    );
    let mut state = SessionState::new(run.session, "Marlowe.");
    state.push(Block::new(
        SourceKind::History,
        "summarise something for me".to_string(),
        TrustClass::UserAsserted,
    ));
    let mut prov = Provenance::new();
    e.run(&mut run, &mut state, &mut prov, &mut ports);

    // The child's window is dropped at the end of `Engine::spawn` by design — §10.2's "the
    // orchestrator's context must never accumulate raw worker history" is structural. The
    // recording is the only place it can be observed.
    let child_view = driver
        .views
        .iter()
        .find(|v| v.volatile.iter().any(|b| b.text.contains(TASK)))
        .unwrap_or_else(|| {
            panic!("the child was never called with its brief; {} views recorded", driver.views.len())
        });

    let ollama = OllamaDriver::new(
        marlowe_provider::LocalEndpoint::default_ollama(),
        Routing::uniform(marlowe_provider::DEFAULT_MODEL).expect("a uniform route"),
        builtin_registry().expect("the builtin manifests load"),
    );
    let body = ollama.request_body(
        child_view,
        &ExposedSet::new(vec![]).expect("a toolless child is constructible"),
        CallLimits { max_output_tokens: 512, route: marlowe_loop::ModelRoute::Orchestrator },
    );
    let messages: Vec<(&str, &str)> = body["messages"]
        .as_array()
        .expect("the request carries messages")
        .iter()
        .map(|m| (m["role"].as_str().unwrap_or_default(), m["content"].as_str().unwrap_or_default()))
        .collect();

    let brief = messages
        .iter()
        .find(|(_, c)| c.contains(TASK))
        .unwrap_or_else(|| panic!("the brief never reached the wire: {messages:?}"));
    assert_eq!(
        brief.0, "user",
        "a spawned child's brief goes out as `{}`, so its conversation ends on its own message \
         with nothing to answer. Live, that produced four model calls and no output: {messages:?}",
        brief.0,
    );
    assert_eq!(
        messages.last().expect("at least one message").0,
        "user",
        "the child's conversation does not end on a turn addressed to it: {messages:?}",
    );
}

/// **The same defect one level up, and the child fix did not reach it.**
///
/// `SourceKind::Brief` made the CHILD's window answerable. The PARENT's was still malformed: the
/// child's return went in as `ChildResults` + `AgentInferred`, which both drivers map to
/// `role: "assistant"`, and `ModelStep::Spawn` pushed no assistant turn announcing the call — so
/// after a child returned, the parent's conversation ended on a message the parent had supposedly
/// written, with nothing to answer.
///
/// Journal seq 4630–4643, 2026-08-26: the child ran, completed, and returned a summary; the parent
/// then produced nothing four times and failed with *"the model produced no reply and no tool call
/// 3 times in a row"*. The first fix was verified live on the child and the parent was never
/// looked at, which is the whole reason this asserts on **both windows from one spawn**.
#[test]
fn after_a_child_returns_the_parent_has_something_to_answer() {
    let registry = builtin_registry().expect("the builtin manifests load");
    let mut e = Engine::new(
        registry,
        Unavailable,
        100_000,
        10_000,
        std::path::PathBuf::from("/ws"),
        Tier::Act,
    );
    let mut driver = ViewRecorder {
        steps: VecDeque::from(vec![
            reply(
                serde_json::json!({
                    "content": "",
                    "tool_calls": [{ "function": { "name": "run", "arguments": {
                        "task": TASK,
                        "exposed_tools": "",
                        "output_contract": "a one-line summary",
                    }}}],
                }),
                100,
            ),
            reply(serde_json::json!({ "content": "A fox jumped over a dog." }), 100),
            reply(serde_json::json!({ "content": "Done." }), 100),
        ]),
        views: Vec::new(),
    };

    let mut summarizer = EmptySummarizer;
    let mut tools = ScriptedTools::default();
    let mut approvals = FixedApprovals(true);
    let mut sink = CollectingSink::default();
    let mut control = marlowe_loop::NoControl;
    let mut clock = FrozenClock(1_700_000_000_000);
    let mut recorder = MemoryRecorder::default();
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
    let mut run = Run::root(
        RunId::from_name("root"),
        SessionId::from_name("root-session"),
        CapabilityProfile::interactive(),
        Budget::interactive(),
        OutputContract::answer(),
    );
    let mut state = SessionState::new(run.session, "Marlowe.");
    state.push(Block::new(
        SourceKind::History,
        "summarise something for me".to_string(),
        TrustClass::UserAsserted,
    ));
    let mut prov = Provenance::new();
    e.run(&mut run, &mut state, &mut prov, &mut ports);

    // The parent's LAST call — the one it had to answer from, after the child came back.
    let parent_view = driver
        .views
        .iter()
        .filter(|v| v.volatile.iter().any(|b| b.text.contains("A fox jumped over a dog.")))
        .next_back()
        .expect("the parent was called after the child returned");

    let ollama = OllamaDriver::new(
        marlowe_provider::LocalEndpoint::default_ollama(),
        Routing::uniform(marlowe_provider::DEFAULT_MODEL).expect("a uniform route"),
        builtin_registry().expect("the builtin manifests load"),
    );
    let body = ollama.request_body(
        parent_view,
        &ExposedSet::new(vec![]).expect("empty"),
        CallLimits { max_output_tokens: 512, route: marlowe_loop::ModelRoute::Orchestrator },
    );
    let messages: Vec<(&str, &str)> = body["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .map(|m| (m["role"].as_str().unwrap_or_default(), m["content"].as_str().unwrap_or_default()))
        .collect();

    let returned = messages
        .iter()
        .find(|(_, c)| c.contains("A fox jumped over a dog."))
        .unwrap_or_else(|| panic!("the child's result never reached the parent's wire: {messages:?}"));
    assert_ne!(
        returned.0, "assistant",
        "the child's result arrived in the parent's window as the PARENT's own words, so there \
         is nothing left for it to reply to: {messages:?}",
    );

    assert_ne!(
        messages.last().expect("at least one message").0,
        "assistant",
        "the parent's conversation ends on its own turn. Live, that produced four model calls and \
         no output: {messages:?}",
    );

    // ── THE PAIRING, ASSERTED UNCONDITIONALLY ────────────────────────────────────────
    //
    // The first version of this was guarded by `if returned.0 == "tool"`, and that guard made it
    // worthless: delete the announcing assistant turn and `unorphan_tool_messages` demotes the
    // result to `user`, which satisfies both assertions above AND skips this block. The test would
    // have gone green on a build where the parent's window records nothing about its own action.
    // A conditional assertion is not an assertion — it is the vacuity family with an `if` in it.
    //
    // So this asserts the property directly and always: **the parent's window records that it
    // called `run`.** Independent of the role the result ends up with.
    let raw = body["messages"].as_array().expect("messages");
    let announced: Vec<&str> = raw
        .iter()
        .filter(|m| m["role"].as_str() == Some("assistant"))
        .filter_map(|m| m["tool_calls"].as_array())
        .flatten()
        .filter(|c| c["function"]["name"].as_str() == Some("run"))
        .filter_map(|c| c["id"].as_str())
        .collect();
    assert!(
        !announced.is_empty(),
        "no assistant turn in the parent's window says it called `run`, so the spawn is invisible          to the model that made it: {messages:?}"
    );

    // And when the result went out as a `tool` message, it names one of those calls — an orphan
    // would be a malformed conversation, which is what `unorphan_tool_messages` exists to stop.
    if returned.0 == "tool" {
        let id = raw
            .iter()
            .find(|m| m["content"].as_str().unwrap_or_default().contains("A fox jumped"))
            .and_then(|m| m["tool_call_id"].as_str())
            .expect("a tool result names the call it answers");
        assert!(
            announced.contains(&id),
            "the child's result answers `{id}`, which no assistant turn announced: {messages:?}"
        );
    }
}

/// **What the SURFACE was told, in order.** Audit finding E4 and §10.2: a subagent returns
/// findings, not a transcript, and prose composed in a child's window must not reach a terminal.
///
/// Watched live 2026-08-26, the user's report: *"I saw the result of the subagent first and then I
/// saw it get overridden by the main agent."* On the wire the child really does stream its answer
/// — 18 content frames in the `--dev` dump — so the only question that matters is whether those
/// deltas reach the sink. This reads the sink.
///
/// The control is the second half: the child must actually have SAID the thing, or an assertion
/// that the parent's screen lacks it passes because nothing ever produced it.
///
/// **And the control took two attempts, which is the part worth keeping.** The first replaced
/// `QuarantinedSink` at the first of its TWO construction sites — line 1998 is layer 1's
/// quarantined reader, line 2587 is the spawn — so it removed a guard this test does not exercise
/// and the test stayed green. A passing control reads exactly like a passing test. Pointed at the
/// spawn's sink it fails with the surface having seen
/// the child's answer followed by the parent's, which is the user's report verbatim.
#[test]
fn a_childs_prose_never_reaches_the_surface_but_the_parents_answer_does() {
    const CHILD_SAID: &str = "A fox jumped over a dog.";
    const PARENT_SAID: &str = "Done, and here is what came back.";

    let registry = builtin_registry().expect("the builtin manifests load");
    let mut e = Engine::new(
        registry,
        Unavailable,
        100_000,
        10_000,
        std::path::PathBuf::from("/ws"),
        Tier::Act,
    );
    let mut driver = ViewRecorder {
        steps: VecDeque::from(vec![
            reply(
                serde_json::json!({
                    "content": "",
                    "tool_calls": [{ "function": { "name": "run", "arguments": {
                        "task": TASK, "exposed_tools": "", "output_contract": "a one-line summary",
                    }}}],
                }),
                100,
            ),
            reply(serde_json::json!({ "content": CHILD_SAID }), 100),
            reply(serde_json::json!({ "content": PARENT_SAID }), 100),
        ]),
        views: Vec::new(),
    };
    let mut summarizer = EmptySummarizer;
    let mut tools = ScriptedTools::default();
    let mut approvals = FixedApprovals(true);
    let mut sink = CollectingSink::default();
    let mut control = marlowe_loop::NoControl;
    let mut clock = FrozenClock(1_700_000_000_000);
    let mut recorder = MemoryRecorder::default();
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
    let mut run = Run::root(
        RunId::from_name("root"),
        SessionId::from_name("root-session"),
        CapabilityProfile::interactive(),
        Budget::interactive(),
        OutputContract::answer(),
    );
    let mut state = SessionState::new(run.session, "Marlowe.");
    state.push(Block::new(
        SourceKind::History,
        "summarise something for me".to_string(),
        TrustClass::UserAsserted,
    ));
    let mut prov = Provenance::new();
    e.run(&mut run, &mut state, &mut prov, &mut ports);

    // THE CONTROL FIRST. The child's words have to exist somewhere, or the absence below is
    // vacuous — the same reason `CollectingSink::text` was given a doc comment saying so.
    let parent_view = driver
        .views
        .iter()
        .find(|v| v.volatile.iter().any(|b| b.text.contains(CHILD_SAID)))
        .expect("the child's answer reached the PARENT'S CONTEXT, which is where it belongs");
    let _ = parent_view;

    let on_screen = sink.text();
    assert!(
        on_screen.contains(PARENT_SAID),
        "the parent's own answer never reached the surface: {on_screen:?}"
    );
    assert!(
        !on_screen.contains(CHILD_SAID),
        "a child's prose reached the terminal. §10.2: a subagent returns findings, not a \
         transcript, and audit finding E4 forbids exactly this. What the surface saw: {on_screen:?}"
    );
}

/// **A child's result is in the durable record.**
///
/// `RunCompleted` was journaled as `{}`. Every run in this project's history recorded that it had
/// finished and nothing about what it finished with — and for a CHILD that is the whole output.
/// `Engine::spawn` drops `child_state` by design (§10.2: the orchestrator's context must never
/// accumulate raw worker history), so the journal was the only place a child's answer could have
/// survived, and it held an empty object.
///
/// What remained was the PARENT's account of the child, which is exactly backwards: this project's
/// standing rule is that a model's summary of a thing is not the thing. The handoff document of
/// 2026-08-26 is the case in point — it reported a spawn that "returned complete content without
/// any failure conditions" and there was no record to check it against.
#[test]
fn a_childs_result_survives_in_the_journal() {
    const CHILD_SAID: &str = "Seventeen crows, one fox, one dog.";
    let registry = builtin_registry().expect("the builtin manifests load");
    let mut e = Engine::new(
        registry,
        Unavailable,
        100_000,
        10_000,
        std::path::PathBuf::from("/ws"),
        Tier::Act,
    );
    let mut driver = ViewRecorder {
        steps: VecDeque::from(vec![
            reply(
                serde_json::json!({
                    "content": "",
                    "tool_calls": [{ "function": { "name": "run", "arguments": {
                        "task": TASK, "exposed_tools": "", "output_contract": "a one-line summary",
                    }}}],
                }),
                100,
            ),
            reply(serde_json::json!({ "content": CHILD_SAID }), 100),
            reply(serde_json::json!({ "content": "Done." }), 100),
        ]),
        views: Vec::new(),
    };
    let mut summarizer = EmptySummarizer;
    let mut tools = ScriptedTools::default();
    let mut approvals = FixedApprovals(true);
    let mut sink = CollectingSink::default();
    let mut control = marlowe_loop::NoControl;
    let mut clock = FrozenClock(1_700_000_000_000);
    let mut recorder = MemoryRecorder::default();
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
        let mut run = Run::root(
            RunId::from_name("root"),
            SessionId::from_name("root-session"),
            CapabilityProfile::interactive(),
            Budget::interactive(),
            OutputContract::answer(),
        );
        let mut state = SessionState::new(run.session, "Marlowe.");
        state.push(Block::new(
            SourceKind::History,
            "summarise something".to_string(),
            TrustClass::UserAsserted,
        ));
        let mut prov = Provenance::new();
        e.run(&mut run, &mut state, &mut prov, &mut ports);
    }

    let completions = recorder.payloads(marlowe_journal::EventKind::RunCompleted);
    assert!(!completions.is_empty(), "nothing recorded a completion at all");

    let carried: Vec<&serde_json::Value> = completions
        .iter()
        .copied()
        .filter(|p| p.get("fields").is_some())
        .collect();
    assert!(
        !carried.is_empty(),
        "every RunCompleted is still shapeless. Live, they were all `{{}}`: {completions:?}"
    );

    let text = serde_json::to_string(&carried).unwrap();
    assert!(
        text.contains(CHILD_SAID),
        "the CHILD's own answer is not in the durable record, so a spawn that returned the wrong \
         thing cannot be examined afterwards -- only the parent's account of it: {carried:?}"
    );

    // Spend travels with it: a result with no cost beside it cannot be judged for value.
    assert!(
        carried.iter().any(|p| p.get("spent_tokens").is_some()),
        "a completion records what was produced but not what it cost: {carried:?}"
    );
}
