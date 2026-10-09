//! **ADR-023's latch, against genuinely untrusted content, for the first time.**
//!
//! Every part of Marlowe's taint path has been unit-tested against a `Block` a test constructed
//! and labelled `UntrustedContent`. That proves the propagation arithmetic. It does not prove the
//! system has ever *met* untrusted content, because until `web` shipped there was nothing in the
//! product that could produce any.
//!
//! This drives the real `Engine`, the real `Adjudicator`, the real `FileSystemTools` host and a
//! real HTTPS fetch through `marlowe-net`, and asserts the four properties the latch exists for:
//!
//! 1. the run's floor **drops** when a page enters the window,
//! 2. the surface announces **exactly once**,
//! 3. a model-composed Target is **blocked** afterwards,
//! 4. and it **holds after the tool-results block is trimmed out of the view** — the specific
//!    failure the latch was built for, and the one that had never been exercised against anything
//!    real.
//!
//! # `#[ignore]` by default, and why that is not a way of not running it
//!
//! It reaches the network. A suite that fails when a laptop is offline, or when `example.com` is
//! slow, teaches people to ignore failures — and a test nobody trusts is worse than no test. It is
//! run explicitly, per milestone, as the one real end-to-end run CLAUDE.md requires:
//!
//! ```text
//! cargo test -p marlowe-exec --test adr023_live -- --ignored --nocapture
//! ```
//!
//! **The model is deliberately scripted.** The subject is the page, not the provider: a real model
//! choosing what to fetch would make the run non-deterministic without making the untrusted bytes
//! any more real. What is real here is the socket, the TLS, and the bytes.

use marlowe_contract::TrustClass;
use marlowe_exec::FileSystemTools;
use marlowe_loop::{
    Assembler, Budget, CallLimits, CapabilityProfile, ContextView, DegradedPath, Engine,
    InterruptPolicy, MemoryRecorder, ModelCall, ModelDriver, ModelRoute, ModelStep,
    OutputContract, Ports, ProviderError, Provenance, Run, RunId, SessionId, SessionState,
    Summarizer, TurnEvent, TurnSink,
};
use marlowe_permission::{Args, EgressPolicy, Unavailable};
use marlowe_tools::{builtin_registry, ExposedSet, ToolId};

/// A public page that is stable, tiny, and exists precisely to be fetched by machines.
const PAGE: &str = "https://example.com/";
const HOST: &str = "example.com";

// ── the minimum doubles, written here rather than shared ──────────────────────────────────
//
// `marlowe-loop`'s `tests/common` cannot be reached from this crate, and making it reachable
// would mean a dev-dependency cycle for four small structs.

struct ScriptDriver(Vec<ModelStep>);

impl ModelDriver for ScriptDriver {
    fn call(
        &mut self,
        _v: &ContextView,
        _t: &ExposedSet,
        _l: CallLimits,
    ) -> Result<ModelCall, ProviderError> {
        let step = if self.0.is_empty() {
            ModelStep::Say("done".into())
        } else {
            self.0.remove(0)
        };
        Ok(ModelCall { step, usage: marlowe_loop::Usage { completion_tokens: 10, ..Default::default() } })
    }
}

struct NoSummary;
impl Summarizer for NoSummary {
    fn summarize(&mut self, _v: &ContextView) -> String {
        String::new()
    }
}

#[derive(Default)]
struct Sink(Vec<TurnEvent>);
impl TurnSink for Sink {
    fn emit(&mut self, e: TurnEvent) {
        self.0.push(e);
    }
}

struct Yes;
impl marlowe_loop::ApprovalGate for Yes {
    fn await_approval(&mut self, _r: &marlowe_permission::BlastRadius) -> bool {
        true
    }
}

struct Frozen(i64);
impl marlowe_loop::ClockSource for Frozen {
    fn now_ms(&mut self) -> i64 {
        self.0
    }
}

