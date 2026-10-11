//! **A failed tool call's reason reaches the model's own window.**
//!
//! `failed()` builds its outcome with `body: ToolBody::Inline(String::new())` and puts the reason
//! in `summary.detail`. The block the loop pushes was built from `summary.render()` — the §B6
//! line's metrics — and the body. **Neither is the detail.** So a failed `edit` arrived in the
//! model's context as the literal string `"edit · "`: its own verb, and a separator.
//!
//! Watched live 2026-08-26, journal seq 4813-4859. The model tried to write a new file, was told
//! `"edit · "`, read the file back, tried again, read, read, and fell back to `bash`. Six calls and
//! three minutes. The handoff it then wrote reported *"no truncation, errors or refusals occurred
//! anywhere along execution path"* and invented a Windows filename-parsing cause — because from
//! inside the window there was nothing else to go on.
//!
//! **A REFUSAL was never affected**, which is exactly why this survived so long: `tool_error`
//! formats `[{tool} blocked] {why}` and has always carried its reason. Only executor failures were
//! silent — and those are the ones the model is supposed to correct rather than abandon.

use marlowe_contract::TrustClass;
use marlowe_loop::{
    Block, Budget, CapabilityProfile, Engine, MemoryRecorder, ModelCall, ModelStep,
    OutputContract, Ports, Provenance, Run, RunId, SessionId, SessionState, SourceKind, ToolBody,
    ToolHost, ToolOutcome, Usage,
};
use marlowe_permission::scope::WorkspaceScope;
use marlowe_permission::{Adjudication, Args, Tier};
use marlowe_tools::{builtin_registry, Metric, ResultSummary, ToolId};

#[path = "common/mod.rs"]
mod common;
use common::*;

const WHY: &str = "the file is empty, so there is nothing to replace";

/// Fails like `marlowe_exec::failed` does: an EMPTY body, with the reason in `detail`. Copying
/// that shape is the point — a fixture that put the reason in the body would test nothing.
struct FailsLikeAnExecutor {
    detail: Option<&'static str>,
}

impl ToolHost for FailsLikeAnExecutor {
    fn executes(&self) -> Vec<ToolId> {
        marlowe_tools::BUILTIN_TOOLS.iter().map(|t| ToolId::new(*t)).collect()
    }
    fn execute(&mut self, _t: &ToolId, _a: &Args, _adj: &Adjudication) -> ToolOutcome {
        ToolOutcome {
            summary: match self.detail {
                Some(d) => ResultSummary::with_detail(vec![Metric::State("edit")], d),
                None => ResultSummary::new(vec![Metric::State("edit")]),
            },
            body: ToolBody::Inline(String::new()),
            trust: TrustClass::AgentObserved,
            failed: true,
            wall_ms: 1,
            preview: None,
        }
    }
}

/// The whole window the model would have been sent, after one failed `edit`.
fn window_after_a_failed_edit(detail: Option<&'static str>) -> String {
    // **A REAL scope, and that is not incidental.** With `Unavailable` every call is refused at
    // adjudication and never reaches an executor -- and the refusal path always carried its
    // reason, which is the half that was never broken. The first version of this test used it and
    // measured the wrong path entirely.
    let root = std::env::temp_dir()
        .join(format!("marlowe-why-failed-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("new.md"), "").unwrap();
    let mut e = Engine::new(
        builtin_registry().expect("the builtin manifests load"),
        WorkspaceScope::new().expect("a verified platform"),
        100_000,
        10_000,
        root.clone(),
        Tier::Act,
    );
    let mut driver = ScriptDriver::new(vec![
        ModelCall {
            usage: Usage { completion_tokens: 100, ..Usage::default() },
            step: ModelStep::one_call(
                ToolId::new("edit"),
                Args::new().text("path", "new.md").text("content", "hi").text("replacing", "x"),
            ),
        },
        ModelCall {
            usage: Usage { completion_tokens: 100, ..Usage::default() },
            step: ModelStep::Say("I could not write it.".to_string()),
        },
    ]);
    let mut summarizer = EmptySummarizer;
    let mut tools = FailsLikeAnExecutor { detail };
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
        SessionId::from_name("s"),
        CapabilityProfile::interactive(),
        Budget::interactive(),
        OutputContract::answer(),
    );
    let mut state = SessionState::new(run.session, "Marlowe.");
    state.push(Block::new(
        SourceKind::History,
        "write me a file".to_string(),
        TrustClass::UserAsserted,
    ));
    let mut prov = Provenance::new();
    e.run(&mut run, &mut state, &mut prov, &mut ports);
    // The assembled view is what the driver is handed, so this is what the model could read.
    e.assembler().assemble(&state).rendered()
}

#[test]
fn a_failed_calls_reason_is_in_the_window_the_model_reads() {
    let window = window_after_a_failed_edit(Some(WHY));
    assert!(
        window.contains(WHY),
        "the reason never reached the model. Live, the tool result was the literal string \
         \"edit · \" and the model spent six calls and three minutes not learning why:\n{window}"
    );

    // **THE CONTROL.** An outcome carrying no detail must leave the window without one, or the
    // assertion above could be satisfied by something else in the view entirely.
    let bare = window_after_a_failed_edit(None);
    assert!(
        !bare.contains(WHY),
        "the reason appeared for an outcome that carried none, so the assertion above is not \
         measuring the detail:\n{bare}"
    );

    // And the dangling separator a `failed()` outcome produces is not left on screen or in
    // context: `render()` for it is just the verb.
    assert!(
        !window.contains("edit · \n") && !window.trim_end().ends_with("edit ·"),
        "the result still ends in a bare separator with nothing after it:\n{window}"
    );
}
