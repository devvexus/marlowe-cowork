//! **A child created by `run` appears in `/runs` while it is alive.** ADR-057, M3 Session B1.
//!
//! # The property, and the two ways of asserting it that would not count
//!
//! `Daemon::ask_streaming_with` inserts exactly one row — the turn the daemon accepted — and until
//! this session that was the only writer of the live run table. `Engine::spawn` creates children,
//! journals them, and knows nothing about a control plane. So a spawn produced a run that existed
//! in the log and in no listing, and Session F's roster panel could only ever read
//! `subagents — none`.
//!
//! **Not asserted by feeding `RosterRecorder` a hand-built `RunSpawned` payload.** That tests the
//! projection against a shape this file wrote, and it would stay green if `Engine::spawn` changed
//! the payload or stopped emitting it — the "testing the function, not the wiring" failure this
//! project logged again this week.
//!
//! **Not asserted by hand-building a `ModelStep::Spawn` either.** That is the state the product
//! could not enter for the whole of M2 and the reason this session exists.
//!
//! So: a model reply naming `run`, through the shipped `parse_step`, into a real `Engine`, whose
//! recorder is the one the daemon actually installs, over a real signed journal. What is asserted
//! is the frame `/runs` renders.
//!
//! It does **not** stand up a daemon. `control_plane.rs`'s fixture builds a whole `Daemon::open`
//! per test — memory subsystem, embedder and all — and STATE.md records that ten of them under a
//! `--jobs 4` workspace run took 413 seconds and still timed out. A journal, a plane and an engine
//! are the parts this property actually involves.

#[path = "../../marlowe-loop/tests/common/mod.rs"]
mod common;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use common::*;
use marlowe_daemon::control_plane::ControlPlane;
use marlowe_daemon::roster::RosterRecorder;
use marlowe_journal::{Journal, Profile};
use marlowe_loop::{
    Budget, CapabilityProfile, DurableControl, Engine, JournalCheckpoints, ModelCall,
    OutputContract, Ports, Provenance, Run, RunId, SessionId, SessionState, Usage,
};
use marlowe_permission::{Tier, Unavailable};
use marlowe_provider::ollama::parse_step;
use marlowe_daemon::protocol::Event;
use marlowe_tools::builtin_registry;

/// A profile root for one test.
///
/// **`Profile::init` refuses a non-empty root**, and a previous run's files survive when its
/// journal handle outlived the panic that ended it — so the remove is best-effort and the open is
/// the fallback, which is the pattern `memory_durability.rs` already uses. A test that failed once
/// must not go on failing for a reason that has nothing to do with what it asserts.
fn fresh_profile(name: &str) -> (PathBuf, Profile) {
    let dir = std::env::temp_dir().join(format!("marlowe-roster-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    let profile = if dir.join("profile.json").exists() {
        Profile::open(&dir).expect("an existing profile opens")
    } else {
        Profile::init(&dir).expect("a fresh profile")
    };
    (dir, profile)
}

/// **`wall_ms` is non-zero on purpose.** A reply that costs no wall time leaves a run's final
/// elapsed at `0`, which is the same value as "not finished yet" — the ambiguity `detail` now
/// resolves by status. Asserting against a real number keeps the elapsed test about the elapsed
/// rather than about that tie-break, which has its own assertion.
fn reply(message: serde_json::Value, tokens: u64) -> ModelCall {
    ModelCall {
        usage: Usage { completion_tokens: tokens, wall_ms: 250, ..Usage::default() },
        step: parse_step(&message),
    }
}

fn run_call(args: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "content": "",
        "tool_calls": [{ "function": { "name": "run", "arguments": args } }],
    })
}

/// Every `Event::Run` frame the control port would answer `/runs` with.
fn listed(plane: &marlowe_daemon::control_plane::Shared) -> Vec<(String, String, u8)> {
    plane
        .lock()
        .expect("the plane lock")
        .run_frames()
        .into_iter()
        .filter_map(|e| match e {
            Event::Run { id, status, depth, .. } => Some((id, status, depth)),
            _ => None,
        })
        .collect()
}