#[test]
#[ignore = "reaches the network; run explicitly as the milestone's one real end-to-end run"]
fn a_real_fetched_page_latches_the_floor_and_it_holds_after_the_page_is_trimmed_away() {
    // **The window is tuned so that ONE page fits the `ToolResults` budget and two do not.**
    //
    // This is load-bearing and the first version of this test got it wrong: at 4,096 the budget
    // was 716 tokens, both ~186-token pages fitted, the trim never happened, and the fourth
    // assertion below passed **without the property it names ever occurring**. It reported success
    // for the one thing that had never been exercised. `pages_in_view` at the bottom is the
    // control that now makes that impossible.
    //
    // budget = 20% of (window - reserve). 1,792 - 512 = 1,280; 20% = 256 tokens. One page fits,
    // two cannot, and `trim_to_budget` walks newest-first so it is the FIRST page that is evicted.
    let window = 1_792;
    let mut engine = Engine::new(
        builtin_registry().expect("the builtins load"),
        Unavailable,
        window,
        512,
        std::path::PathBuf::from("/ws"),
        marlowe_permission::Tier::Act,
    );

    // **ONE fetch, then trusted results until the page is evicted from the view.**
    //
    // A second fetch cannot work: `trim_to_budget` walks newest-first, so the newest page always
    // keeps its budget and the view floor stays at `UntrustedContent` however much is trimmed.
    // Masking cannot work either — `clear_tool_results` rewrites the text and **keeps the trust
    // class**, so a masked page still floors the view. The only way the view's floor can rise is
    // for every untrusted block to fall out of it, which needs enough *trusted* results after it.
    //
    // Each refused `bash` is a `ToolResults` block at `AgentObserved`, which is exactly that.
    let mut script = vec![ModelStep::one_call(ToolId::new("web"), Args::new().text("url", PAGE))];
    for i in 0..7 {
        script.push(ModelStep::one_call(
            ToolId::new("bash"),
            Args::new().text("command", format!("echo composed-{i}")),
        ));
    }
    script.push(ModelStep::Say("stopped".into()));
    let mut driver = ScriptDriver(script);
    let mut summarizer = NoSummary;
    let mut tools = FileSystemTools::new(Unavailable, "/ws");
    let mut approvals = Yes;
    let mut sink = Sink::default();
    let mut control = marlowe_loop::NoControl;
    let mut clock = Frozen(1_700_000_000_000);
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
        RunId::from_name("live"),
        SessionId::from_name("live"),
        CapabilityProfile::new(
            ExposedSet::new(vec![ToolId::new("web"), ToolId::new("bash")]).unwrap(),
            // One host. **Not `AllowAnyHost`** — the point is that egress is a declared grant, and
            // a test that granted everything would not be exercising the shipped shape.
            EgressPolicy::allow(&[HOST]),
            InterruptPolicy::Interruptible,
            ModelRoute::Orchestrator,
            // A root run standing in for Marlowe. Level 1 is the only level that may hold a
            // full working set.
            marlowe_loop::AgentLevel::Secretary,
            false,
            false,
        )
        .unwrap(),
        Budget::interactive(),
        OutputContract::answer(),
    );
    let mut state = SessionState::new(run.session, "Marlowe.");
    let mut prov = Provenance::new();
    let _ = engine.run(&mut run, &mut state, &mut prov, &mut ports);

    let rendered = Assembler::new(window, 512).assemble(&state).rendered();
    println!("\n─── rendered view ───\n{rendered}\n─────────────────────\n");

    // **Guard against a vacuous pass.** If the fetch failed — offline, DNS, TLS — every assertion
    // below would still hold for entirely the wrong reason: no page, no untrusted content, no
    // latch, and `bash` blocked by something else. Assert the page actually arrived first.
    //
    // **It checks the SESSION STATE, not the rendered view.** The view is where the page is
    // supposed to have been evicted from by the end of the run, so a guard reading `rendered`
    // fails on a healthy run and succeeds only when the fourth property did *not* happen — the
    // guard and the property would be in direct opposition. The state is the durable record.
    // It reads the emitted tool LINE, because `clear_tool_results` masks an older result's text
    // in the state itself — so by the end of this run neither the view nor the state still
    // contains the page's bytes. The §B6 line is the record that survives both.
    let fetched = sink
        .0
        .iter()
        .filter(|e| {
            matches!(e, TurnEvent::ToolLine { verb, state: marlowe_loop::ToolLineState::Ok(_), .. }
                if verb == "web")
        })
        .count();
    assert!(
        fetched >= 1,
        "the fetch did not succeed, so nothing below would be testing the latch — every \
         assertion would hold for the wrong reason. Is the network up? events: {:?}",
        sink.0.len()
    );

    // ── 1. the floor dropped, and it dropped to the class that blocks ────────────────────
    assert_eq!(
        run.trust_floor(),
        TrustClass::UntrustedContent,
        "a real fetched page must move the run's floor to UntrustedContent"
    );
    assert!(marlowe_permission::blocks_composed_targets(run.trust_floor()));

    // ── 2. announced exactly once, across four iterations ───────────────────────────────
    let announced = sink
        .0
        .iter()
        .filter(|e| matches!(e, TurnEvent::Degraded { what: DegradedPath::TrustFloorLatched }))
        .count();
    assert_eq!(
        announced, 1,
        "the latch is monotonic, so it announces on the transition and never again"
    );

    // ── 3. the composed Target was blocked ──────────────────────────────────────────────
    assert!(
        rendered.contains("[bash blocked]"),
        "a shell command composed after reading a page must be refused:\n{rendered}"
    );
    assert!(
        !rendered.contains("composed-after-the-page\n0"),
        "and it must not have executed"
    );

    // ── 4. THE ONE THAT HAD NEVER BEEN EXERCISED ────────────────────────────────────────
    //
    // The first page is now out of the view: `ToolResults` is trimmable, the budget is a fraction
    // of a 4 KB window, and two pages do not fit. `ContextView::trust_floor()` therefore reads
    // BETTER than it did — and the run's latched floor must not follow it up.
    //
    // This is the hole the latch was built to close: before it, `taint_for` used the view's floor
    // alone, so the assembler dropping a block to stay inside budget silently handed the run back
    // the privileges ADR-023 says it loses permanently.
    let view = Assembler::new(window, 512).assemble(&state);
    let pages_in_state = state
        .volatile
        .iter()
        .filter(|b| b.text.contains("Example Domain"))
        .count();
    let pages_in_view = view.blocks().filter(|b| b.text.contains("Example Domain")).count();
    println!(
        "view floor: {:?}   run floor: {:?}   pages in state: {pages_in_state}   in view: {pages_in_view}",
        view.trust_floor(),
        run.trust_floor()
    );

    // **THE CONTROL, and the reason this test is worth anything.**
    //
    // Everything below holds trivially if nothing was ever trimmed — which is exactly what
    // happened on the first run of this test, at a larger window, while it reported success. A
    // pass here must mean the assembler actually dropped an untrusted block from the view.
    assert_eq!(
        pages_in_view, 0,
        "the page is STILL IN THE VIEW, so the fourth property was not exercised and this test \
         proved nothing about it. The view's floor can only rise once every untrusted block has \
         left it. state={pages_in_state} view={pages_in_view} — add trusted results or shrink \
         the window."
    );
    assert!(
        view.trust_floor() > TrustClass::UntrustedContent,
        "the view's floor did not actually rise, so `min(view, latched)` is not being tested — \
         the latch would be redundant here and a pass would mean nothing. view floor: {:?}",
        view.trust_floor()
    );

    assert!(
        run.trust_floor() <= view.trust_floor(),
        "the run's latched floor may never be better than what it has seen"
    );
    assert_eq!(
        run.trust_floor(),
        TrustClass::UntrustedContent,
        "the floor must still be latched after trimming"
    );
    // **Not one shell command ran**, including the ones issued after the page left the view.
    // Counted from the emitted lines rather than the rendered view, which is trimmed.
    let (ran, refused): (usize, usize) = sink.0.iter().fold((0, 0), |(ran, refused), e| match e {
        TurnEvent::ToolLine { verb, state, .. } if verb == "bash" => match state {
            marlowe_loop::ToolLineState::Ok(_) => (ran + 1, refused),
            marlowe_loop::ToolLineState::Failed(s) if s.render().contains("blocked") => {
                (ran, refused + 1)
            }
            _ => (ran, refused),
        },
        _ => (ran, refused),
    });
    assert_eq!(ran, 0, "a composed shell command executed after the run read a page");
    assert_eq!(
        refused, 7,
        "every composed command must be refused — including those issued after the page was \
         trimmed out of the view, which is the case the latch exists for"
    );
}
