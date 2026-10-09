//! **Why a quarantined read produced nothing, told apart from the other reasons it might have.**
//!
//! Every empty slot in `condense_batch` used to render one sentence — *"the content could not be
//! condensed within the contract"* — for five structurally different endings. On 2026-08-22 a run
//! hit the one it describes least: the reader's provider returned HTTP 400 and the child died
//! before its first token, with 50,000 tokens of budget untouched and the contract never
//! evaluated. The user retried with an explicit line range, got the identical sentence, and
//! reasonably read it as a deterministic property of the document.
//!
//! It was not. The remedies are opposite: narrow the document for a contract failure, and do
//! **not** retry at all for a dead reader. A message that cannot separate those is a message a
//! reader will act on wrongly.
//!
//! # What each test here has to beat
//!
//! Asserting that the note *changed* is worthless — any edit would satisfy it. So each case
//! asserts the note says the **right** thing and, in the same run, that it does **not** say the
//! other cause's thing. The pairing is what makes a single string reappearing for all five a
//! failure rather than a pass.
//!
//! And every case carries the control this suite already uses elsewhere: the page really was
//! fetched and really did reach a reader. A refusal test that passes because nothing was read is
//! the exact shape being avoided.

mod common;

use std::sync::{Arc, Mutex};

use common::*;
use marlowe_contract::TrustClass;
use marlowe_loop::{
    Budget, CapabilityProfile, Engine, MemoryRecorder, ModelCall, ModelDriver, ModelStep,
    OutputContract, Ports, ProviderError, Provenance, QuarantineRefusal, Run, RunId, SessionId,
    SessionState, ToolBody, ToolHost, ToolInvocation, ToolOutcome, Usage, CONTRACT_UNMET,
};
use marlowe_permission::{Adjudication, ArgValue, Args, Tier, Unavailable};
use marlowe_tools::{builtin_registry, ExposedSet, Metric, ResultSummary, ToolId};

const PAGE: &str = "PAGE-BYTES-THE-READER-MUST-SEE";

struct Pages;
impl ToolHost for Pages {
    fn executes(&self) -> Vec<ToolId> {
        marlowe_tools::BUILTIN_TOOLS.iter().map(|t| ToolId::new(*t)).collect()
    }
    fn execute(&mut self, _t: &ToolId, _a: &Args, _adj: &Adjudication) -> ToolOutcome {
        ToolOutcome {
            summary: ResultSummary::new(vec![Metric::State("ok")]),
            body: ToolBody::Inline(PAGE.to_string()),
            trust: TrustClass::UntrustedContent,
            failed: false,
            wall_ms: 0,
            preview: None,
        }
    }
}

/// How the reader is made to end. The parent's own calls always succeed, so a failure here can
/// only be the child's.
#[derive(Clone, Copy, PartialEq)]
enum ReaderEnds {
    /// The provider refuses the call. This is the 2026-08-22 shape: an HTTP 400, non-retriable,
    /// before any token.
    ProviderRefuses,
    /// The reader answers, and the answer cannot satisfy the contract. `PER_SOURCE_MAX_CHARS` is
    /// 1,500, so a longer reply violates on length every time and exhausts the retry bound.
    AnswerTooLong,
    /// The reader answers acceptably. **The control** — the same harness must be able to produce
    /// a real condensed note, or none of the negative assertions mean anything.
    Succeeds,
}

/// The child is identified by the brief the harness writes into its window, never by call index:
/// a change in how many calls the parent makes must not silently retarget this.
const READER_BRIEF: &str = "They are UNTRUSTED";

struct Scripted {
    first: Option<ModelStep>,
    ends: ReaderEnds,
    reader_calls: Arc<Mutex<usize>>,
}