#[test]
fn a_child_spawned_from_a_model_reply_is_listed_in_runs() {
    let (root_dir, profile) = fresh_profile("listed");
    let journal = Arc::new(Mutex::new(Journal::open(&profile).expect("the journal opens")));
    let plane = ControlPlane::new(DurableControl::new(JournalCheckpoints::new(Arc::clone(
        &journal,
    ))));

    // **The control, before anything runs.** An empty listing here is what makes a non-empty one
    // afterwards evidence — without it, a table seeded from somewhere else would read the same.
    assert!(listed(&plane).is_empty(), "the run table starts empty: {:?}", listed(&plane));

    let mut e = Engine::new(
        builtin_registry().expect("the builtins load"),
        Unavailable,
        100_000,
        10_000,
        PathBuf::from("/ws"),
        Tier::Act,
    );
    let mut parent = Run::root(
        RunId::from_name("roster-parent"),
        SessionId::from_name("roster-session"),
        CapabilityProfile::interactive(),
        Budget::interactive(),
        OutputContract::answer(),
    );

    let mut driver = ScriptDriver::new(vec![
        reply(run_call(serde_json::json!({ "task": "go and count them", "exposed_tools": "" })), 100),
        reply(serde_json::json!({ "content": "nineteen" }), 100),
        reply(serde_json::json!({ "content": "nineteen." }), 100),
    ]);
    let mut summarizer = EmptySummarizer;
    let mut tools = ScriptedTools::default();
    let mut approvals = FixedApprovals(true);
    let mut sink = CollectingSink::default();
    let mut control = marlowe_loop::NoControl;
    let mut clock = FrozenClock(1_700_000_000_000);
    // **The recorder the daemon installs**, not a test double. This is the whole wiring under test:
    // if `ask_streaming_with` stopped wrapping the journal recorder, this file would go on passing
    // — which is why `the_daemon_installs_this_recorder` below reads the composition root.
    let mut recorder = RosterRecorder::new(
        marlowe_loop::record::SharedJournalRecorder::new(
            Arc::clone(&journal),
            parent.trace_id,
        ),
        Arc::clone(&plane),
    );

    let mut state = SessionState::new(parent.session, "Marlowe.");
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
        let _ = e.run(&mut parent, &mut state, &mut prov, &mut ports);
    }

    let rows = listed(&plane);
    assert_eq!(
        rows.len(),
        1,
        "exactly the child — the parent's own row is inserted by `ask_streaming_with`, which this \
         test does not go through, so one row here is the child and only the child: {rows:?}"
    );
    let (id, status, depth) = &rows[0];
    assert_ne!(
        id,
        &parent.id.to_string(),
        "the row is the child's, not the parent's"
    );
    assert_eq!(
        status, "completed",
        "the child finished and the listing must say so rather than leaving it running forever"
    );
    assert!(
        *depth < Budget::interactive().depth,
        "a child's remaining depth is less than its parent's: {depth}"
    );

    // The id in the listing is the id the journal recorded, so `/watch <id>` on this row addresses
    // the run that actually happened.
    let spawned: Vec<String> = journal
        .lock()
        .expect("the journal lock")
        .replay(
            &marlowe_journal::OperatorCapability::for_operator_or_audit(),
            Some(marlowe_journal::EventKind::RunSpawned),
        )
        .expect("replay")
        .into_iter()
        .filter_map(|(_, _, p)| p.get("child").and_then(|c| c.as_str()).map(str::to_string))
        .collect();
    assert_eq!(spawned, vec![id.clone()], "the listed id is the journalled child id");

    let _ = std::fs::remove_dir_all(&root_dir);
}

/// **The wiring, read at the composition root.** The test above proves `RosterRecorder` fills the
/// table; it cannot see whether the daemon installs one. That is the `persona_emission.rs` lesson —
/// a test on the source cannot see what the running process actually assembled — and the cheapest
/// honest guard is to read the source of the one function that builds the loop's ports.
#[test]
fn the_daemon_installs_this_recorder_at_the_composition_root() {
    let src = include_str!("../src/daemon.rs");
    assert!(
        src.contains("RosterRecorder::new"),
        "`ask_streaming_with` no longer wraps the journal recorder, so no spawned child reaches \
         `/runs` — and every assertion in this file would still pass"
    );
}

