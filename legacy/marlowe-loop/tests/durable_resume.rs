//! M3 Session A — a run survives, and the things that must survive with it.
//!
//! # Every test here has a control, and the controls are the point
//!
//! `CLAUDE.md`: *"a resume test needs a control proving the run actually died first. An assertion
//! whose subject is 'X came back' carries an assertion that X stopped."* So each resume test
//! asserts, on the same run, that phase one **did not finish** — the model was never asked for the
//! steps that produce the answer, and the answer is not in the window.
//!
//! The security tests carry the other kind of control: a negative case that *would* pass if the
//! mechanism were absent, so a green result is evidence about the mechanism rather than about the
//! setup. `the_trust_floor_survives_a_restart` is paired with
//! `a_clean_run_resumes_with_a_clean_floor`, which fails if `restore` ever hard-codes the floor to
//! its own worst case to make its sibling pass.

mod common;

use std::cell::Cell;
use std::fs;
use std::path::PathBuf;

use common::*;
use marlowe_contract::{Clock, TrustClass};
use marlowe_journal::{EventKind, Journal, Profile};
use marlowe_loop::{
    settle_orphan, Block, Budget, BudgetShare, CapabilityProfile, Checkpoint, CheckpointStore,
    ContextView, DurableControl, Engine, JournalCheckpoints, LoopOutcome, MemoryCheckpoints, ModelDriver,
    MemoryRecorder, ModelStep, OrphanOutcome, OrphanPolicy, OutputContract, Ports, Provenance,
    ResumeError, Run, RunControl, RunId, RunStatus, SessionId, SessionState, SourceKind,
    SteerMessage, Urgency,
};
use marlowe_permission::{Tier, Unavailable};

fn engine() -> Engine<Unavailable> {
    Engine::new(
        marlowe_tools::builtin_registry().expect("the eleven builtin manifests load"),
        Unavailable,
        100_000,
        10_000,
        PathBuf::from("/ws"),
        Tier::Act,
    )
}

fn root(name: &str, budget: Budget) -> Run {
    Run::root(
        RunId::from_name(name),
        SessionId::from_name(name),
        CapabilityProfile::interactive(),
        budget,
        OutputContract::answer(),
    )
}

/// A step that is not a completion. `ModelStep::Say` ends a run (M2 C2e: completion is the
/// absence of an action), so a run that must survive to a third iteration has to *do* something
/// twice. The call is refused by the `Unavailable` path scope, which is fine and is in fact the
/// cheaper shape: a refusal still costs an iteration and still checkpoints, with no tool host
/// behaviour in the way of what is being measured.
fn a_clock() -> Clock {
    Clock::new(1_780_000_000_000)
}

fn a_read(path: &str) -> ModelStep {
    ModelStep::one_call(marlowe_tools::ToolId::new("read"), marlowe_permission::Args::new().text("path", path))
}

fn tmp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("marlowe-durable-{name}"));
    let _ = fs::remove_dir_all(&dir);
    dir
}

/// **The kill, and the first version of it was the wrong instrument.**
///
/// A daemon that is `taskkill /F`-ed does not get to write anything: the newest thing in the
/// journal is the last per-iteration checkpoint, stamped `Running`, and the process is gone.
///
/// The first version used a `Control` that reported `cancelled` after N steps. That was a
/// **cancel**, not a crash — and once the loop started writing a final checkpoint carrying the
/// terminal status (which it must, or every completed run looks resumable forever), a cancelled
/// run correctly refused to resume and this test correctly failed. The instrument had been
/// modelling a different event all along; nothing showed it until the other half was right.
///
/// A panic inside the driver is the faithful model. `drive` never returns, so no final checkpoint
/// is written, and what survives is exactly what survives a `kill -9`.
struct DiesAfter {
    calls: Cell<u32>,
    at: u32,
}

impl ModelDriver for DiesAfter {
    fn call(
        &mut self,
        _view: &ContextView,
        _tools: &marlowe_tools::ExposedSet,
        _limits: marlowe_loop::CallLimits,
    ) -> Result<marlowe_loop::ModelCall, marlowe_loop::ProviderError> {
        let n = self.calls.get() + 1;
        self.calls.set(n);
        if n > self.at {
            panic!("MODELLED KILL: the daemon died during model call {n}");
        }
        Ok(step(a_read(&format!("f{n}.md")), 100))
    }

