//! **Who writes `MemoryWritten` — and the answer is: not the loop.**
//!
//! `engine.rs` has always carried the comment *"The loop does not append a MemoryWritten event
//! itself — that is the memory component's to emit, because it is the one that signs."* Until M2
//! Session D wired a memory host, the code did the opposite, and nothing could see it: the loop's
//! event was the only one in the log, so a fold over it worked.
//!
//! With a host wired, one `remember` produced **two** `MemoryWritten` events — the memory
//! component's signed `MemoryWrittenPayload`, and the loop's carrying `{"text": ...}`.
//! `BeliefStore::derive` decodes every `MemoryWritten` into a `MemoryWrittenPayload`, so the second
//! failed with `missing field 'id'` and **the daemon refused to start on the next restart.**
//!
//! It was found by a real end-to-end run, and every unit test in the workspace passed throughout:
//! `marlowe-memory`'s claim tests call `remember_claim` directly, and `marlowe-daemon`'s durability
//! test calls the host directly. Neither crosses the seam where the two writers meet. **A test that
//! drives half a path cannot see a seam** — the same lesson `done`'s routing produced in M2 C2c.
//!
//! This test is the seam, expressed in the one place that owns it.

mod common;

use common::*;
use marlowe_contract::TrustClass;
use marlowe_journal::EventKind;
use marlowe_loop::driver::ClaimRequest;
use marlowe_loop::{
    Budget, CapabilityProfile, Engine, GovernanceConstraint, LoopOutcome, MemoryRecorder,
    ModelStep, OutputContract, Ports, Provenance, Run, RunId, SessionId, SessionState,
};
use marlowe_permission::{scope::WorkspaceScope, Tier};
use marlowe_tools::builtin_registry;

fn engine() -> Engine<WorkspaceScope> {
    Engine::new(
        builtin_registry().expect("the builtins load"),
        WorkspaceScope::new().expect("a scope here"),
        32_000,
        2_000,
        std::env::current_dir().expect("a cwd"),
        Tier::Act,
    )
}

fn claim(text: &str) -> ClaimRequest {
    ClaimRequest {
        text: text.to_string(),
        payload_kind: String::new(),
        derived_from: Vec::new(),
    }
}

/// One `remember` must put **exactly one** `MemoryWritten` in the log, and it must not be the
/// loop's.
#[test]
fn a_successful_remember_produces_no_memory_written_event_from_the_loop() {
    let mut driver = ScriptDriver::new(vec![
        step(ModelStep::MemoryWrite(claim("the release train leaves Thursday")), 50),
        say("noted", 50),
    ]);
    let mut summarizer = EmptySummarizer;
    let mut tools = ScriptedTools::default();
    let mut approvals = FixedApprovals(true);
    let mut sink = CollectingSink::default();
    let mut control = marlowe_loop::NoControl;
    let mut clock = FrozenClock(1_700_000_000_000);
    let mut recorder = MemoryRecorder::default();
    let mut memory = RecordingMemory::default();

    let mut e = engine();
    let mut ports = Ports {
        escalations: None,
        driver: &mut driver,
        summarizer: &mut summarizer,
        tools: &mut tools,
        // **The host is wired, which is the whole point.** With `memory: None` the write fails,
        // the loop records a rejection, and the duplicate this test is about never occurs — so the
        // test would pass while proving nothing.
        memory: Some(&mut memory),
        approvals: &mut approvals,
        sink: &mut sink,
        control: &mut control,
        clock: &mut clock,
        recorder: &mut recorder,
    };

    let mut run = Run::root(
        RunId::from_name("root"),
        SessionId::from_name("root-session"),
        CapabilityProfile::consolidation(),
        Budget::interactive(),
        OutputContract::answer(),
    );
    let mut state = SessionState::new(run.session, "Marlowe.");
    state.assert_governance(GovernanceConstraint::asserted("c"));
    let mut prov = Provenance::new();
    let outcome = e.run(&mut run, &mut state, &mut prov, &mut ports);

    assert!(matches!(outcome, LoopOutcome::Completed(_)), "{outcome:?}");
    assert_eq!(memory.claims.len(), 1, "the host received the claim");

    assert_eq!(
        recorder.count(EventKind::MemoryWritten),
        0,
        "the loop must emit NO MemoryWritten -- the memory component signs it and owns it. Two \
         of them makes BeliefStore::derive fail with `missing field id`, and the daemon refuses \
         to start on the next restart"
    );
}

