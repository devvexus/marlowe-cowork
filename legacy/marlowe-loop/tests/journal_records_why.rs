//! **A failed tool call's REASON reaches the journal.**
//!
//! `ResultSummary::render` is the §B6 line's right-hand side and is metrics only — it drops
//! `detail`, which is where the reason lives. So a failed call was journaled as
//! `{"tool":"read","summary":"read"}`: the tool's name, twice, and nothing else.
//!
//! Observed live 2026-08-26, journal seq 4677–4679. The model called `read` with neither `path`
//! nor `ref`. The executor refused it naming both, the model corrected itself, and the run
//! completed — everything worked, and the durable record of it said nothing at all. Reading that
//! journal afterwards cannot distinguish a refused call from a crashed one, which makes the
//! journal useless for the one job it has when a live run misbehaves.
//!
//! The journal is not model-reachable (invariant 8), which is what makes the full text safe here
//! where it is not safe in a parent's window.

use marlowe_contract::TrustClass;
use marlowe_journal::EventKind;
use marlowe_loop::{
    Block, Budget, CapabilityProfile, Engine, MemoryRecorder, ModelCall,
    ModelStep, OutputContract, Ports, Provenance, Run, RunId, SessionId, SessionState, SourceKind,
    ToolBody, ToolHost, ToolOutcome, Usage,
};
use marlowe_permission::{Adjudication, Args, Tier, Unavailable};
use marlowe_tools::{builtin_registry, Metric, ResultSummary, ToolId};

#[path = "common/mod.rs"]
mod common;
use common::*;

const WHY: &str = "read needs either `path` or `ref`, and this call supplied neither";

/// Fails every call, with a reason — the shape a real executor's refusal takes.
#[derive(Default)]
struct FailingTools {
    /// The control's switch: when false the outcome carries no detail at all, which is the state
    /// the product was in.
    with_detail: bool,
}

impl ToolHost for FailingTools {
    fn executes(&self) -> Vec<ToolId> {
        marlowe_tools::BUILTIN_TOOLS.iter().map(|t| ToolId::new(*t)).collect()
    }

    fn execute(&mut self, _tool: &ToolId, _args: &Args, _a: &Adjudication) -> ToolOutcome {
        ToolOutcome {
            summary: if self.with_detail {
                ResultSummary::with_detail(vec![Metric::State("failed")], WHY)
            } else {
                ResultSummary::new(vec![Metric::State("failed")])
            },
            body: ToolBody::Inline(String::new()),
            trust: TrustClass::AgentObserved,
            failed: true,
            wall_ms: 1,
            preview: None,
        }
    }
}

fn journal_of(with_detail: bool) -> MemoryRecorder {
    let mut e = Engine::new(
        builtin_registry().expect("the builtin manifests load"),
        Unavailable,
        100_000,
        10_000,
        std::path::PathBuf::from("/ws"),
        Tier::Act,
    );
    let mut driver = ScriptDriver::new(vec![
        ModelCall {
            usage: Usage { completion_tokens: 100, ..Usage::default() },
            step: ModelStep::one_call(ToolId::new("read"), Args::new()),
        },
        ModelCall {
            usage: Usage { completion_tokens: 100, ..Usage::default() },
            step: ModelStep::Say("I could not read it.".to_string()),
        },
    ]);
    let mut summarizer = EmptySummarizer;
    let mut tools = FailingTools { with_detail };
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
            SessionId::from_name("s"),
            CapabilityProfile::interactive(),
            Budget::interactive(),
            OutputContract::answer(),
        );
        let mut state = SessionState::new(run.session, "Marlowe.");
        state.push(Block::new(
            SourceKind::History,
            "read something".to_string(),
            TrustClass::UserAsserted,
        ));
        let mut prov = Provenance::new();
        e.run(&mut run, &mut state, &mut prov, &mut ports);
    }
    recorder
}

#[test]
fn a_failed_tool_calls_reason_is_in_the_journal() {
    let recorder = journal_of(true);
    let failures = recorder.payloads(EventKind::ToolFailed);
    assert_eq!(failures.len(), 1, "expected exactly one failure: {failures:?}");
    let payload = failures[0];

    assert_eq!(
        payload.get("detail").and_then(|d| d.as_str()),
        Some(WHY),
        "the journal records that `read` failed and not why. Live, that read \
         `{{\"tool\":\"read\",\"summary\":\"read\"}}` — the name twice: {payload:?}"
    );

    // **THE CONTROL FOR THE CONTROL.** An executor that never produced a detail would make the
    // assertion above test the fixture rather than the engine, so the same run with no detail must
    // journal no `detail` key — not an empty one, and not a default.
    let without = journal_of(false);
    let none = without.payloads(EventKind::ToolFailed);
    assert_eq!(none.len(), 1);
    assert!(
        none[0].get("detail").is_none(),
        "a `detail` key appeared for an outcome that carried none, so the assertion above could \
         not have failed: {:?}",
        none[0]
    );

    // A successful call keeps its detail OUT: a `read` that worked has the whole file in there,
    // and the journal is not a copy of every file the agent has opened.
    assert!(
        recorder.payloads(EventKind::ToolCompleted).is_empty(),
        "this fixture fails every call; a completion here means the outcome was misread"
    );
}