    fn failover(&mut self, _e: &marlowe_loop::ProviderError) -> bool {
        false
    }
}

/// Everything `Ports` needs, so a test reads as the thing it is testing.
macro_rules! ports {
    ($driver:expr, $sink:expr, $control:expr, $recorder:expr, $tools:expr, $clock:expr) => {{
        Ports {
            escalations: None,
            driver: $driver,
            summarizer: &mut EmptySummarizer,
            tools: $tools,
            memory: None,
            approvals: &mut FixedApprovals(true),
            sink: $sink,
            control: $control,
            clock: $clock,
            recorder: $recorder,
        }
    }};
}

// ─────────────────────────────────────────────────────────────────────────────────────────
// 1. The headline: a run dies mid-flight and comes back at its last completed step
// ─────────────────────────────────────────────────────────────────────────────────────────

#[test]
fn a_run_that_died_mid_flight_resumes_from_its_last_completed_step() {
    let dir = tmp("resume");
    let profile = Profile::init(&dir).unwrap();
    let journal = std::sync::Arc::new(std::sync::Mutex::new(Journal::open(&profile).unwrap()));
    // The clock every write in this test stamps with; see `a_clock`.
    let trace = uuid::Uuid::nil();

    // ── phase one: the daemon is alive, and dies during its third model call ──────────
    let run_id = RunId::from_name("phase-one");
    {
        let mut e = engine();
        let mut driver = DiesAfter { calls: Cell::new(0), at: 2 };
        let mut sink = CollectingSink::default();
        let mut control = marlowe_loop::NoControl;
        let mut clk = FrozenClock(1_780_000_000_000);
        let mut recorder =
            marlowe_loop::record::SharedJournalRecorder::new(std::sync::Arc::clone(&journal), trace);
        let mut tools = ScriptedTools::default();
        let mut ports = ports!(
            &mut driver,
            &mut sink,
            &mut control,
            &mut recorder,
            &mut tools,
            &mut clk
        );

        let mut run = root("phase-one", Budget::interactive());
        let mut state = SessionState::new(run.session, "Marlowe.");
        let mut prov = Provenance::new();
        prov.attribute_user_message("please compute the answer");
        state.push(Block::new(
            SourceKind::History,
            "please compute the answer",
            TrustClass::UserAsserted,
        ));

        // ── THE CONTROL. Without it the test below proves nothing: a run that never stopped
        //    "resuming" is just a run.
        let died = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            e.run(&mut run, &mut state, &mut prov, &mut ports)
        }));
        assert!(died.is_err(), "phase one must actually die; it returned {died:?}");
    }
    // The engine, the run, the session state and the provenance are all dropped here. Nothing
    // survives but the journal on disk -- which is the whole claim.

    // ── phase two: a NEW control plane, reading only the journal ───────────────────────
    let store = JournalCheckpoints::new(std::sync::Arc::clone(&journal));
    let mut control = DurableControl::new(store);
    control.resume(run_id).expect("a run that checkpointed twice must be resumable");
    let cp = control.take_resumed().expect("resume returned Ok and staged nothing");

    assert_eq!(cp.step, 2, "the resume point is the LAST COMPLETED step, not the one that died");

    let mut e = engine();
    let mut driver = ScriptDriver::new(vec![say("THE ANSWER IS 42", 100)]);
    let mut sink = CollectingSink::default();
    let mut control = control; // the durable control is the resumed run's control plane too
    let mut clk = FrozenClock(1_780_000_100_000);
    let mut recorder = MemoryRecorder::default();
    let mut tools = ScriptedTools::default();
    let mut ports = ports!(
        &mut driver,
        &mut sink,
        &mut control,
        &mut recorder,
        &mut tools,
        &mut clk
    );

    let (run, state, outcome) = e.resume_from(cp, &mut ports);

    assert!(matches!(outcome, LoopOutcome::Completed(_)), "the resumed run finished: {outcome:?}");
    assert_eq!(run.id, run_id, "the resumed run is the SAME run, not a new one wearing its work");
    let rendered = e.assembler().assemble(&state).rendered();
    assert!(
        rendered.contains("please compute the answer"),
        "the window came back with it: {rendered}"
    );
    assert!(rendered.contains("THE ANSWER IS 42"), "and the run finished the work: {rendered}");

    let _ = fs::remove_dir_all(&dir);
}