/// The control, and it is what stops the assertion above from being vacuous.
///
/// A refusal the memory component never saw — no host wired, or a host that rejects before it
/// reaches `remember_claim` — is refused by nobody else. §4.6: a refusal inferred from absence is
/// not a refusal anyone can measure. `MemoryWriteRejected` is not a belief-store input, so the fold
/// ignores it by kind rather than choking on its shape.
#[test]
fn a_refused_remember_is_still_recorded_by_the_loop() {
    let mut driver = ScriptDriver::new(vec![
        step(ModelStep::MemoryWrite(claim("something")), 50),
        say("noted", 50),
    ]);
    let mut summarizer = EmptySummarizer;
    let mut tools = ScriptedTools::default();
    let mut approvals = FixedApprovals(true);
    let mut sink = CollectingSink::default();
    let mut control = marlowe_loop::NoControl;
    let mut clock = FrozenClock(1_700_000_000_000);
    let mut recorder = MemoryRecorder::default();

    let mut e = engine();
    let mut ports = Ports {
        escalations: None,
        driver: &mut driver,
        summarizer: &mut summarizer,
        tools: &mut tools,
        // No host: the write cannot happen, and the refusal must still be visible in the log.
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
        CapabilityProfile::consolidation(),
        Budget::interactive(),
        OutputContract::answer(),
    );
    let mut state = SessionState::new(run.session, "Marlowe.");
    state.assert_governance(GovernanceConstraint::asserted("c"));
    let mut prov = Provenance::new();
    let _ = e.run(&mut run, &mut state, &mut prov, &mut ports);

    assert_eq!(
        recorder.count(EventKind::MemoryWriteRejected),
        1,
        "a claim no memory component ever saw must not be refused silently"
    );
    assert_eq!(recorder.count(EventKind::MemoryWritten), 0);
}

/// ADR-038's floor reaches the host through the loop, and the session with it.
#[test]
fn the_loop_hands_the_host_the_runs_latched_floor_and_its_session() {
    let mut driver = ScriptDriver::new(vec![
        step(ModelStep::MemoryWrite(claim("a fact")), 50),
        say("noted", 50),
    ]);
    let mut summarizer = EmptySummarizer;
    let mut tools = ScriptedTools::default();
    let mut approvals = FixedApprovals(true);
    let mut sink = CollectingSink::default();
    let mut control = marlowe_loop::NoControl;
    let mut clock = FrozenClock(1_700_000_000_000);
    let mut recorder = MemoryRecorder::default();
    let mut memory = RecordingMemory::default();

    let mut e = engine();
    let mut ports = Ports {
        escalations: None,
        driver: &mut driver,
        summarizer: &mut summarizer,
        tools: &mut tools,
        memory: Some(&mut memory),
        approvals: &mut approvals,
        sink: &mut sink,
        control: &mut control,
        clock: &mut clock,
        recorder: &mut recorder,
    };

    let mut run = Run::root(
        RunId::from_name("root"),
        SessionId::from_name("root-session"),
        CapabilityProfile::consolidation(),
        Budget::interactive(),
        OutputContract::answer(),
    );
    let session = run.session;
    let mut state = SessionState::new(session, "Marlowe.");
    state.assert_governance(GovernanceConstraint::asserted("c"));
    let mut prov = Provenance::new();
    let _ = e.run(&mut run, &mut state, &mut prov, &mut ports);

    assert_eq!(memory.sessions, vec![session], "a claim written to the wrong session is retrievable by nobody");
    assert_eq!(
        memory.floors.len(),
        1,
        "the floor must arrive with the claim, not be re-derived by the host"
    );
    assert!(
        memory.floors[0] <= TrustClass::AgentObserved,
        "the stable tier's Identity block is AgentObserved, so any real run's floor is at most \
         that. A floor of UserAsserted here would mean the view was never consulted: {:?}",
        memory.floors[0]
    );
    assert_eq!(memory.times, vec![1_700_000_000_000], "the host must use the run's clock");
}