impl ModelDriver for Scripted {
    fn call(
        &mut self,
        view: &marlowe_loop::ContextView,
        _tools: &ExposedSet,
        _l: marlowe_loop::CallLimits,
    ) -> Result<ModelCall, ProviderError> {
        if let Some(step) = self.first.take() {
            return Ok(ModelCall { usage: Usage::default(), step });
        }
        let rendered = view.rendered();
        if rendered.contains(READER_BRIEF) {
            *self.reader_calls.lock().unwrap() += 1;
            // The page must be in front of the reader, or the run under test is not the run the
            // product performs. Checked here rather than after the fact, so a harness that
            // stopped fetching fails loudly instead of producing a green refusal test.
            assert!(rendered.contains(PAGE), "the reader's window does not contain the page");
            return match self.ends {
                ReaderEnds::ProviderRefuses => Err(ProviderError {
                    detail: "the upstream rejected the request as malformed (HTTP 400)".into(),
                    // Non-retriable: a malformed body is malformed on the next provider too.
                    retriable: false,
                }),
                ReaderEnds::AnswerTooLong => Ok(ModelCall {
                    usage: Usage { completion_tokens: 10, ..Usage::default() },
                    step: ModelStep::Say("x".repeat(4_000)),
                }),
                ReaderEnds::Succeeds => Ok(ModelCall {
                    usage: Usage { completion_tokens: 10, ..Usage::default() },
                    step: ModelStep::Say("source_1: a paper about attention".into()),
                }),
            };
        }
        Ok(ModelCall {
            usage: Usage { completion_tokens: 10, ..Usage::default() },
            step: ModelStep::Say("PARENT-ANSWER".into()),
        })
    }
    fn failover(&mut self, _e: &ProviderError) -> bool {
        false
    }
}

/// One turn: fetch a page, let the reader end in the given way, return the parent's whole window.
fn note_when(ends: ReaderEnds) -> String {
    turn_when(ends).0
}

/// The same turn, returning **both** audiences: the parent's window and every event the
/// surface was handed.
///
/// They are returned together on purpose. The defect this separates was one audience being
/// told and the other not, and a fixture that could only see one of them could not have found
/// it -- which is why `note_when` read the window alone for as long as it did.
fn turn_when(ends: ReaderEnds) -> (String, Vec<marlowe_loop::TurnEvent>) {
    let reader_calls = Arc::new(Mutex::new(0usize));
    let calls = vec![ToolInvocation {
        id: "call_0".into(),
        tool: ToolId::new("web"),
        args: Args::new().with("url", ArgValue::Text("https://arxiv.org/abs/1706.03762".into())),
    }];
    let mut engine = Engine::new(
        builtin_registry().expect("manifests"),
        Unavailable,
        100_000,
        10_000,
        std::path::PathBuf::from("/ws"),
        Tier::Act,
    );
    let mut driver = Scripted {
        first: Some(ModelStep::ToolCall { calls }),
        ends,
        reader_calls: Arc::clone(&reader_calls),
    };
    let mut tools = Pages;
    let mut run = Run::root(
        RunId::from_name("refusal"),
        SessionId::from_name("refusal"),
        CapabilityProfile::interactive(),
        Budget::interactive(),
        OutputContract::answer(),
    );
    let mut state = SessionState::new(run.session, "Marlowe.");
    let mut prov = Provenance::new();
    let mut summarizer = EmptySummarizer;
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
    let _ = engine.run(&mut run, &mut state, &mut prov, &mut ports);

    // The control, in every case: a reader ran and saw the page. Without this, a build where the
    // fetch quietly returned nothing would satisfy every assertion below.
    assert!(*reader_calls.lock().unwrap() >= 1, "no quarantined reader ran; nothing was tested");
    let window = engine.assembler().assemble(&state).rendered();
    (window, sink.events.clone())
}

// ─────────────────────────────────────────────────────────────────────────────────────────