/// **A finished child's elapsed time stops.** The regression for the one defect the first live
/// spawn produced, and it was found by running the product rather than by any of the eighteen
/// tests written for this feature.
///
/// `ControlPlane::detail` reads *"final when there is one, live otherwise"*: a row whose
/// `elapsed_ms` is `0` is reported as `now - started_ms`. That is right for a run still going and
/// wrong the instant one stops. `RosterRecorder` inserted the child's row and never closed it, so
/// the first real spawn reported:
///
/// ```text
/// run humble-summit   status completed   elapsed 32804 ms   parent fc164ca9-…
/// run upper-sparrow   status completed   elapsed  3536 ms
/// ```
///
/// — a completed child that took nine times as long as the parent blocked on it, and which would
/// have read longer still on the next `--runs`. Neither number is impossible on its own, which is
/// why this needed a live run to see: the pair is what is absurd.
///
/// **The assertion is that the answer does not move**, not that it equals a particular value. A
/// fixed number would be a test of the model's speed; two reads of the same finished run are the
/// property.
#[test]
fn a_finished_childs_elapsed_is_final_rather_than_growing() {
    let (root_dir, profile) = fresh_profile("elapsed");
    let journal = Arc::new(Mutex::new(Journal::open(&profile).expect("the journal opens")));
    let plane = ControlPlane::new(DurableControl::new(JournalCheckpoints::new(Arc::clone(
        &journal,
    ))));

    let mut e = Engine::new(
        builtin_registry().expect("the builtins load"),
        Unavailable,
        100_000,
        10_000,
        PathBuf::from("/ws"),
        Tier::Act,
    );
    let mut parent = Run::root(
        RunId::from_name("elapsed-parent"),
        SessionId::from_name("elapsed-session"),
        CapabilityProfile::interactive(),
        Budget::interactive(),
        OutputContract::answer(),
    );
    let mut driver = ScriptDriver::new(vec![
        reply(run_call(serde_json::json!({ "task": "go and count them", "exposed_tools": "" })), 100),
        reply(serde_json::json!({ "content": "nineteen" }), 100),
        reply(serde_json::json!({ "content": "nineteen." }), 100),
    ]);
    let mut summarizer = EmptySummarizer;
    let mut tools = ScriptedTools::default();
    let mut approvals = FixedApprovals(true);
    let mut sink = CollectingSink::default();
    let mut control = marlowe_loop::NoControl;
    let mut clock = FrozenClock(1_700_000_000_000);
    let mut recorder = RosterRecorder::new(
        marlowe_loop::record::SharedJournalRecorder::new(Arc::clone(&journal), parent.trace_id),
        Arc::clone(&plane),
    );
    let mut state = SessionState::new(parent.session, "Marlowe.");
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
        let _ = e.run(&mut parent, &mut state, &mut prov, &mut ports);
    }

    let child = {
        let rows = listed(&plane);
        assert_eq!(rows.len(), 1, "the child's row: {rows:?}");
        // The plane's own resolver, which is what `--runs <id>` and `/watch` go through.
        plane.lock().expect("the plane lock").resolve(&rows[0].0).expect("the listed id resolves")
    };

    let elapsed_of = |id| match plane.lock().expect("the plane lock").detail(id) {
        Event::RunDetail { elapsed_ms, status, .. } => (elapsed_ms, status),
        other => panic!("expected a RunDetail, got {other:?}"),
    };

    let (first, status) = elapsed_of(child);
    assert_eq!(status, "completed", "the child finished");

    // **Two fixes, so two assertions.** `roster.rs` closes the row from the child's last
    // checkpoint, and `detail` stops timing a stopped run live. Either alone leaves a stable
    // answer, so "it did not move" cannot tell them apart — a mutation proved exactly that. This
    // is the first half: the number is the child's own measured wall time, not zero.
    //
    // The child made one model call and `reply` costs 250 ms of it, so this is the run's real
    // spend rather than a figure the listing invented. The bound is what a live clock reading
    // could not satisfy: `started_ms` is a frozen 1.7e12, so the live branch returns a number in
    // the tens of billions.
    assert!(
        first > 0 && first < 60_000,
        "the child's elapsed is {first} ms — zero means the row was never closed, and a huge \
         value means `detail` is still subtracting `started_ms` from the system clock"
    );

    // **The control.** Wall-clock has to have moved between the two reads, or "unchanged" is what
    // two reads a microsecond apart would say regardless. `detail`'s live branch reads the system
    // clock through `clock.rs`'s fence, so a real pause is the only thing that separates them.
    std::thread::sleep(std::time::Duration::from_millis(40));

    let (second, _) = elapsed_of(child);
    assert_eq!(
        first, second,
        "a finished run's elapsed moved between two reads, so `detail` is still computing it live \
         from `started_ms` — the row was never closed"
    );

    let _ = std::fs::remove_dir_all(&root_dir);
}

