//! **Concurrency is granted on declared consequence, and this is where that is asserted.**
//!
//! `Engine::run_group` batches only maximal runs of consecutive `Inert` tools. The tempting
//! justification for batching a whole `ToolCall` message is the note on `ModelStep::ToolCall` —
//! *"no call here can have been shaped by another call's output"* — but that is a statement about
//! **data flow**, not about **side-effect ordering**. A model routinely emits `edit src/lib.rs`
//! alongside `bash cargo test`, having seen neither result, and those two must not overlap.
//!
//! So these tests assert the shape of what the loop hands the host: **what the host received**,
//! not what the engine declares about itself.

mod common;

use std::sync::{Arc, Mutex};

use common::*;
use marlowe_contract::TrustClass;
use marlowe_loop::{
    BatchItem, Budget, CapabilityProfile, Engine, MemoryRecorder, ModelStep, OutputContract, Ports,
    Provenance, Run, RunId, SessionId, SessionState, ToolBody, ToolHost, ToolInvocation, ToolOutcome,
};
use marlowe_permission::{Adjudication, ArgValue, Args, Tier, Unavailable};
use marlowe_tools::{builtin_registry, Metric, ResultSummary, ToolId};

/// Records the shape of every batch the loop delivers, and the order of tools within it.
#[derive(Default, Clone)]
struct Recording(Arc<Mutex<Vec<Vec<String>>>>);

struct Spy(Recording);

impl ToolHost for Spy {
    fn executes(&self) -> Vec<ToolId> {
        marlowe_tools::BUILTIN_TOOLS.iter().map(|t| ToolId::new(*t)).collect()
    }

    fn execute(&mut self, tool: &ToolId, _a: &Args, _adj: &Adjudication) -> ToolOutcome {
        // Reached only if the loop bypassed `execute_batch`, which would itself be a finding.
        self.0 .0.lock().unwrap().push(vec![format!("SINGLE:{tool}")]);
        ok()
    }

    fn execute_batch(&mut self, items: &[BatchItem<'_>]) -> Vec<ToolOutcome> {
        self.0
             .0
            .lock()
            .unwrap()
            .push(items.iter().map(|i| i.tool.to_string()).collect());
        items.iter().map(|_| ok()).collect()
    }
}

fn ok() -> ToolOutcome {
    ToolOutcome {
        summary: ResultSummary::new(vec![Metric::State("ok")]),
        body: ToolBody::Inline("ok".into()),
        trust: TrustClass::AgentObserved,
        failed: false,
        wall_ms: 0,
        preview: None,
    }
}

fn call(id: &str, tool: &str) -> ToolInvocation {
    // Arguments that will not adjudicate are fine: what is under test is the GROUPING the loop
    // performs before it asks anyone's permission, and a refused call is simply absent from the
    // group it would have joined. Every tool here is given a plausible target so that the
    // grouping, not an argument error, is what shapes the result.
    let args = match tool {
        "web" => Args::new().with("url", ArgValue::Text("https://example.com/".into())),
        "bash" => Args::new().with("command", ArgValue::Text("true".into())),
        _ => Args::new().with("path", ArgValue::Text("./notes.md".into())),
    };
    ToolInvocation { id: id.to_string(), tool: ToolId::new(tool), args }
}

fn drive(calls: Vec<ToolInvocation>) -> Vec<Vec<String>> {
    let recording = Recording::default();
    let mut engine = Engine::new(
        builtin_registry().expect("manifests load"),
        // `Unavailable` refuses every path, so filesystem tools are BLOCKED before execution.
        // That is deliberate here: it keeps the test off the disk while still exercising the
        // grouping, which happens before adjudication.
        Unavailable,
        100_000,
        10_000,
        std::path::PathBuf::from("/ws"),
        Tier::Act,
    );
    let mut driver = ScriptDriver::new(vec![
        step(ModelStep::ToolCall { calls }, 10),
        say("done", 5),
    ]);
    let mut tools = Spy(recording.clone());
    let mut run = Run::root(
        RunId::from_name("g"),
        SessionId::from_name("g"),
        CapabilityProfile::interactive(),
        Budget::interactive(),
        OutputContract::answer(),
    );
    let mut state = SessionState::new(run.session, "Marlowe.");
    let mut provenance = Provenance::default();
    let mut summarizer = EmptySummarizer;
    let mut gate = FixedApprovals(true);
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
        approvals: &mut gate,
        sink: &mut sink,
        control: &mut control,
        clock: &mut clock,
        recorder: &mut recorder,
    };
    let _ = engine.run(&mut run, &mut state, &mut provenance, &mut ports);
    let out = recording.0.lock().unwrap().clone();
    out
}