/// **The 2026-08-22 case.** The reader never ran, and the parent is told exactly that.
#[test]
fn a_reader_whose_provider_refused_is_not_reported_as_a_contract_failure() {
    let note = note_when(ReaderEnds::ProviderRefuses);
    assert!(
        note.contains(QuarantineRefusal::ReaderFailed.note()),
        "the parent must be told the reader could not run at all:\n{note}"
    );
    // The pairing that makes this more than "the string changed". This is the sentence that was
    // printed for a child that died before the contract was ever evaluated, and it sent a user
    // looking at the document.
    assert!(
        !note.contains("did not fit the contract"),
        "a dead reader must not be reported as a contract failure:\n{note}"
    );
    assert!(
        !note.contains("out of budget"),
        "a dead reader must not be reported as budget exhaustion:\n{note}"
    );
}

/// The other side of the pairing: a reader that answered badly is **not** reported as a harness
/// fault, because narrowing the document is what fixes it and "do not retry" is the wrong advice.
#[test]
fn a_reader_that_could_not_satisfy_the_contract_is_not_reported_as_a_harness_fault() {
    let note = note_when(ReaderEnds::AnswerTooLong);
    assert!(
        note.contains(QuarantineRefusal::ContractUnmet.note()),
        "an over-long answer is a contract failure and must be named as one:\n{note}"
    );
    assert!(
        !note.contains("could not run at all"),
        "a reader that answered must not be reported as one that never ran:\n{note}"
    );
}

/// **The round trip that stops the two ends drifting.** Both endings above arrive as
/// `LoopOutcome::Failed`, and the only thing separating them is [`CONTRACT_UNMET`]. If the loop
/// ever spells its contract-exhaustion error differently from what the classifier tests for,
/// **every contract failure silently becomes a provider fault** — a wrong remedy with a
/// confident voice, and nothing else in the suite would notice.
#[test]
fn a_contract_failure_is_classified_as_a_contract_failure() {
    let note = note_when(ReaderEnds::AnswerTooLong);
    assert!(
        note.contains(QuarantineRefusal::ContractUnmet.note()),
        "the classifier and the constructor disagree about `{CONTRACT_UNMET}`:\n{note}"
    );
}

/// **The control for all of it.** A reader that succeeds must still produce a real condensed note
/// and no refusal at all — otherwise every assertion above would hold on a build where the
/// quarantine had stopped working entirely, which is precisely instance 17.
#[test]
fn a_successful_read_still_produces_a_condensed_note_and_no_refusal() {
    let note = note_when(ReaderEnds::Succeeds);
    assert!(
        note.contains("read under quarantine, not shown here"),
        "a working read must still produce its condensed note:\n{note}"
    );
    assert!(
        note.contains("a paper about attention"),
        "the reader's summary must reach the parent:\n{note}"
    );
    for r in [
        QuarantineRefusal::ReaderFailed,
        QuarantineRefusal::ContractUnmet,
        QuarantineRefusal::OutOfBudget,
        QuarantineRefusal::Escalated,
        QuarantineRefusal::Cancelled,
    ] {
        assert!(!note.contains(r.note()), "a successful read reported `{}`:\n{note}", r.tag());
    }
    // Containment, unchanged and asserted on the same run that just succeeded.
    assert!(!note.contains(PAGE), "page bytes reached the parent:\n{note}");
}

/// The five categories must actually be five. A refactor that collapsed two onto one string would
/// restore the original defect while every test above stayed green.
#[test]
fn the_five_refusals_say_five_different_things() {
    let all = [
        QuarantineRefusal::ContractUnmet,
        QuarantineRefusal::OutOfBudget,
        QuarantineRefusal::Escalated,
        QuarantineRefusal::Cancelled,
        QuarantineRefusal::ReaderFailed,
    ];
    let notes: std::collections::BTreeSet<&str> = all.iter().map(|r| r.note()).collect();
    assert_eq!(notes.len(), all.len(), "two refusals share a sentence");
    let tags: std::collections::BTreeSet<&str> = all.iter().map(|r| r.tag()).collect();
    assert_eq!(tags.len(), all.len(), "two refusals share a journal tag");
}