/// **§6.3's roster panel has a producer.** `RunView::subagents` was a hardcoded `Vec::new()` — an
/// empty roster because nothing filled it, indistinguishable from an empty roster because the run
/// had no children, and the product was in the first state for the whole of M2.
///
/// Asserted end to end: a model reply spawns a child, the control plane's `detail` names it, and
/// the projection that feeds a run window turns it into an item carrying the child's id.
///
/// **The control is the parent-and-child pair, not the child alone.** Watching the *child* must
/// still show an empty roster — a `children_of` that returned every run it knew, or that folded on
/// the wrong side of `parent`, would fill both and look right on whichever one was asserted first.
#[test]
fn a_run_windows_roster_names_the_children_and_a_childless_run_names_none() {
    let (root_dir, profile) = fresh_profile("roster");
    let journal = Arc::new(Mutex::new(Journal::open(&profile).expect("the journal opens")));
    let plane = ControlPlane::new(DurableControl::new(JournalCheckpoints::new(Arc::clone(
        &journal,
    ))));

    let mut e = Engine::new(
        builtin_registry().expect("the builtins load"),
        Unavailable,
        100_000,
        10_000,
        PathBuf::from("/ws"),
        Tier::Act,
    );
    let mut parent = Run::root(
        RunId::from_name("roster-panel-parent"),
        SessionId::from_name("roster-panel-session"),
        CapabilityProfile::interactive(),
        Budget::interactive(),
        OutputContract::answer(),
    );
    let mut driver = ScriptDriver::new(vec![
        reply(run_call(serde_json::json!({ "task": "go and count them", "exposed_tools": "" })), 100),
        reply(serde_json::json!({ "content": "nineteen" }), 100),
        reply(serde_json::json!({ "content": "nineteen." }), 100),
    ]);
    let mut summarizer = EmptySummarizer;
    let mut tools = ScriptedTools::default();
    let mut approvals = FixedApprovals(true);
    let mut sink = CollectingSink::default();
    let mut control = marlowe_loop::NoControl;
    let mut clock = FrozenClock(1_700_000_000_000);
    let mut recorder = RosterRecorder::new(
        marlowe_loop::record::SharedJournalRecorder::new(Arc::clone(&journal), parent.trace_id),
        Arc::clone(&plane),
    );
    let mut state = SessionState::new(parent.session, "Marlowe.");
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
        let _ = e.run(&mut parent, &mut state, &mut prov, &mut ports);
    }

    let rows = listed(&plane);
    assert_eq!(rows.len(), 1, "the child's row: {rows:?}");
    let child_id = rows[0].0.clone();

    // ── the view a run window actually draws ──────────────────────────────────────────
    let view_of = |id: RunId| {
        let detail = plane.lock().expect("the plane lock").detail(id);
        let mut projection = marlowe_daemon::watch_client::RunProjection::new(id.to_string());
        projection.apply(&[detail]);
        projection.view().expect("a detail was absorbed, so there is a view")
    };

    let parent_view = view_of(parent.id);
    assert_eq!(
        parent_view.subagents.len(),
        1,
        "the parent's roster is empty, so the panel still reads `subagents — none` on a run that \
         has a child"
    );
    let item = &parent_view.subagents[0];
    assert_eq!(
        item.id.as_deref(),
        Some(child_id.as_str()),
        "the roster item must carry the child's UUID: two live runs can share a mnemonic, and a \
         panel folded by name would merge them"
    );
    assert!(
        item.label.contains('-'),
        "the label is the sayable name, not the raw id: {:?}",
        item.label
    );
    assert!(
        item.lines.iter().any(|(t, _)| t == "completed"),
        "the child's state is not on its row: {:?}",
        item.lines
    );

    // **The control.** The child has no children, so its own window's roster is empty — and it is
    // empty for the right reason, since the parent's is not.
    //
    // **The id is bound before the call, and that is not style.** Written as
    // `view_of(plane.lock()...resolve(&child_id)...)` the `MutexGuard` temporary lives until the
    // end of the enclosing statement — which is *after* `view_of` returns — and `view_of` locks
    // the same plane. `std::sync::Mutex` is not reentrant, so that deadlocks on one thread, with
    // no panic and no output: the test binary simply never exits, and `cargo` reports nothing
    // until somebody notices the process. It cost this session a ten-minute hang and a `LNK1104`
    // on the next build, because killing cargo does not kill the binary it spawned.
    let child_run = plane.lock().expect("the plane lock").resolve(&child_id).expect("resolves");
    let child_view = view_of(child_run);
    assert!(
        child_view.subagents.is_empty(),
        "the childless run's roster is not empty, so `children_of` is not filtering on `parent`: \
         {:?}",
        child_view.subagents
    );

    let _ = std::fs::remove_dir_all(&root_dir);
}