// ─────────────────────────────────────────────────────────────────────────────────────────
// 2. The security property: ADR-023's latch is not undone by a restart
// ─────────────────────────────────────────────────────────────────────────────────────────

#[test]
fn the_trust_floor_survives_a_restart() {
    // **The one that makes this an ADR.** `Run::root` starts at `UserAsserted`, so a resume that
    // rebuilt the run through it would restore every privilege the run had lost -- a daemon
    // restart doing what trimming the untrusted block used to do, which is the exact hole the
    // latch was written to close.
    let mut store = MemoryCheckpoints::new();
    let mut run = root("tainted", Budget::interactive());
    run.latch_trust_floor(TrustClass::UntrustedContent);
    assert_eq!(run.trust_floor(), TrustClass::UntrustedContent, "setup");
    store.write(&Checkpoint::capture(&run, &SessionState::default(), 4, 0), a_clock()).unwrap();
    drop(run);

    let mut control = DurableControl::new(store);
    control.resume(RunId::from_name("tainted")).unwrap();
    let back = control.take_resumed().unwrap().restore();

    assert_eq!(
        back.run.trust_floor(),
        TrustClass::UntrustedContent,
        "the latch is monotonic and PER RUN; a restart is not a new run"
    );
    assert!(
        marlowe_permission::blocks_composed_targets(back.run.trust_floor()),
        "and it still blocks composed targets, which is what the floor is FOR"
    );
}

#[test]
fn a_clean_run_resumes_with_a_clean_floor() {
    // **The control for the test above**, and it is not decoration: without it, a `restore` that
    // hard-coded `UntrustedContent` -- the safest-looking possible bug -- would pass the security
    // test and break the product, since every resumed run would refuse every composed target.
    let mut store = MemoryCheckpoints::new();
    let run = root("clean", Budget::interactive());
    store.write(&Checkpoint::capture(&run, &SessionState::default(), 1, 0), a_clock()).unwrap();

    let mut control = DurableControl::new(store);
    control.resume(run.id).unwrap();
    let back = control.take_resumed().unwrap().restore();

    assert_eq!(back.run.trust_floor(), TrustClass::UserAsserted);
    assert!(!marlowe_permission::blocks_composed_targets(back.run.trust_floor()));
}

#[test]
fn spend_and_the_step_counter_survive_a_restart() {
    // Two bounds that would each become per-restart rather than per-run. A budget that resets
    // means a run costs its declared ceiling every time the daemon bounces; a step counter that
    // resets means `MAX_STEPS` never fires on a run that restarts.
    let mut store = MemoryCheckpoints::new();
    let mut run = root("spent", Budget::interactive());
    run.spent.add(&Budget { tokens: 150_000, tool_calls: 7, ..Budget::default() });
    store.write(&Checkpoint::capture(&run, &SessionState::default(), 137, 3), a_clock()).unwrap();

    let mut control = DurableControl::new(store);
    control.resume(run.id).unwrap();
    let back = control.take_resumed().unwrap().restore();

    assert_eq!(back.run.spent.tokens, 150_000, "the spend came back");
    assert_eq!(back.run.spent.tool_calls, 7);
    assert_eq!(back.steps, 137, "the loop continues at 138, not at 1");
    assert_eq!(back.contract_retries, 3, "audit finding E8's cap is per run, so per RESUME too");
}