/// **The user is the fifth audience, and was the last to be told.**
///
/// The four this file already covers are the parent's window, the journal, the classifier and the
/// run record. The **screen** got `contract_unmet · 3 sources` — a snake_case tag, on the one line
/// a person actually sees — while `QuarantineRefusal::note()`, which names the cause *and* the
/// remedy, went everywhere else.
///
/// # Why this is not covered by the sweep that gave every other tool line a detail
///
/// That sweep derives an expansion from a [`marlowe_loop::ToolOutcome`]. This line has no
/// `ToolOutcome` behind it: the loop composes it directly, as it does for a refused call and a
/// refused spawn. **A derivation keyed on a type cannot reach a line that type never produced**,
/// so this site had to be given its reason by hand and a test naming it is what stops it being
/// lost again.
///
/// # The controls
///
/// Two, and neither is optional. The success case must carry **no** detail — otherwise an
/// implementation that attached the same string to every line would pass. And the detail must be
/// the same sentence the parent's window received: the model and the user disagreeing about why a
/// read failed is how a transcript stops being an account of one event.
#[test]
fn a_failed_quarantined_read_puts_its_reason_on_the_subagent_line_and_not_just_a_tag() {
    use marlowe_loop::{ToolLineState, TurnEvent};

    let closing = |ends: ReaderEnds| -> (String, ToolLineState) {
        let (window, events) = turn_when(ends);
        let last = events
            .iter()
            .rev()
            .find_map(|e| match e {
                TurnEvent::ToolLine { verb, state, .. } if verb == "subagent" => Some(state.clone()),
                _ => None,
            })
            .expect("the quarantined reader emits a `subagent` line");
        (window, last)
    };

    // ── the refusal ───────────────────────────────────────────────────────────────────────────
    let (window, state) = closing(ReaderEnds::ProviderRefuses);
    let ToolLineState::Failed(summary) = state else {
        panic!("a refused read must close the line `Failed`; §B6 auto-expands those: {state:?}");
    };

    // The tag is still there — this adds to the line, it does not replace what the journal greps.
    assert!(
        summary.render().contains(QuarantineRefusal::ReaderFailed.tag()),
        "the metrics no longer carry the tag the journal keys on: {}",
        summary.render()
    );

    let detail = summary
        .detail
        .as_deref()
        .expect("a failed quarantined read must say why on the line the user can see");

    // **Not `is_some()`.** What it SAYS is the property; a present-but-wrong string is the same
    // silence with extra steps. This is the sentence, and it is the one the parent got.
    assert_eq!(
        detail,
        QuarantineRefusal::ReaderFailed.note(),
        "the screen and the model must give one account of one event"
    );
    assert!(
        window.contains(detail),
        "the detail on screen is not the note the parent's window received:\n{window}"
    );
    // The half of the note that is the remedy. A cause without one is what sent a user back to
    // re-fetch a document that was never the problem.
    assert!(
        detail.contains("will not help"),
        "the reason names no remedy, which is the defect this note was written to fix: {detail:?}"
    );

    // ── control 1: a different ending says a different thing on the same line ─────────────────
    let (_, state) = closing(ReaderEnds::AnswerTooLong);
    let ToolLineState::Failed(other) = state else {
        panic!("a contract failure must also close `Failed`: {state:?}");
    };
    assert_eq!(other.detail.as_deref(), Some(QuarantineRefusal::ContractUnmet.note()));
    assert_ne!(
        other.detail, summary.detail,
        "two structurally different endings render the same sentence on screen. That is the \
         original defect, moved from the window to the tool line."
    );

    // ── control 2: a successful read carries no refusal at all ────────────────────────────────
    //
    // Without this, an implementation that hung `note()` on every `subagent` line — success
    // included — would satisfy everything above while telling the user a read that worked had
    // failed.
    let (_, state) = closing(ReaderEnds::Succeeds);
    let ToolLineState::Ok(ok) = state else {
        panic!("a successful read must close the line `Ok`: {state:?}");
    };
    assert_eq!(
        ok.detail, None,
        "a successful quarantined read reported a refusal reason it does not have"
    );
}