/// The case the whole feature exists for: many fetches arrive as ONE batch.
#[test]
fn a_run_of_inert_calls_is_delivered_as_a_single_batch() {
    let got = drive(vec![
        call("c1", "web"),
        call("c2", "web"),
        call("c3", "web"),
        call("c4", "web"),
    ]);
    assert_eq!(
        got,
        vec![vec!["web", "web", "web", "web"]],
        "four fetches must reach the host as one batch, not four"
    );
}

/// **The safety property.** A non-`Inert` tool must never share a batch with anything.
#[test]
fn a_mutating_call_is_never_batched_with_the_reads_around_it() {
    // `web` is Inert; `bash` is Irreversible.
    let got = drive(vec![
        call("c1", "web"),
        call("c2", "web"),
        call("c3", "bash"),
        call("c4", "web"),
    ]);
    let sizes: Vec<usize> = got.iter().map(|g| g.len()).collect();
    assert_eq!(
        sizes,
        vec![2, 1, 1],
        "expected [web web] -> [bash] -> [web], got {got:?}"
    );
    assert_eq!(got[1], vec!["bash"], "the mutating call must be alone: {got:?}");
}

/// **A call that is REFUSED still splits the run around it.**
///
/// `edit` is `Reversible`, and under this test's `Unavailable` path scope it is blocked before
/// execution — so it never reaches the host at all. Its *position* must still break the batch,
/// or a refused write would let the fetches on either side of it merge into one group and be
/// reordered relative to a write that a different scope would have allowed.
///
/// Ungrouped, the host would see a single batch of the three surviving fetches. Grouped, it sees
/// two. That difference is what makes this assertion non-vacuous.
#[test]
fn a_refused_mutating_call_still_splits_the_batch_around_it() {
    let got = drive(vec![
        call("c1", "web"),
        call("c2", "web"),
        call("c3", "edit"),
        call("c4", "web"),
    ]);
    let sizes: Vec<usize> = got.iter().map(|g| g.len()).collect();
    assert_eq!(sizes, vec![2, 1], "the refused edit must still split: {got:?}");
    assert_ne!(sizes, vec![3], "a single batch of 3 would mean the split never happened");
}

/// `bash` is `Irreversible`. It must be alone even when surrounded by reads.
#[test]
fn bash_is_never_batched() {
    let got = drive(vec![call("c1", "web"), call("c2", "bash"), call("c3", "web")]);
    let sizes: Vec<usize> = got.iter().map(|g| g.len()).collect();
    assert_eq!(sizes, vec![1, 1, 1], "got {got:?}");
    assert_eq!(got[1], vec!["bash"]);
}

/// Two mutating calls in a row stay in order and stay separate.
#[test]
fn consecutive_mutating_calls_are_not_merged_with_each_other() {
    let got = drive(vec![call("c1", "bash"), call("c2", "bash")]);
    assert_eq!(got, vec![vec!["bash"], vec!["bash"]], "got {got:?}");
}

/// A single call must not pay anything for the batching machinery existing.
#[test]
fn one_call_is_still_one_batch_of_one() {
    let got = drive(vec![call("c1", "web")]);
    assert_eq!(got, vec![vec!["web"]]);
}