#[test]
fn a_quarantined_reader_cannot_come_back_holding_tools() {
    // The profile is checkpointed, and `CapabilityProfile`'s `Deserialize` routes through its
    // validating constructor. So the trifecta break survives the round trip **structurally**:
    // there is no shape a checkpoint can take that produces a reader with a tool.
    let parent = root("qr-parent", Budget::interactive());
    let child = Run::child(
        RunId::from_name("qr"),
        &parent,
        SessionId::from_name("qr"),
        CapabilityProfile::quarantined_reader(),
        Budget::interactive(),
        OrphanPolicy::Terminate,
        OutputContract::new("what it says", &["about"]),
    );
    let cp = Checkpoint::capture(&child, &SessionState::default(), 1, 0);

    // Through JSON, because that is the only way a checkpoint ever arrives.
    let json = serde_json::to_string(&cp).unwrap();
    let back: Checkpoint = serde_json::from_str(&json).unwrap();
    assert!(back.profile.reads_untrusted());
    assert!(back.profile.exposed_tools().is_empty(), "a reader with a tool is unconstructible");

    // ...and the forged version is refused at DECODE, not repaired into something valid.
    let forged = json.replace(r#""exposed_tools":[]"#, r#""exposed_tools":["bash"]"#);
    assert_ne!(forged, json, "the fixture must actually have been edited");
    assert!(
        serde_json::from_str::<Checkpoint>(&forged).is_err(),
        "a checkpoint claiming a quarantined reader with tools must not decode"
    );
}

// ─────────────────────────────────────────────────────────────────────────────────────────
// 3. The journal really is the substrate
// ─────────────────────────────────────────────────────────────────────────────────────────

#[test]
fn a_checkpoint_round_trips_through_the_signed_log() {
    let dir = tmp("roundtrip");
    let profile = Profile::init(&dir).unwrap();
    let journal = std::sync::Arc::new(std::sync::Mutex::new(Journal::open(&profile).unwrap()));
    // The clock every write in this test stamps with; see `a_clock`.

    let mut run = root("logged", Budget::interactive());
    run.latch_trust_floor(TrustClass::UntrustedContent);
    let mut state = SessionState::new(run.session, "Marlowe.");
    state.push(Block::new(SourceKind::History, "find the thing", TrustClass::UserAsserted));

    {
        let mut store = JournalCheckpoints::new(std::sync::Arc::clone(&journal));
        store.write(&Checkpoint::capture(&run, &state, 1, 0), a_clock()).unwrap();
        store.write(&Checkpoint::capture(&run, &state, 2, 0), a_clock()).unwrap();
    }

    // Re-open from disk. **The chain is verified on open**, so this also asserts the payload did
    // not break the signature -- a checkpoint that could not be verified would be worse than none.
    let reopened = Journal::open(&profile).unwrap();
    let store = JournalCheckpoints::new(std::sync::Arc::new(std::sync::Mutex::new(reopened)));
    let latest = store.latest(run.id).expect("the run's checkpoint is in the log");
    assert_eq!(latest.step, 2, "the LATEST wins, not the first");
    assert_eq!(latest.trust_floor, TrustClass::UntrustedContent);
    assert!(store.latest(RunId::from_name("someone-else")).is_none(), "runs do not share");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_pre_m3_checkpoint_payload_is_skipped_rather_than_fatal() {
    // The live journal holds 895 `{"step": n}` payloads. A build that refused to start on them
    // would turn this change into a migration; one that *interpreted* them would resume a run
    // with every security field at its default. Skipped is the third answer and the right one.
    let dir = tmp("legacy");
    let profile = Profile::init(&dir).unwrap();
    let journal = std::sync::Arc::new(std::sync::Mutex::new(Journal::open(&profile).unwrap()));
    // The clock every write in this test stamps with; see `a_clock`.

    journal
        .lock()
        .unwrap()
        .append(
            a_clock(),
            marlowe_journal::AppendRequest {
                trace_id: uuid::Uuid::nil(),
                session_id: Some("s".into()),
                run_id: Some(RunId::from_name("old").to_string()),
                actor: marlowe_journal::Actor::Harness,
                kind: EventKind::Checkpointed,
                payload: serde_json::json!({ "step": 12 }),
            },
        )
        .unwrap();

    let store = JournalCheckpoints::new(journal);
    assert!(store.latest(RunId::from_name("old")).is_none());

    let mut control = DurableControl::new(store);
    let run = RunId::from_name("old");
    assert_eq!(
        control.resume(run).unwrap_err(),
        ResumeError::NoCheckpoint { run },
        "an old payload is not a resumable one, and the refusal says so rather than guessing"
    );

    let _ = fs::remove_dir_all(&dir);
}

/// **Measured, not argued.** The module header claims one checkpoint is bounded by the window
/// rather than by the length of the run. This prints the number.
#[test]
fn a_checkpoint_is_bounded_by_the_window_not_by_the_run() {
    let mut run = root("sized", Budget::interactive());
    let mut state = SessionState::new(run.session, "Marlowe.");
    for i in 0..40 {
        state.push(Block::new(
            SourceKind::History,
            format!("turn {i}: {}", "x".repeat(500)),
            TrustClass::AgentInferred,
        ));
    }
    run.latch_trust_floor(TrustClass::AgentObserved);

    let at_10 = serde_json::to_string(&Checkpoint::capture(&run, &state, 10, 0)).unwrap().len();
    let at_300 = serde_json::to_string(&Checkpoint::capture(&run, &state, 300, 0)).unwrap().len();
    println!("checkpoint at step 10: {at_10} B; at step 300: {at_300} B");
    assert!(
        at_300 - at_10 < 32,
        "a checkpoint's size must not grow with the step count; it grew by {} B",
        at_300 - at_10
    );
    // The stated bound: a window this size is tens of kilobytes, not megabytes. If this ever
    // trips, the compression note in the module header has become work rather than an option.
    assert!(at_10 < 64 * 1024, "one checkpoint is {at_10} B");
}

/// **A run that survived and cannot be FOUND has not survived**, and this is the read that makes
/// it findable. The daemon's run table is in memory; after a restart it is empty, so without
/// `latest_per_run` a resumable run has no id anybody could produce.
#[test]
fn every_resumable_run_is_enumerable_and_the_finished_ones_are_distinguishable() {
    let mut store = MemoryCheckpoints::new();

    let live = root("enum-live", Budget::interactive());
    store.write(&Checkpoint::capture(&live, &SessionState::default(), 3, 0), a_clock()).unwrap();
    // A later checkpoint for the SAME run: the enumeration must collapse to one row per run,
    // or a long run would appear hundreds of times.
    store.write(&Checkpoint::capture(&live, &SessionState::default(), 4, 0), a_clock()).unwrap();

    let mut done = root("enum-done", Budget::interactive());
    done.status = RunStatus::Completed;
    store.write(&Checkpoint::capture(&done, &SessionState::default(), 9, 0), a_clock()).unwrap();

    let all = store.latest_per_run();
    assert_eq!(all.len(), 2, "one row per run, not one per checkpoint: {all:?}");
    let live_row = all.iter().find(|c| c.run == live.id).expect("the live run is listed");
    assert_eq!(live_row.step, 4, "the LATEST checkpoint wins");

    // The two are distinguishable, which is what lets the daemon seed only the resumable ones —
    // offering a resume that `RunControl::resume` refuses would be a control with one outcome.
    assert!(!live_row.is_terminal());
    assert!(all.iter().find(|c| c.run == done.id).unwrap().is_terminal());
}

// ─────────────────────────────────────────────────────────────────────────────────────────
// 4. Orphan policy — asserted on the CHILD's fate, never on the policy's value
// ─────────────────────────────────────────────────────────────────────────────────────────

fn a_live_child(name: &str) -> Checkpoint {
    let parent = root("orphan-parent", Budget::interactive());
    let mut child = Run::child(
        RunId::from_name(name),
        &parent,
        SessionId::from_name(name),
        CapabilityProfile::interactive(),
        Budget::interactive(),
        OrphanPolicy::Detach,
        OutputContract::new("findings", &["findings"]),
    );
    child.status = RunStatus::Running;
    Checkpoint::capture(&child, &SessionState::default(), 5, 0)
}

#[test]
fn terminate_leaves_a_child_that_refuses_to_resume() {
    let cp = a_live_child("t");
    let (amended, outcome) = settle_orphan(&cp, OrphanPolicy::Terminate).expect("a live child settles");
    assert_eq!(outcome, OrphanOutcome::Terminated { child: cp.run });

    let mut store = MemoryCheckpoints::new();
    store.write(&amended, a_clock()).unwrap();
    let mut control = DurableControl::new(store);
    assert_eq!(
        control.resume(cp.run).unwrap_err(),
        ResumeError::Terminated { run: cp.run },
        "the fate is observed on the CHILD: it will not come back"
    );
}

#[test]
fn detach_leaves_a_child_that_resumes_with_no_parent() {
    let cp = a_live_child("d");
    assert!(cp.parent.is_some(), "the control: it HAD a parent before settlement");
    let (amended, outcome) = settle_orphan(&cp, OrphanPolicy::Detach).unwrap();
    assert_eq!(outcome, OrphanOutcome::Detached { child: cp.run });

    let mut store = MemoryCheckpoints::new();
    store.write(&amended, a_clock()).unwrap();
    let mut control = DurableControl::new(store);
    control.resume(cp.run).expect("a detached child is still a run");
    let back = control.take_resumed().unwrap().restore();
    assert_eq!(back.run.parent, None, "it outlived its parent, which is the whole clause");
    assert_eq!(back.run.id, cp.run);
}

#[test]
fn adopt_leaves_a_child_that_resumes_under_the_named_run() {
    let cp = a_live_child("a");
    let new_parent = RunId::from_name("the-adopter");
    let (amended, outcome) = settle_orphan(&cp, OrphanPolicy::Adopt { by: new_parent }).unwrap();
    assert_eq!(outcome, OrphanOutcome::Adopted { child: cp.run, by: new_parent });

    let mut store = MemoryCheckpoints::new();
    store.write(&amended, a_clock()).unwrap();
    let mut control = DurableControl::new(store);
    control.resume(cp.run).unwrap();
    let back = control.take_resumed().unwrap().restore();
    assert_eq!(back.run.parent, Some(new_parent));
    assert_ne!(back.run.parent, cp.parent, "and it is a DIFFERENT parent, not the original");
}

#[test]
fn the_three_policies_are_distinguishable_from_the_child_alone() {
    // **The control for the three tests above, together.** Each of them could pass against a
    // `settle_orphan` that ignored its argument and did the same thing three times, as long as
    // that thing happened to satisfy the one assertion. This fails unless the outcomes differ.
    let cp = a_live_child("distinct");
    let t = settle_orphan(&cp, OrphanPolicy::Terminate).unwrap().0;
    let d = settle_orphan(&cp, OrphanPolicy::Detach).unwrap().0;
    let a = settle_orphan(&cp, OrphanPolicy::Adopt { by: RunId::from_name("x") }).unwrap().0;
    assert_ne!(t, d);
    assert_ne!(d, a);
    assert_ne!(t, a);
    assert_eq!(t.status, RunStatus::Cancelled);
    assert_eq!(d.parent, None);
    assert_eq!(a.parent, Some(RunId::from_name("x")));
}

#[test]
fn a_child_that_already_finished_is_not_settled() {
    // Marking a completed run cancelled because its parent later ended would rewrite history.
    let mut cp = a_live_child("done");
    cp.status = RunStatus::Completed;
    assert!(settle_orphan(&cp, OrphanPolicy::Terminate).is_none());
    assert!(settle_orphan(&cp, OrphanPolicy::Detach).is_none());
}

#[test]
fn a_parent_completing_settles_its_children_through_the_loop() {
    // The other half: the policy is read by the ENGINE at the moment a parent ends, not only by a
    // function a test can call. Without `settle_children` this whole file would assert on
    // machinery nothing invokes -- the `SourceKind::Skills` failure applied to orphan policy.
    //
    // **The child must NOT finish, and the first version of this test did not know that.** It
    // spawned a child that completed, and passed -- because at the time a completed run's last
    // checkpoint still said `Running`, so settlement fired on a run that had nothing to settle.
    // The test was green *because of* the defect it should have been indifferent to. A tiny grant
    // makes the child pause after one step, which is a child that genuinely outlives its parent.
    let mut e = engine();
    let mut driver = ScriptDriver::new(vec![
        step(
            ModelStep::Spawn(marlowe_loop::SpawnRequest {
                task: "go and look".into(),
                contract: OutputContract::new("findings", &["findings"]),
                orphan: OrphanPolicy::Detach,
                share: BudgetShare::Standard,
                // Enough for one call and not two. **The figure is derived, not typed**:
                // `MIN_CHILD_TOKENS` is the floor `Budget::grant` enforces, and the child's one
                // scripted step below is sized to leave less than `MIN_CALL_TOKENS` behind it.
                // Writing 600 here was fine until the floor existed, and then it was a refusal
                // rather than a starved child -- a test measuring a different thing than its name.
                grant_tokens: Some(marlowe_loop::MIN_CHILD_TOKENS),
                tools: vec![],
                reads_untrusted: false,
                tools_declared: true,
                role: marlowe_loop::ModelRoute::Worker,
                disposition: marlowe_loop::Disposition::Work,
            }),
            100,
        ),
        // Spends all but 401 of the child's grant, which is under `MIN_CALL_TOKENS` -- so the
        // child pauses after exactly one call and is still live when the parent ends.
        step(a_read("child-step.md"), marlowe_loop::MIN_CHILD_TOKENS - 401),
        say("parent done", 100),
    ]);
    let mut sink = CollectingSink::default();
    let mut control = marlowe_loop::NoControl;
    let mut clk = FrozenClock(1_780_000_000_000);
    let mut recorder = MemoryRecorder::default();
    let mut tools = ScriptedTools::default();
    let mut ports = ports!(&mut driver, &mut sink, &mut control, &mut recorder, &mut tools, &mut clk);

    let mut run = root("settling-parent", Budget::interactive());
    let mut state = SessionState::new(run.session, "Marlowe.");
    let mut prov = Provenance::new();
    e.run(&mut run, &mut state, &mut prov, &mut ports);

    let fates: Vec<&serde_json::Value> = recorder.payloads(EventKind::RunCompleted);
    assert!(
        fates.iter().any(|p| p.get("fate").and_then(|f| f.as_str()) == Some("detached")),
        "the parent ended and nothing recorded the child's declared fate: {fates:?}"
    );
}

/// **The control for the final checkpoint**, and it is what the live demo found the hard way.
///
/// The per-iteration checkpoint is written at the END of an iteration, while the run is still
/// `Running`; the terminal status is set after the loop. Without a final checkpoint the last
/// durable record of a run that finished perfectly says it was still going — so every consumer
/// reads it as resumable. The demo listed **three completed turns as interrupted** and resumed
/// one, re-running finished work and producing a second, different answer.
#[test]
fn a_run_that_completed_leaves_a_terminal_checkpoint_and_refuses_to_resume() {
    let mut e = engine();
    let mut driver = ScriptDriver::new(vec![say("done", 100)]);
    let mut sink = CollectingSink::default();
    let mut control = DurableControl::new(MemoryCheckpoints::new());
    let mut clk = FrozenClock(1_780_000_000_000);
    let mut recorder = MemoryRecorder::default();
    let mut tools = ScriptedTools::default();

    let mut run = root("finished", Budget::interactive());
    let mut state = SessionState::new(run.session, "Marlowe.");
    let mut prov = Provenance::new();
    {
        let mut ports =
            ports!(&mut driver, &mut sink, &mut control, &mut recorder, &mut tools, &mut clk);
        let outcome = e.run(&mut run, &mut state, &mut prov, &mut ports);
        assert!(matches!(outcome, LoopOutcome::Completed(_)), "setup: {outcome:?}");
    }

    let cp = e.last_checkpoint_of(run.id).expect("a finished run still checkpoints");
    assert_eq!(cp.status, RunStatus::Completed, "the LAST checkpoint carries the terminal status");
    assert!(cp.is_terminal(), "so nothing downstream offers to resume it");

    // ...and the control plane refuses by name rather than re-running finished work.
    control.checkpoint(cp, a_clock()).unwrap();
    let e2 = control.resume(run.id).unwrap_err();
    assert_eq!(e2, ResumeError::AlreadyFinished { run: run.id, status: "completed".into() });
}

// ─────────────────────────────────────────────────────────────────────────────────────────
// 5. Steering a running child, with no restart
// ─────────────────────────────────────────────────────────────────────────────────────────

#[test]
fn a_running_child_takes_a_steer_addressed_to_it_without_restarting() {
    let parent = root("steer-parent", Budget::interactive());
    let mut child = Run::child(
        RunId::from_name("steer-child"),
        &parent,
        SessionId::from_name("steer-child"),
        CapabilityProfile::interactive(),
        Budget::interactive(),
        OrphanPolicy::Detach,
        OutputContract::new("findings", &["findings"]),
    );

    let mut control = DurableControl::new(MemoryCheckpoints::new());
    // Addressed to the CHILD. The parent is not in the room.
    control.steer(
        child.id,
        SteerMessage { text: "only the 2024 filings".into(), urgency: Urgency::Advisory },
    );
    assert_eq!(control.pending_steers(child.id), 1);
    assert_eq!(control.pending_steers(parent.id), 0, "the parent's queue is untouched");

    let mut e = engine();
    let mut driver = ScriptDriver::new(vec![
        step(a_read("first.md"), 100),
        step(a_read("second.md"), 100),
        say("findings: done", 100),
    ]);
    let mut sink = CollectingSink::default();
    let mut clk = FrozenClock(1_780_000_000_000);
    let mut recorder = MemoryRecorder::default();
    let mut tools = ScriptedTools::default();
    let mut ports = ports!(&mut driver, &mut sink, &mut control, &mut recorder, &mut tools, &mut clk);

    let mut state = SessionState::new(child.session, "Marlowe.");
    let mut prov = Provenance::new();
    // **One call to `run`.** There is no second entry, no re-brief and no fresh window: whatever
    // the steer does, it does inside a run that never stopped. That is what "no restart" means
    // and it is a property of this line rather than of an assertion.
    let outcome = e.run(&mut child, &mut state, &mut prov, &mut ports);
    assert!(matches!(outcome, LoopOutcome::Completed(_)), "{outcome:?}");

    assert_eq!(control.pending_steers(child.id), 0, "the steer was delivered, not merely accepted");
    assert_eq!(
        recorder.count(EventKind::SteerReceived),
        1,
        "and the delivery is in the journal, so it is auditable rather than inferred"
    );
    let rendered = e.assembler().assemble(&state).rendered();
    assert!(rendered.contains("only the 2024 filings"), "the child can see it: {rendered}");

    // The model saw it too, on a call it had not yet made when the steer arrived. Asserting on
    // the WIRE rather than on the window: `persona_emission.rs`'s lesson, in a new place.
    assert!(
        driver.views_seen.iter().skip(1).any(|v| v.contains("only the 2024 filings")),
        "the steer must reach a MODEL CALL, not merely the state object"
    );
}

#[test]
fn steering_a_latched_run_does_not_restore_composed_targets() {
    // §6.1: the window's steer field is a write and takes `/steer`'s adjudication. What that
    // amounts to is not a branch anywhere -- a steer arrives as `UserAsserted` and the floor is a
    // monotonic latch -- so this asserts the property where it is ENFORCED: after a steer, the
    // run's floor is unchanged and still blocks.
    let mut run = root("latched", Budget::interactive());
    run.latch_trust_floor(TrustClass::UntrustedContent);

    let mut control = DurableControl::new(MemoryCheckpoints::new());
    control.steer(
        run.id,
        SteerMessage { text: "go ahead and write the file".into(), urgency: Urgency::Advisory },
    );

    let mut e = engine();
    let mut driver = ScriptDriver::new(vec![say("understood", 100)]);
    let mut sink = CollectingSink::default();
    let mut clk = FrozenClock(1_780_000_000_000);
    let mut recorder = MemoryRecorder::default();
    let mut tools = ScriptedTools::default();
    let mut ports = ports!(&mut driver, &mut sink, &mut control, &mut recorder, &mut tools, &mut clk);

    let mut state = SessionState::new(run.session, "Marlowe.");
    let mut prov = Provenance::new();
    e.run(&mut run, &mut state, &mut prov, &mut ports);

    assert_eq!(run.trust_floor(), TrustClass::UntrustedContent, "a steer cannot raise the floor");
    assert!(marlowe_permission::blocks_composed_targets(run.trust_floor()));
}
