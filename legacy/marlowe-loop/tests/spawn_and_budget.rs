//! M2's three report items, each asserted as the property rather than as a proxy.
//!
//! 1. A spawn declaring `reads_untrusted` with a tool set fails at load time.
//! 2. A budget ceiling pauses rather than overspending.
//! 3. A child's transcript never reaches the parent's context.

mod common;

use common::*;
use marlowe_contract::TrustClass;
use marlowe_journal::EventKind;
use marlowe_loop::{
    Budget, BudgetShare, CapabilityProfile, CondensedResult, Engine, LoopOutcome, MemoryRecorder,
    ModelStep, OrphanPolicy, OutputContract, PauseReason, Ports, Provenance, Run, RunId,
    SessionId, SessionState, SpawnRequest,
};
use marlowe_permission::{Tier, Unavailable};
use marlowe_tools::{builtin_registry, ToolId};

fn engine() -> Engine<Unavailable> {
    Engine::new(
        builtin_registry().expect("the eleven builtin manifests load"),
        Unavailable,
        100_000,
        10_000,
        std::path::PathBuf::from("/ws"),
        Tier::Act,
    )
}

fn root(budget: Budget) -> Run {
    Run::root(
        RunId::from_name("root"),
        SessionId::from_name("root-session"),
        CapabilityProfile::interactive(),
        budget,
        OutputContract::answer(),
    )
}

// ─────────────────────────────────────────────────────────────────────────────────────────
// 1. A spawn that declares reads_untrusted with a tool set fails at load time
// ─────────────────────────────────────────────────────────────────────────────────────────

#[test]
fn a_spawn_that_reads_untrusted_with_tools_is_refused_and_the_child_never_starts() {
    // NOTE on the phrasing of this requirement. CONTRACTS §5 states the load-time error as
    // `reads_untrusted && !exposed_tools.is_empty()` — it is a **non-empty** tool set that is
    // refused. The empty set with `reads_untrusted` is the *valid* quarantined reader, and it
    // is asserted below so the two cases cannot be confused.
    let mut e = engine();
    let mut driver = ScriptDriver::new(vec![
        step(
            ModelStep::Spawn(SpawnRequest {
                task: "read this page and tell me what it says".into(),
                contract: OutputContract::new("findings", &["findings"]),
                orphan: OrphanPolicy::Terminate,
                share: BudgetShare::Standard,
                grant_tokens: None,
                // The violation is the **non-empty set**, not which tool is in it. It was `web`
                // until `interactive()` stopped exposing tools with no executor, at which point
                // the spawn was refused by the narrowing rule first and this test stopped
                // exercising the trifecta rule it is named for. Any tool the parent actually
                // holds keeps the assertion pointed at the right refusal.
                tools: vec![ToolId::new("read")],
                reads_untrusted: true,
                tools_declared: true,
                role: marlowe_loop::ModelRoute::Worker,
                disposition: marlowe_loop::Disposition::Work,
            }),
            100,
        ),
        // **Two replies, one each.** A spawn re-enters the same loop with the same scripted
        // driver, so the CHILD consumes the first reply and completes on it; the parent then
        // needs one of its own. Before M2 C2e a reply ended nothing and one sufficed.
        say("child done", 100),
        say("parent done", 100),
    ]);
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

    let mut run = root(Budget::interactive());
    let mut state = SessionState::new(run.session, "Marlowe.");
    let mut prov = Provenance::new();
    let outcome = e.run(&mut run, &mut state, &mut prov, &mut ports);

    assert!(matches!(outcome, LoopOutcome::Completed(_)), "the parent survives the refusal");

    // The child never started: no RunSpawned event exists.
    assert_eq!(
        recorder.count(EventKind::RunSpawned),
        0,
        "an invalid capability profile must be refused before a child run is created"
    );
    // ...and the parent was told why, in words naming the rule.
    let rendered = e.assembler().assemble(&state).rendered();
    assert!(
        rendered.contains("trifecta"),
        "the refusal must reach the model as a reason it can act on: {rendered}"
    );
    // The requested tool set was NOT silently dropped in favour of a valid quarantined reader.
    assert!(
        !rendered.contains("[child returned"),
        "a refused spawn must not quietly run as something else"
    );
}

#[test]
fn the_empty_tool_set_with_reads_untrusted_is_the_valid_quarantined_reader() {
    // The other side of the same rule, asserted so the refusal above cannot be read as
    // "reads_untrusted is never allowed" — the quarantined reader is the whole reason a
    // subagent may safely read untrusted content at all (§8.2).
    let q = CapabilityProfile::quarantined_reader();
    assert!(q.reads_untrusted());
    assert!(q.exposed_tools().is_empty());
    assert!(!q.may_write_memory());
}

#[test]
fn a_quarantined_child_runs_and_returns_findings() {
    let mut e = engine();
    let mut driver = ScriptDriver::new(vec![
        step(
            ModelStep::Spawn(SpawnRequest {
                task: "summarise the fetched page".into(),
                contract: OutputContract::new("findings", &["findings"]),
                orphan: OrphanPolicy::Terminate,
                share: BudgetShare::Small,
                grant_tokens: None,
                tools: vec![], // empty: the quarantined shape
                reads_untrusted: true,
                tools_declared: true,
                role: marlowe_loop::ModelRoute::Worker,
                disposition: marlowe_loop::Disposition::Work,
            }),
            100,
        ),
        // the child
        step(
            ModelStep::Say("the page recommends x".into()),
            100,
        ),
        // the parent finishes
        say("x", 100),
    ]);
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

    let mut run = root(Budget::interactive());
    let mut state = SessionState::new(run.session, "Marlowe.");
    let mut prov = Provenance::new();
    let outcome = e.run(&mut run, &mut state, &mut prov, &mut ports);

    assert!(matches!(outcome, LoopOutcome::Completed(_)));
    assert_eq!(recorder.count(EventKind::RunSpawned), 1);

    // The orphan policy is recorded at spawn even though M2 cannot orphan anything. That
    // record is what lets M3 be an extension rather than a migration.
    let spawned = recorder.payloads(EventKind::RunSpawned);
    assert_eq!(spawned[0]["orphan_policy"], serde_json::json!("terminate"));
    assert_eq!(spawned[0]["reads_untrusted"], serde_json::json!(true));

    assert!(e.assembler().assemble(&state).rendered().contains("the page recommends x"));
}

// ─────────────────────────────────────────────────────────────────────────────────────────
// 2. A budget ceiling pauses rather than overspending
// ─────────────────────────────────────────────────────────────────────────────────────────

#[test]
fn a_budget_ceiling_pauses_and_never_spends_past_the_line() {
    let mut e = engine();
    // Twenty steps offered; the budget allows far fewer.
    // **Tool calls, not replies.** M2 C2e made a reply-with-no-tool-call the end of a turn,
    // so a script of twenty `say`s now ends at the first one and the budget never bites. A long
    // turn in the real product is a run of tool calls, which is what this scripts.
    let script: Vec<_> = (0..20).map(|_| work(1_000)).collect();
    let mut driver = ScriptDriver::new(script);
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

    let budget = Budget { tokens: 3_000, ..Budget::interactive() };
    let mut run = root(budget);
    let mut state = SessionState::new(run.session, "Marlowe.");
    let mut prov = Provenance::new();
    let outcome = e.run(&mut run, &mut state, &mut prov, &mut ports);

    // It PAUSED. It did not fail, and it did not run to the end of the script.
    assert_eq!(
        outcome,
        LoopOutcome::Paused {
            reason: PauseReason::BudgetExhausted { dimension: "tokens".into() }
        }
    );
    assert_eq!(recorder.count(EventKind::RunPaused), 1, "a pause is never silent");

    // It did not spend past the line.
    assert!(
        run.spent.tokens <= budget.tokens,
        "spent {} against a ceiling of {}",
        run.spent.tokens,
        budget.tokens
    );

    // THE non-proxy assertion: the cap handed to the provider tracked what was left. A
    // constant here would also produce a pause, and the pause alone proves nothing about
    // whether the line could have been crossed.
    assert_eq!(
        driver.limits_seen,
        vec![3_000, 2_000, 1_000],
        "each call is capped at the remaining budget, and the fourth is never issued"
    );
    assert!(driver.steps.len() >= 17, "the script was not exhausted; the budget stopped it");
}

#[test]
fn every_budget_dimension_can_be_the_one_that_pauses() {
    // A budget test that only ever trips on tokens is a budget test for one dimension. Each
    // case below sets one ceiling to its minimum and leaves the others generous.
    for (dimension, budget) in [
        ("tokens", Budget { tokens: 600, ..Budget::interactive() }),
        ("tool_calls", Budget { tool_calls: 0, ..Budget::interactive() }),
        ("micros_usd", Budget { micros_usd: 0, ..Budget::interactive() }),
        ("wall_ms", Budget { wall_ms: 0, ..Budget::interactive() }),
        ("subagents", Budget { subagents: 0, ..Budget::interactive() }),
    ] {
        let mut e = engine();
        let script: Vec<_> = (0..5).map(|_| work(100)).collect();
        let mut driver = ScriptDriver::new(script);
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
        let mut run = root(budget);
        let mut state = SessionState::new(run.session, "Marlowe.");
        let mut prov = Provenance::new();
        let outcome = e.run(&mut run, &mut state, &mut prov, &mut ports);
        assert_eq!(
            outcome,
            LoopOutcome::Paused {
                reason: PauseReason::BudgetExhausted { dimension: dimension.into() }
            },
            "a zero {dimension} ceiling must pause on {dimension}"
        );
    }
}

#[test]
fn a_driver_that_ignores_its_cap_is_stopped_at_the_next_iteration() {
    // The honest limit of the mechanism, asserted rather than left implied. The harness hands
    // the provider a hard cap; a provider that returns more than it was allowed overshoots by
    // at most one call, and the top-of-loop check stops it there. Claiming "never past the
    // line" without this test would be claiming something about a provider we do not control.
    let mut e = engine();
    let mut driver = ScriptDriver::new(vec![
        // Tool calls, not replies: a reply ends the turn (M2 C2e), and this test needs a
        // SECOND iteration to exist so the top-of-loop check can stop it there.
        work(50_000), // ignores a 3,000 cap
        work(50_000),
    ]);
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
    let mut run = root(Budget { tokens: 3_000, ..Budget::interactive() });
    let mut state = SessionState::new(run.session, "Marlowe.");
    let mut prov = Provenance::new();
    let outcome = e.run(&mut run, &mut state, &mut prov, &mut ports);

    assert!(matches!(outcome, LoopOutcome::Paused { .. }));
    assert_eq!(driver.limits_seen, vec![3_000], "exactly one call was issued");
    assert_eq!(driver.steps.len(), 1, "the second scripted step was never reached");
    assert!(run.spent.tokens > 3_000, "the overshoot is real and is bounded to one call");
}

// ─────────────────────────────────────────────────────────────────────────────────────────
// 3. A child's transcript never reaches the parent's context
// ─────────────────────────────────────────────────────────────────────────────────────────

const CHILD_MARKER: &str = "CHILD-TRANSCRIPT-MARKER-9f2a";

#[test]
fn a_childs_transcript_never_reaches_the_parents_context() {
    let mut e = engine();
    let mut driver = ScriptDriver::new(vec![
        // the parent spawns
        step(
            ModelStep::Spawn(SpawnRequest {
                task: "investigate the thing".into(),
                contract: OutputContract::new("what you found", &["findings"]),
                orphan: OrphanPolicy::Detach,
                share: BudgetShare::Standard,
                grant_tokens: None,
                // The child needs a tool in order to HAVE working to isolate.
                tools: vec![ToolId::new("read")],
                reads_untrusted: false,
                tools_declared: true,
                role: marlowe_loop::ModelRoute::Worker,
                disposition: marlowe_loop::Disposition::Work,
            }),
            100,
        ),
        // The child works, then replies — which is what ends it (M2 C2e). It cannot "talk at
        // length" across several replies any more, because the first reply completes it.
        work(100),
        work(100),
        // **The marker is on the child's WORKING (its tool results), not on its answer.**
        //
        // M2 C2e made the reply the result, so a child's final reply legitimately crosses to
        // the parent — that is what a subagent is FOR. What must never cross is how it got
        // there. Marking the answer would test the opposite of the invariant.
        say("the answer is 42", 100),
        // the parent finishes
        say("42", 100),
    ]);
    let mut summarizer = EmptySummarizer;
    // The child's tool results carry the marker: that is its WORKING, which must not cross.
    let mut tools = ScriptedTools {
        body: Some(format!("{CHILD_MARKER} intermediate finding")),
        ..Default::default()
    };
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

    let mut run = root(Budget::interactive());
    let mut state = SessionState::new(run.session, "Marlowe.");
    let mut prov = Provenance::new();
    let outcome = e.run(&mut run, &mut state, &mut prov, &mut ports);
    assert!(matches!(outcome, LoopOutcome::Completed(_)));

    // The anti-vacuity check FIRST. Without it, an absent marker in the parent would prove
    // nothing — it would also be absent if the child had never spoken.
    // **The anti-vacuity check, and it took three attempts to state correctly.**
    //
    // Not `sink.text()`: tool bodies never reach the sink, which carries §B6 tool *lines* — a
    // verb and a typed summary. Not `tools.calls`: this engine's scope is `Unavailable`, so
    // adjudication refuses the call before the tool host ever sees it.
    //
    // What is true regardless is that the child made several model calls. A child that ran one
    // step and stopped would leave an absent marker in the parent proving nothing.
    assert!(
        driver.views_seen.len() >= 4,
        "the child must actually have worked across several steps for this test to mean \
         anything; the driver saw {} calls",
        driver.views_seen.len()
    );

    // The property: none of it is in the parent's context, in any tier.
    let view = e.assembler().assemble(&state);
    assert!(
        !view.rendered().contains(CHILD_MARKER),
        "the parent's assembled view carries the child's transcript:\n{}",
        view.rendered()
    );
    for block in view.blocks() {
        assert!(!block.text.contains(CHILD_MARKER), "block {:?} carries it", block.source);
    }
    // ...and not in the raw session state either, which is what the view is built from.
    assert!(!state.volatile.iter().any(|b| b.text.contains(CHILD_MARKER)));

    // What did cross is the contract's field, and only that.
    //
    // **The spelling changed with `CondensedResult::render` and the property did not.** The old
    // form was `findings: the answer is 42` on one line; a value containing a newline could
    // therefore forge a second field header, since the parent only ever sees the flattened
    // string. Headers now sit at column 0 and every line a value contributes is indented, so
    // this reads across two lines.
    let rendered = view.rendered();
    assert!(
        rendered.contains("findings:") && rendered.contains("  the answer is 42"),
        "the contract's field crossed, in the indented form render now produces: {rendered}"
    );

    // The parent's own driver only ever saw views without the marker. This is the assertion
    // that would catch a leak arriving through some path other than `SessionState`.
    assert!(
        driver.views_seen.iter().all(|v| !v.contains(CHILD_MARKER)
            || v.contains("investigate the thing")),
        "only the child's own views may contain the child's transcript"
    );
}

#[test]
fn a_child_that_returns_a_transcript_shaped_result_is_refused_by_its_contract() {
    // The second half: the child cannot smuggle its history through the return value either.
    let contract = OutputContract::new("what you found", &["findings"]);
    let smuggled = CondensedResult::new()
        .with("findings", "ok")
        .with("transcript", "...everything I read...");
    assert!(contract.validate(&smuggled).is_err());

    let fat = CondensedResult::new().with("findings", "x".repeat(marlowe_loop::DEFAULT_RESULT_MAX_CHARS + 1));
    assert!(contract.validate(&fat).is_err());
}

#[test]
fn a_child_does_not_inherit_its_parents_provenance_attributions() {
    // A string the user typed to the parent is not user-asserted inside a child that never saw
    // the user say it. Without this, a spawn would be a laundering step for provenance.
    let mut e = engine();
    let mut driver = ScriptDriver::new(vec![
        step(
            ModelStep::Spawn(SpawnRequest {
                task: "look into it".into(),
                contract: OutputContract::new("f", &["findings"]),
                orphan: OrphanPolicy::Terminate,
                share: BudgetShare::Small,
                grant_tokens: None,
                tools: vec![ToolId::new("remember")],
                reads_untrusted: false,
                tools_declared: true,
                role: marlowe_loop::ModelRoute::Worker,
                disposition: marlowe_loop::Disposition::Work,
            }),
            100,
        ),
        // The child names a memory id the *user* mentioned to the parent. In the child that is
        // a model-composed target, and the child's window floor is what decides it.
        step(
            ModelStep::one_call(ToolId::new("remember"), marlowe_permission::Args::new()
                    .text("text", "a claim")
                    .text("derived_from", "m-secret")
                    .text("payload_kind", "fact")),
            100,
        ),
        say("done", 100),
        say("ok", 100),
    ]);
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

    let mut run = root(Budget::interactive());
    let mut state = SessionState::new(run.session, "Marlowe.");
    let mut prov = Provenance::new();
    // The user said it — to the parent.
    prov.attribute_user_message("please remember m-secret");

    let _ = e.run(&mut run, &mut state, &mut prov, &mut ports);

    // The child's call went through on the child's own floor — which is `agent_inferred`,
    // because the child's window holds its task brief, and that brief is text the *parent's
    // model* composed. It is allowed, and it is allowed for a reason that has nothing to do
    // with the user having vouched for `m-secret` in a conversation the child never saw.
    let decisions = recorder.payloads(EventKind::PermissionDecided);
    let d = decisions.last().expect("the child adjudicated its remember call");
    assert_eq!(
        d["taint"]["derived_from"],
        serde_json::json!("agent_inferred"),
        "the child's window floor decided this, not the parent's attribution: {d}"
    );
    assert_ne!(
        d["taint"]["derived_from"],
        serde_json::json!("user_asserted"),
        "a spawn must not launder the parent's attributions into the child"
    );
}

#[test]
fn a_child_cannot_be_given_a_tool_its_parent_does_not_have() {
    // **`run` is in the set on purpose.** `Engine::spawn`'s first refusal is now the create
    // grant — M3-DESIGN §1's *"a worker creates nothing"* — so a parent that does not hold `run`
    // is refused for that reason and never reaches the narrowing check this test is about. The
    // subject is what a parent may GIVE AWAY, not whether it may spawn at all.
    let narrow = CapabilityProfile::new(
        marlowe_tools::ExposedSet::new(vec![
            ToolId::new("read"),
            ToolId::new("done"),
            ToolId::new("run"),
        ])
        .unwrap(),
        marlowe_permission::EgressPolicy::DenyAll,
        marlowe_loop::InterruptPolicy::Interruptible,
        marlowe_loop::ModelRoute::Orchestrator,
        marlowe_loop::AgentLevel::Secretary,
        false,
        false,
    )
    .unwrap();

    let mut e = engine();
    let mut driver = ScriptDriver::new(vec![
        step(
            ModelStep::Spawn(SpawnRequest {
                task: "do a thing".into(),
                contract: OutputContract::new("f", &["findings"]),
                orphan: OrphanPolicy::Terminate,
                share: BudgetShare::Small,
                grant_tokens: None,
                tools: vec![ToolId::new("bash")], // the parent does not have it
                reads_untrusted: false,
                tools_declared: true,
                role: marlowe_loop::ModelRoute::Worker,
                disposition: marlowe_loop::Disposition::Work,
            }),
            100,
        ),
        say("ok", 100),
    ]);
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
        SessionId::from_name("s"),
        narrow,
        Budget::interactive(),
        OutputContract::answer(),
    );
    let mut state = SessionState::new(run.session, "Marlowe.");
    let mut prov = Provenance::new();
    let _ = e.run(&mut run, &mut state, &mut prov, &mut ports);

    assert_eq!(recorder.count(EventKind::RunSpawned), 0, "privilege must not grow with depth");
    assert!(e.assembler().assemble(&state).rendered().contains("not available to this run"));
}

#[test]
fn the_spawn_tree_is_bounded_by_depth() {
    // Anthropic's documented deep-research failures were excessive spawning and endless loops.
    // Depth is the structural bound, and it is checked before a child exists.
    //
    // **THE DISPOSITIONS ARE LOAD-BEARING AND THE TEST WENT RED WITHOUT THEM (2026-08-31).**
    // Every spawn here used `Disposition::Work`, and after `a017ee0` that chain is
    // `Secretary --Work--> TopAgent { manages: false }`, which `AgentLevel::child_of` refuses to
    // let spawn at all. So the tree stopped after ONE child -- at the LEVEL, never reaching the
    // depth this test is named for.
    //
    // Changing the expected count from 2 to 1 would have made it green while measuring a
    // different mechanism: instance #15, in the test whose whole subject is a structural bound.
    // Instead the chain is `Manage, Manage, Work`, so levels permit all three
    // (`TopAgent { manages: true }` -> `Master` -> `Worker`) and **depth is the thing that
    // refuses the third**. The control below asserts that reason rather than trusting the count.
    let mut e = engine();
    let spawn = |task: &str, disposition: marlowe_loop::Disposition, tools: Vec<ToolId>| {
        step(
            ModelStep::Spawn(SpawnRequest {
                task: task.into(),
                contract: OutputContract::new("f", &["findings"]),
                orphan: OrphanPolicy::Terminate,
                share: BudgetShare::Standard,
                grant_tokens: None,
                tools,
                reads_untrusted: false,
                tools_declared: true,
                role: marlowe_loop::ModelRoute::Worker,
                disposition,
            }),
            10,
        )
    };
    use marlowe_loop::Disposition::{Manage, Work};
    let mut driver = ScriptDriver::new(vec![
        // **`run` has to be granted or the tree stops for a THIRD reason.** `may_create_agents`
        // reads the exposed set, so a child handed `tools: vec![]` cannot spawn whatever its
        // level permits — which is correct, and is a capability bound rather than a depth one.
        // A `Master` may now hold working tools alongside `run` (the section 1.2 reversal); `run`
        // alone is granted here because delegating is all this test needs it to do.
        spawn("depth 1", Manage, vec![ToolId::new("run")]),
        spawn("depth 2", Manage, vec![ToolId::new("run")]),
        // `Master --Work--> Worker` is a LEGAL level transition and the child needs no tools to
        // be refused, so nothing but depth can refuse this one. That is what makes the refusal
        // below attributable.
        spawn("depth 3 — refused", Work, vec![]),
        say("d2", 10),
        say("d1", 10),
        say("ok", 10),
    ]);
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

    let mut run = root(Budget { depth: 2, ..Budget::interactive() });
    let mut state = SessionState::new(run.session, "Marlowe.");
    let mut prov = Provenance::new();
    let outcome = e.run(&mut run, &mut state, &mut prov, &mut ports);

    assert!(matches!(outcome, LoopOutcome::Completed(_)));
    assert_eq!(recorder.count(EventKind::RunSpawned), 2, "two levels, then a refusal");

    // ── THE CONTROL: the third spawn was refused by DEPTH, not by level ──────────────────
    //
    // Without this, a count of 2 is satisfied by *any* refusal, which is exactly how this test
    // came to be measuring `AgentLevel::child_of` while carrying depth's name.
    //
    // **The first version of this control read the ROOT's window and could never have worked.**
    // The third spawn is refused inside the grandchild's run, so its reason never appears in the
    // parent's context — the assertion would have been red for a reason unrelated to the
    // property, which is the same defect one level along. The mechanism is asserted directly
    // instead: at `depth == 0` the grant refuses by name, and the positive control shows that a
    // level-legal spawn with depth remaining is granted, so the refusal is attributable to depth
    // and to nothing else.
    let exhausted_depth = Budget { depth: 0, ..Budget::interactive() };
    // `GrantRefused` is not re-exported from the crate root, so the variant is asserted through
    // its own `Debug` name rather than by importing it. Still by NAME: a refusal for any other
    // reason -- `PoolTooSmall`, `BelowFloor`, `MoreThanRemains` -- fails this line and says which.
    let refusal = format!(
        "{:?}",
        exhausted_depth.grant(&Budget::default(), BudgetShare::Standard, None)
    );
    assert!(
        refusal.contains("NoDepth"),
        "depth must be what refuses the third spawn, by name; got {refusal}"
    );
    let has_depth = Budget { depth: 1, ..Budget::interactive() };
    assert!(
        has_depth.grant(&Budget::default(), BudgetShare::Standard, None).is_ok(),
        "positive control: with depth remaining the same grant succeeds, so the refusal above          is depth and not something the budget refuses unconditionally"
    );
}

#[test]
fn a_childs_spend_counts_against_its_parent() {
    // A tree whose children spent from thin air would let a run cost arbitrarily more than the
    // root declared, and every per-run budget test would still pass.
    let mut e = engine();
    let mut driver = ScriptDriver::new(vec![
        step(
            ModelStep::Spawn(SpawnRequest {
                task: "t".into(),
                contract: OutputContract::new("f", &["findings"]),
                orphan: OrphanPolicy::Terminate,
                share: BudgetShare::Standard,
                grant_tokens: None,
                tools: vec![],
                reads_untrusted: false,
                tools_declared: true,
                role: marlowe_loop::ModelRoute::Worker,
                disposition: marlowe_loop::Disposition::Work,
            }),
            1_000,
        ),
        say("child thinking", 5_000),
        step(ModelStep::Say("f".into()), 1_000),
    ]);
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

    let mut run = root(Budget::interactive());
    let mut state = SessionState::new(run.session, "Marlowe.");
    let mut prov = Provenance::new();
    let _ = e.run(&mut run, &mut state, &mut prov, &mut ports);

    assert_eq!(run.spent.subagents, 1);
    assert_eq!(
        run.spent.tokens, 7_000,
        // A reply ends a run, so the child costs ONE call and the parent one more. Under the old
        // `done` contract each needed a second step to finish, which is where 8,000 came from.
        "1,000 (spawn) + 5,000 (the child's reply) + 1,000 (the parent's reply)"
    );
}

#[test]
fn a_tool_call_whose_target_came_from_untrusted_content_is_blocked_by_the_loop() {
    // The permission layer's rule, exercised through the loop rather than in isolation — the
    // point being that provenance is computed by the harness from the view, and the model
    // supplies no taint at all.
    // **The untrusted content arrives as an INJECTED MEMORY, not as a fetched page, and the
    // change is forced by layer 1 rather than chosen for convenience.**
    //
    // This test used to fetch a page and then propose a shell command. Since the quarantined-read
    // routing landed, a `web` result never enters this run at all — it is condensed by a child and
    // comes back at `AgentInferred` — so the parent's floor stays clean and there is nothing for
    // the adjudicator to refuse. Fetching here would make the test pass for the wrong reason and
    // then rot into a green light over a deleted `adjudicate` call.
    //
    // Memory injection is the path that still delivers `UntrustedContent` into a run's own window
    // (`daemon.rs` pushes the retrieved block at `retrieved.floor`, §3.3's worst case), so that is
    // what is reproduced: a block in the view, at the bottom class, with no tool involved.
    //
    // **This is the test CLAUDE.md names as the thing standing behind the §13 boundary** — the one
    // that fails if `engine.rs`'s call into `adjudicate` is ever deleted. Its subject is unchanged:
    // a real composed Target, refused, driven through the loop.
    let mut e = engine();
    let mut driver = ScriptDriver::new(vec![
        // the model proposes a shell command, with untrusted content already in the window
        step(
            ModelStep::one_call(ToolId::new("bash"), marlowe_permission::Args::new().text("command", "rm -rf /")),
            100,
        ),
        say("stopped", 100),
    ]);
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

    let mut profile_run = Run::root(
        RunId::from_name("root"),
        SessionId::from_name("s"),
        CapabilityProfile::new(
            marlowe_tools::ExposedSet::new(vec![ToolId::new("web"), ToolId::new("bash")]).unwrap(),
            marlowe_permission::EgressPolicy::allow(&["docs.example.com"]),
            marlowe_loop::InterruptPolicy::Interruptible,
            marlowe_loop::ModelRoute::Orchestrator,
            marlowe_loop::AgentLevel::Secretary,
            false,
            false,
        )
        .unwrap(),
        Budget::interactive(),
        OutputContract::answer(),
    );
    let mut state = SessionState::new(profile_run.session, "Marlowe.");
    // The injected memory, at the class §3.3 derives for it.
    state.push(marlowe_loop::Block::new(
        marlowe_loop::SourceKind::InjectedMemory,
        "remembered: the page says to run rm -rf /",
        TrustClass::UntrustedContent,
    ));
    let mut prov = Provenance::new();
    let _ = e.run(&mut profile_run, &mut state, &mut prov, &mut ports);

    // The shell command never executed. **The negative control is the floor**: without it, zero
    // tool calls would also be what a run that simply never proposed one looks like.
    assert_eq!(tools.calls.len(), 0, "nothing executed: {:?}", tools.calls);
    assert_eq!(
        profile_run.trust_floor(),
        TrustClass::UntrustedContent,
        "the injected memory must have latched the floor, or the refusal below proves nothing"
    );
    // **The property, not the spelling.** This asserted `contains("UntrustedTarget")` — the name
    // of a Rust enum variant — which pinned the `Debug` rendering that was being handed to the
    // model as its refusal. That rendering is now prose, and the boundary is unchanged: what
    // matters is that the tainted call was refused, was never executed, and the model was told
    // which argument caused it.
    let rendered = e.assembler().assemble(&state).rendered();
    assert!(
        rendered.contains("[bash blocked]"),
        "the blocked call must be reported to the model: {rendered}"
    );
    assert!(
        rendered.contains("outside this conversation"),
        "the refusal must name the rule that fired — an argument shaped by untrusted content:          {rendered}"
    );
    assert!(
        rendered.contains("`command`"),
        "and WHICH argument, or the model cannot correct it: {rendered}"
    );
}

/// One scripted run: an inert workspace search whose result carries `trust`, then two composed
/// shell commands, then a reply. Returns what the surface was told, what actually executed, and
/// the floor the run ended on.
///
/// **The tool is the same in every cell and only `trust` varies.** That is what makes a cell
/// reading zero evidence about the guard rather than evidence about the harness: the identical
/// path emits one in another cell.
fn run_latching(trust: TrustClass) -> (usize, usize, TrustClass) {
    let mut e = engine();
    let mut driver = ScriptDriver::new(vec![
        // `grep` with only `pattern` — Inert, and it declares no path, so the `Unavailable`
        // scope in `engine()` is never consulted. An ordinary workspace read.
        step(
            ModelStep::one_call(ToolId::new("grep"), marlowe_permission::Args::new().text("pattern", "TODO")),
            100,
        ),
        // Two composed Targets AFTER the result is in view, not one: a second iteration is what
        // would expose a banner that re-announces on every pass.
        step(
            ModelStep::one_call(ToolId::new("bash"), marlowe_permission::Args::new().text("command", "echo one")),
            100,
        ),
        step(
            ModelStep::one_call(ToolId::new("bash"), marlowe_permission::Args::new().text("command", "echo two")),
            100,
        ),
        say("stopped", 100),
    ]);
    let mut summarizer = EmptySummarizer;
    // **The tool result is neutral; the trust class under test arrives as an injected memory.**
    //
    // It used to arrive as this tool's result. Layer 1 ended that: an `UntrustedContent` tool
    // result is condensed by a quarantined child and never reaches this run, so the
    // `UntrustedContent` row of the table below would have exercised a floor that no longer
    // moves — the table would still have four rows and one of them would have been measuring
    // nothing. Injected memory is the path that still delivers the bottom class into a run's own
    // window (§3.3's worst case, `daemon.rs`), and it reproduces the original four rows exactly.
    let mut tools = ScriptedTools {
        body: Some("a line from the workspace".into()),
        trust: Some(TrustClass::AgentObserved),
        ..Default::default()
    };
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
        CapabilityProfile::new(
            marlowe_tools::ExposedSet::new(vec![ToolId::new("grep"), ToolId::new("bash")]).unwrap(),
            marlowe_permission::EgressPolicy::DenyAll,
            marlowe_loop::InterruptPolicy::Interruptible,
            marlowe_loop::ModelRoute::Orchestrator,
            marlowe_loop::AgentLevel::Secretary,
            false,
            false,
        )
        .unwrap(),
        Budget::interactive(),
        OutputContract::answer(),
    );
    let mut state = SessionState::new(run.session, "Marlowe.");
    // The class under test, delivered the way memory injection delivers it.
    state.push(marlowe_loop::Block::new(
        marlowe_loop::SourceKind::InjectedMemory,
        "a remembered line",
        trust,
    ));
    let mut prov = Provenance::new();
    let _ = e.run(&mut run, &mut state, &mut prov, &mut ports);

    let announced = sink
        .events
        .iter()
        .filter(|ev| {
            matches!(
                ev,
                marlowe_loop::TurnEvent::Degraded {
                    what: marlowe_loop::DegradedPath::TrustFloorLatched
                }
            )
        })
        .count();
    let shells = tools.calls.iter().filter(|(t, _)| t == "bash").count();
    (announced, shells, run.trust_floor())
}

/// **The banner claims what the wall does, at every trust class.** (M2 C2f, item 1.)
///
/// The defect this pins: the loop announced `Degraded{TrustFloorLatched}` on *any* downward move
/// of the floor, and the surface renders that as *"read untrusted · composed targets blocked"*.
/// A run starts at `UserAsserted`, so the first assistant turn (`AgentInferred`) or the first
/// plain workspace read (`AgentObserved`) tripped it — and **every live run printed the banner at
/// its second iteration with both clauses false.**
///
/// It is the capability-report family that CLAUDE.md tracks. The event fired on *floor moved*,
/// the text asserted *floor reached untrusted*, and the banner read the same whether or not the
/// guard was working — so it was never evidence about the guard. A latch that fires on everything
/// means nothing, which is the state it must not be in when `web` makes it live.
///
/// Asserted as the agreement rather than as either half: across all four classes, the surface
/// announces **iff** the adjudicator refuses.
#[test]
fn the_latch_announces_exactly_when_a_composed_target_is_actually_blocked() {
    for trust in [
        TrustClass::UserAsserted,
        TrustClass::AgentObserved,
        TrustClass::AgentInferred,
        TrustClass::UntrustedContent,
    ] {
        let (announced, shells, floor) = run_latching(trust);
        let blocks = marlowe_permission::blocks_composed_targets(floor);

        assert_eq!(
            announced,
            usize::from(blocks),
            "at tool trust {trust:?} the run ended on floor {floor:?}, which \
             blocks_composed_targets={blocks} — the banner must agree with the wall, and it \
             announced {announced} time(s)"
        );
        assert_eq!(
            shells,
            if blocks { 0 } else { 2 },
            "at tool trust {trust:?}, floor {floor:?}: the composed shell commands must run \
             exactly when the banner stays silent. {shells} ran"
        );
    }
}

/// The half of the above that a reader will want stated without arithmetic, plus the property
/// the `usize::from(blocks)` above hides: it is announced **once**, not once per iteration.
#[test]
fn an_ordinary_workspace_read_is_silent_and_a_fetched_page_announces_once() {
    let (announced, shells, floor) = run_latching(TrustClass::AgentObserved);
    assert_eq!(
        announced, 0,
        "a plain workspace read moves the floor UserAsserted -> AgentObserved and blocks \
         nothing; announcing it is the false claim this test exists for"
    );
    assert_eq!(shells, 2, "and the composed calls still run");
    assert_eq!(floor, TrustClass::AgentInferred, "the floor did move — it is the CLAIM that was wrong");

    let (announced, shells, floor) = run_latching(TrustClass::UntrustedContent);
    assert_eq!(
        announced, 1,
        "untrusted content in view announces, and announces ONCE across two later iterations \
         — the latch is monotonic, so the transition happens at most once"
    );
    assert_eq!(shells, 0, "and every composed Target after it is refused");
    assert_eq!(floor, TrustClass::UntrustedContent);
}

/// **What actually tripped the banner was Marlowe's own name.** (M2 C2f, item 1.)
///
/// STATE recorded this as firing *"after a plain `read`"*. The read was never needed. The
/// assembler constructs the stable tier on every assemble and the `Identity` block — the string
/// `"Marlowe."` — is `AgentObserved` (`context.rs`, `Assembler::assemble`). A run starts at
/// `UserAsserted`, so **the floor moved on the first assemble of every run that has ever run**,
/// before the model spoke and before any tool executed. The first assistant turn
/// (`AgentInferred`) then moved it a second time.
///
/// So the product printed *"read untrusted · composed targets blocked"* on **iteration one of
/// every run**, and again at the first assistant turn: the negative control for the fix above
/// reads `left: 2, right: 0` on a multi-iteration run for exactly those two moves. A run that
/// replies immediately, as here, only gets as far as the first.
///
/// The floor moving here is CORRECT and is left alone: `AgentObserved` is genuinely the worst
/// class in view, and `taint_for` needs that value. Only the claim was wrong. This test pins both
/// halves so a later change cannot "fix" the trajectory instead of the claim.
#[test]
fn the_floor_moves_on_the_identity_block_alone_and_the_screen_says_nothing() {
    let mut e = engine();
    let mut driver = ScriptDriver::new(vec![say("nothing to do", 100)]);
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

    let mut run = root(Budget::interactive());
    let mut state = SessionState::new(run.session, "Marlowe.");
    let mut prov = Provenance::new();
    let _ = e.run(&mut run, &mut state, &mut prov, &mut ports);

    assert_eq!(tools.calls.len(), 0, "no tool ran, which is the point");

    // The JOURNAL still records the whole trajectory — an audit that only recorded the blocking
    // move could not answer "when did this run stop being user-asserted".
    let floors: Vec<String> = recorder
        .payloads(EventKind::TrustFloorLatched)
        .iter()
        .filter_map(|p| p.get("floor").and_then(|f| f.as_str()).map(str::to_string))
        .collect();
    assert_eq!(
        floors,
        vec!["AgentObserved".to_string()],
        "the identity block alone moves the floor on the first assemble, with no tool call \
         anywhere in the run"
    );

    // ...and the SCREEN claims nothing, because nothing is blocked.
    let announced = sink
        .events
        .iter()
        .filter(|ev| {
            matches!(
                ev,
                marlowe_loop::TurnEvent::Degraded {
                    what: marlowe_loop::DegradedPath::TrustFloorLatched
                }
            )
        })
        .count();
    assert_eq!(announced, 0, "a toolless run must never claim it read untrusted content");
}

/// **A partially-failing batch tells the model WHICH call failed.** (M2 C2f.)
///
/// Three calls in one message: two `grep`s that run and a `read` that path scoping refuses. The
/// model must be able to tell them apart, and `tool_name` cannot do it — two of the three share a
/// name. Without `tool_call_id` a partial failure reads as a total one, or the model retries the
/// call that worked.
#[test]
fn every_result_in_a_batch_is_attributable_to_the_call_that_produced_it() {
    let mut e = engine();
    let mut driver = ScriptDriver::new(vec![
        step(
            ModelStep::ToolCall {
                calls: vec![
                    marlowe_loop::ToolInvocation {
                        id: "call_1".into(),
                        tool: ToolId::new("grep"),
                        args: marlowe_permission::Args::new().text("pattern", "alpha"),
                    },
                    // `Unavailable` path scoping refuses this one, and only this one.
                    marlowe_loop::ToolInvocation {
                        id: "call_2".into(),
                        tool: ToolId::new("read"),
                        args: marlowe_permission::Args::new().text("path", "/etc/passwd"),
                    },
                    marlowe_loop::ToolInvocation {
                        id: "call_3".into(),
                        tool: ToolId::new("grep"),
                        args: marlowe_permission::Args::new().text("pattern", "beta"),
                    },
                ],
            },
            100,
        ),
        say("stopped", 100),
    ]);
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
        SessionId::from_name("s"),
        CapabilityProfile::new(
            marlowe_tools::ExposedSet::new(vec![ToolId::new("grep"), ToolId::new("read")]).unwrap(),
            marlowe_permission::EgressPolicy::DenyAll,
            marlowe_loop::InterruptPolicy::Interruptible,
            marlowe_loop::ModelRoute::Orchestrator,
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
    let _ = e.run(&mut run, &mut state, &mut prov, &mut ports);

    // **All three were attempted**, which is the whole point — the old adapter ran one.
    let results: Vec<_> = state
        .volatile
        .iter()
        .filter(|b| b.source == marlowe_loop::SourceKind::ToolResults)
        .collect();
    assert_eq!(results.len(), 3, "every call in the batch produces a result");

    // Each carries a DISTINCT call id, and the ids match the ones the assistant turn declared.
    let ids: Vec<&str> = results
        .iter()
        .filter_map(|b| b.wire.as_ref()?.tool_call_id.as_deref())
        .collect();
    assert_eq!(ids, vec!["call_1", "call_2", "call_3"], "results are attributed in order");

    // **One assistant turn declaring all three**, not three turns. `/api/chat` puts `tool_calls`
    // on the assistant message, and three separate turns would misrepresent what the model did.
    let declared: Vec<usize> = state
        .volatile
        .iter()
        .filter_map(|b| b.wire.as_ref())
        .filter(|w| !w.tool_calls.is_empty())
        .map(|w| w.tool_calls.len())
        .collect();
    assert_eq!(declared, vec![3], "one assistant turn, declaring the whole batch");

    // The failure is legible as ONE failure among three.
    let blocked: Vec<&str> = results
        .iter()
        .filter(|b| b.wire.as_ref().is_some_and(|w| w.tool_failed))
        .filter_map(|b| b.wire.as_ref()?.tool_call_id.as_deref())
        .collect();
    assert_eq!(blocked, vec!["call_2"], "exactly the refused call is marked failed, by id");
    assert_eq!(tools.calls.len(), 2, "the refused call never reached the executor");
}

/// **A batch is not a hole in ADR-023's latch.** (M2 C2f.)
///
/// Taint is computed once, from the pre-batch view — which is *correct*, because every call in the
/// batch was composed before any of their results existed, so none can have been shaped by a
/// sibling. The property that must hold is the ordering: the floor latches from the batch's
/// results before the NEXT batch is adjudicated.
///
/// So `web`-then-`bash` **inside one batch** both run, and the same `bash` **in the next batch** is
/// refused. Asserting only the first half would pass on a build with no latch at all; asserting
/// only the second would pass on a build that recomputed taint mid-batch, which would be wrong for
/// a different reason. Both halves, in one test.
#[test]
fn a_batch_cannot_launder_a_target_through_its_own_sibling() {
    let mut e = engine();
    let composed = || marlowe_permission::Args::new().text("command", "echo composed");
    let mut driver = ScriptDriver::new(vec![
        step(
            ModelStep::ToolCall {
                calls: vec![
                    marlowe_loop::ToolInvocation {
                        id: "call_1".into(),
                        tool: ToolId::new("grep"),
                        args: marlowe_permission::Args::new().text("pattern", "x"),
                    },
                    marlowe_loop::ToolInvocation {
                        id: "call_2".into(),
                        tool: ToolId::new("bash"),
                        args: composed(),
                    },
                ],
            },
            100,
        ),
        // **ONE scripted reply for the whole run, and the count is the cost model (ADR-041).**
        //
        // This fixture marks every tool result untrusted, so all three executions are condensed.
        // Two changes cut what that costs, and neither touches the four properties below:
        //
        //   1. a group of untrusted results is read by ONE quarantined child, and
        //   2. condensed documents are cached by CONTENT hash.
        //
        // `ScriptedTools` returns byte-identical bodies, so only the FIRST condensation is a cache
        // miss; the batch's `bash` and the next turn's `bash` are both hits and cost no model call
        // at all. (`grep` is Inert and `bash` is Irreversible, so they are separate groups — see
        // `tests/batch_grouping.rs`.)
        //
        // Getting this count wrong does not fail loudly: a child eats the next turn's step and the
        // parent runs a shorter script than the author intended, which is exactly how this read
        // `1` where it expected `2`.
        say("the find result is about widgets", 50), // the only cache miss
        // The next turn's identical call.
        step(ModelStep::one_call(ToolId::new("bash"), composed()), 100),
        say("stopped", 100),
    ]);
    let mut summarizer = EmptySummarizer;
    // Every result is untrusted, so the batch's own results lower the floor.
    let mut tools = ScriptedTools {
        body: Some("a fetched page".into()),
        trust: Some(TrustClass::UntrustedContent),
        ..Default::default()
    };
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
        CapabilityProfile::new(
            marlowe_tools::ExposedSet::new(vec![ToolId::new("grep"), ToolId::new("bash")]).unwrap(),
            marlowe_permission::EgressPolicy::DenyAll,
            marlowe_loop::InterruptPolicy::Interruptible,
            marlowe_loop::ModelRoute::Orchestrator,
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
    let _ = e.run(&mut run, &mut state, &mut prov, &mut ports);

    // Half one: inside the batch, `bash` ran. Its command predates the sibling's result, and
    // blocking it would be blocking a call on content its author had never seen.
    //
    // **Asserted as ORDER, not as a count.** This used to read "exactly one bash call in the
    // whole run", which stood in for "the in-batch one ran" only while the next turn's call was
    // refused. Now that both run, a count says nothing about which one this half is about --
    // the first two executions being `grep` then `bash` is the property, and it stays true
    // whatever happens later in the run.
    let order: Vec<&str> = tools.calls.iter().map(|(t, _)| t.as_str()).collect();
    assert_eq!(
        &order[..2],
        &["grep", "bash"],
        "the in-batch shell command must run, in batch order: it was composed before the \
         sibling's result existed. Got {order:?}"
    );

    // ── Half two, REPLACED, and the replacement is the honest statement of what changed ──────
    //
    // This half used to assert that the identical call in the NEXT turn is refused, because the
    // batch's own untrusted results had latched the floor. **Layer 1 makes that scenario
    // unreachable**: an untrusted tool result is condensed by a quarantined child and never
    // enters this window, so no tool result — in a batch or out of it — moves the parent's floor
    // any more. Keeping the old assertion would have meant re-introducing a raw untrusted result
    // purely so a test could observe it, which is writing the hole back in to keep its guard.
    //
    // The ordering property itself is NOT abandoned. It is asserted where it still has a subject:
    // `run_latching` drives all four trust classes through an injected memory, and
    // `the_latch_announces_exactly_when_a_composed_target_is_actually_blocked` asserts the
    // agreement between the announcement and the refusal across every one of them.
    assert_eq!(
        run.trust_floor(),
        TrustClass::AgentInferred,
        "the batch's untrusted results were condensed away and never touched this run's floor"
    );
    let rendered = e.assembler().assemble(&state).rendered();
    assert!(
        !rendered.contains("[bash blocked]"),
        "with the floor clean, the next turn's identical command is no longer refused:\n{rendered}"
    );
    // Two shell calls in total: the in-batch one and the next turn's.
    assert_eq!(
        tools.calls.iter().filter(|(t, _)| t == "bash").count(),
        2,
        "the next turn's command ran too, because nothing tainted the run"
    );
}

/// **A decline by a present human is not a hard block, and the model must not be told it is.**
/// (M2 C2f, found on the first live TUI approval.)
///
/// One message served both situations, and it was the unattended one: *"no interactive approval
/// surface is attached to this run … retrying it will fail the same way."* Declining in the TUI
/// therefore told the model that no surface existed — false, it was on screen — and the model
/// reasonably reported the capability hard-blocked and stopped trying anything.
///
/// The three cases are asserted together because the defect was that they were one.
#[test]
fn a_declined_call_reads_differently_depending_on_whether_anyone_could_be_asked() {
    /// Refuses, and says whether a human was there and what they said.
    struct Gate {
        interactive: bool,
        reason: Option<String>,
    }
    impl marlowe_loop::ApprovalGate for Gate {
        fn is_interactive(&self) -> bool {
            self.interactive
        }
        fn decline_reason(&self) -> Option<String> {
            self.reason.clone()
        }
        fn await_approval(&mut self, _r: &marlowe_permission::BlastRadius) -> bool {
            false
        }
    }

    let rendered_with = |gate: Gate| -> String {
        let mut e = engine();
        let mut driver = ScriptDriver::new(vec![
            step(
                ModelStep::one_call(
                    ToolId::new("bash"),
                    marlowe_permission::Args::new().text("command", "echo hi"),
                ),
                100,
            ),
            say("stopped", 100),
        ]);
        let mut summarizer = EmptySummarizer;
        let mut tools = ScriptedTools::default();
        let mut approvals = gate;
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
        let mut run = root(Budget::interactive());
        let mut state = SessionState::new(run.session, "Marlowe.");
        let mut prov = Provenance::new();
        let _ = e.run(&mut run, &mut state, &mut prov, &mut ports);
        e.assembler().assemble(&state).rendered()
    };

    // 1. Nobody could be asked. This is the only case where "unavailable" is true.
    let unattended = rendered_with(Gate { interactive: false, reason: None });
    assert!(
        unattended.contains("no interactive approval surface"),
        "an unattended run must still say so: {unattended}"
    );

    // 2. A human declined, silently. The tool is NOT unavailable.
    let declined = rendered_with(Gate { interactive: true, reason: None });
    assert!(
        !declined.contains("no interactive approval surface"),
        "THE DEFECT: a present human declining must not be reported as an absent surface.          The model reads this and stops trying anything at all:
{declined}"
    );
    assert!(
        declined.contains("DECLINED"),
        "it must say what actually happened: {declined}"
    );

    // 3. A human declined with a reason, which is guidance rather than a dead end.
    let with_reason =
        rendered_with(Gate { interactive: true, reason: Some("wrong host".into()) });
    assert!(
        with_reason.contains("wrong host"),
        "the user's reason must reach the model verbatim: {with_reason}"
    );
    assert!(
        !with_reason.contains("no interactive approval surface"),
        "still not an absent surface: {with_reason}"
    );
}

/// A manifest that declares an `Amount`, built by hand. **This exists because no builtin declares
/// one any more**, and saying so is the point — see the two tests below.
fn a_manifest_declaring_an_amount() -> marlowe_tools::CapabilityManifest {
    marlowe_tools::load(
        marlowe_tools::RawManifest {
            tool: ToolId::new("pay"),
            paths: Vec::new(),
            hosts: Vec::new(),
            creds: Vec::new(),
            consequence: Some(marlowe_tools::ConsequenceLevel::Irreversible),
            params: vec![
                marlowe_tools::RawParamSpec {
                    name: "amount".into(),
                    role: Some(marlowe_tools::ArgumentRole::Target),
                    ty: marlowe_tools::ParamType::Amount,
                    required: true,
                    description: None,
                },
                marlowe_tools::RawParamSpec {
                    name: "task".into(),
                    role: Some(marlowe_tools::ArgumentRole::Payload),
                    ty: marlowe_tools::ParamType::Text,
                    required: false,
                    description: None,
                },
            ],
        },
        marlowe_tools::ManifestProvenance::FirstParty,
    )
    .expect("a hand-built manifest with declared roles loads")
}

/// **A declared `Amount` reaches the approval prompt as money, not as a bare integer.**
/// (M2 C2f.)
///
/// JSON has one number type, so a money amount arrives from any provider as `ArgValue::Integer`
/// — meaning `ParamType::Amount` was a declared type the runtime value never took, and every
/// branch on `Amount` was dead on the model path. The visible consequence is the §B9 scope line:
/// `2500000` is a number a human approves after reading it as dollars.
///
/// **THE SUBJECT MOVED AND THE TEST DID NOT FOLLOW IT — DELIBERATELY.** This used to run against
/// `run.budget_micros_usd`, the only builtin parameter typed `Amount`. ADR-057 §6 renamed it to
/// `budget_tokens` and retyped it `Integer`, because `SpawnRequest::grant_tokens` is tokens and
/// `Amount` is documented as money. Pointing this test at `budget_tokens` would have been the
/// obvious edit and it would have been **vacuous**: `Integer` declared and `Integer` supplied means
/// `coerce_to_declared_types` does nothing, and the test would be green on a build where the
/// function was `fn coerce(_, args) { args }`.
///
/// So it runs against a hand-built manifest instead, and the honest consequence is recorded here:
/// **`coerce_to_declared_types` currently has no live instance in the builtin set.** It is one arm
/// wide, that arm is `Amount`, and nothing shipped declares one. That is a finding, not a fix — the
/// function is correct, it is simply unexercised by the product until a spend ceiling returns at M6.
#[test]
fn a_declared_amount_is_rendered_as_money_in_the_scope_line() {
    let manifest = a_manifest_declaring_an_amount();

    // What a provider actually produces for `"amount": 2500000`.
    let raw = marlowe_permission::Args::new()
        .text("task", "summarise")
        .with("amount", marlowe_permission::ArgValue::Integer(2_500_000));

    let coerced = marlowe_loop::coerce_to_declared_types(&manifest, raw);
    assert_eq!(
        coerced.get("amount"),
        Some(&marlowe_permission::ArgValue::Amount(2_500_000)),
        "the manifest declares Amount, so that is what the adjudicator should see"
    );
    assert!(
        coerced.get("amount").map(|v| v.render()).unwrap_or_default().contains("2.500000"),
        "and it renders as money rather than as a raw micro count"
    );

    // Untouched types stay untouched.
    assert_eq!(
        coerced.get("task"),
        Some(&marlowe_permission::ArgValue::Text("summarise".into()))
    );
}

/// The control for the above, and it is what makes the pair non-vacuous: the coercion must be
/// driven by the **declared type**, not applied to every integer it sees. `run.budget_tokens` is
/// declared `Integer`, so an integer must come out the other side unchanged.
///
/// Without this, a `coerce_to_declared_types` that turned every non-negative integer into an
/// `Amount` would pass the test above and would render a 12,000-token grant as `0.012000` on the
/// line a human approves.
#[test]
fn an_integer_declared_as_an_integer_is_not_turned_into_money() {
    let r = builtin_registry().expect("the builtins load");
    let manifest = r.manifest(&ToolId::new("run")).expect("`run` is a builtin");
    let raw = marlowe_permission::Args::new()
        .with("budget_tokens", marlowe_permission::ArgValue::Integer(12_000));
    let coerced = marlowe_loop::coerce_to_declared_types(manifest, raw);
    assert_eq!(
        coerced.get("budget_tokens"),
        Some(&marlowe_permission::ArgValue::Integer(12_000)),
        "ADR-057 §6: a token grant is not money and must not render as a currency figure"
    );
}

/// A nonsensical value is passed through rather than made plausible. Clamping a negative amount
/// to zero would hand the adjudicator a number nobody sent.
#[test]
fn a_negative_amount_is_not_quietly_turned_into_a_valid_one() {
    let manifest = a_manifest_declaring_an_amount();
    let raw = marlowe_permission::Args::new()
        .with("amount", marlowe_permission::ArgValue::Integer(-5));
    let coerced = marlowe_loop::coerce_to_declared_types(&manifest, raw);
    assert_eq!(
        coerced.get("amount"),
        Some(&marlowe_permission::ArgValue::Integer(-5)),
        "left as it arrived: the adjudicator should see what was actually sent"
    );
}

/// **An empty turn must never quietly succeed.** (M2 C2e, issue 2.)
///
/// Completion is the absence of an action, and an empty reply is technically that — so without a
/// guard, a model returning nothing repeatedly would END THE RUN with an empty answer, reported as
/// success. That is the difference between "Marlowe answered" and "Marlowe said nothing and we
/// called it done".
#[test]
fn a_model_that_returns_nothing_fails_the_run_rather_than_completing_it() {
    let mut e = engine();
    // Four empty turns: three are nudged, the fourth exhausts the allowance.
    let mut driver = ScriptDriver::new((0..4).map(|_| say("", 10)).collect());
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

    let mut run = root(Budget::interactive());
    let mut state = SessionState::new(run.session, "Marlowe.");
    let mut prov = Provenance::new();
    let outcome = e.run(&mut run, &mut state, &mut prov, &mut ports);

    match outcome {
        LoopOutcome::Failed { error } => assert!(
            error.contains("no reply and no tool call"),
            "the failure must name what happened: {error}"
        ),
        other => panic!("an empty turn was reported as {other:?} rather than a failure"),
    }
}

/// The nudge is retried a bounded number of times before that failure.
#[test]
fn an_empty_turn_is_nudged_before_it_is_failed() {
    let mut e = engine();
    // Two empties then a real reply: the nudges recover the turn.
    let mut driver = ScriptDriver::new(vec![say("", 10), say("", 10), say("here it is", 10)]);
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

    let mut run = root(Budget::interactive());
    let mut state = SessionState::new(run.session, "Marlowe.");
    let mut prov = Provenance::new();
    let outcome = e.run(&mut run, &mut state, &mut prov, &mut ports);

    assert!(
        matches!(outcome, LoopOutcome::Completed(_)),
        "two empty turns should be recovered by the nudge, not fatal: {outcome:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────────────────
// ADR-023 — the trust floor latches for the run's whole lifetime
// ─────────────────────────────────────────────────────────────────────────────────────────

/// **The specific failure, not the mechanism.**
///
/// `taint_for` used `ContextView::trust_floor()` — the minimum over the blocks *currently* in the
/// view. `ToolResults` is trimmable, so the assembler could drop an untrusted block to stay inside
/// its per-source budget and the floor would **rise again**: the run silently regaining privileges
/// ADR-023 says it loses permanently, with no error and no event.
///
/// This drives that exact sequence — untrusted content in, then enough pressure to trim it out —
/// and asserts the floor holds. Asserting only that a latch exists would pass against a latch that
/// never engaged.
#[test]
fn the_trust_floor_holds_after_the_untrusted_block_is_trimmed_out_of_the_view() {
    use marlowe_loop::{Assembler, Block, SourceKind};

    let mut run = root(Budget::interactive());
    assert_eq!(
        run.trust_floor(),
        TrustClass::UserAsserted,
        "a fresh run has seen nothing untrusted"
    );

    let mut state = SessionState::new(run.session, "Marlowe.");

    // A tool returns untrusted content — a web page, an inbound mail, an MCP payload.
    state.push(Block::new(
        SourceKind::ToolResults,
        format!("UNTRUSTED-PAGE {}", "p".repeat(500)),
        TrustClass::UntrustedContent,
    ));

    // A small window, so the next pushes actually force the assembler to trim.
    let assembler = Assembler::new(2_000, 200);
    let view = assembler.assemble(&state);
    assert_eq!(
        view.trust_floor(),
        TrustClass::UntrustedContent,
        "the untrusted block must be IN the view at this point, or the trim below proves nothing"
    );
    assert_eq!(
        run.latch_trust_floor(view.trust_floor()),
        Some(TrustClass::UntrustedContent),
        "the latch must engage the first time untrusted content is seen"
    );

    // Now bury it: enough trimmable bulk that the assembler drops the untrusted block.
    for i in 0..40 {
        state.push(Block::new(
            SourceKind::ToolResults,
            format!("filler-{i} {}", "f".repeat(500)),
            TrustClass::AgentObserved,
        ));
    }
    let later = assembler.assemble(&state);

    // The anti-vacuity check: the block really is gone from the view.
    assert!(
        !later.rendered().contains("UNTRUSTED-PAGE"),
        "the untrusted block was not trimmed, so this test is not exercising the failure it \
         exists for:\n{}",
        later.rendered()
    );
    // ── WHY THIS ASSERTION IS NOW THE OPPOSITE OF WHAT IT WAS ───────────────────────────
    //
    // It read `assert_eq!(later.trust_floor(), AgentObserved)`, with the message *"the VIEW's
    // floor rose, which is the behaviour that made the hole reachable"* — and that was accurate
    // when it was written. **Audit finding F1 closed the route.** `trim_to_budget`'s omission
    // branch used to stamp its marker at a hardcoded `AgentObserved`; it now carries the `min`
    // of the classes it swallowed, so evicting an untrusted block to stay inside a per-source
    // budget no longer un-taints the view.
    //
    // The view's floor is therefore monotone-faithful under every lever that shortens it:
    // truncation always carried `b.trust`, `clear_tool_results` always preserved it, omission
    // now does, and `Assembler::compact` does too (E5). **The taint survives its own eviction.**
    assert_eq!(
        later.trust_floor(),
        TrustClass::UntrustedContent,
        "F1: the omission marker stands in for an untrusted block and carries its class, so the \
         view's floor must NOT rise when the per-source budget evicts the taint. If this reads \
         AgentObserved the marker is a constant again, and the budget — not the content — is \
         deciding when a run stops being tainted."
    );

    // ...and the run's floor did not move with it.
    assert_eq!(
        run.latch_trust_floor(later.trust_floor()),
        None,
        "the latch must never RAISE the floor"
    );
    assert_eq!(
        run.trust_floor(),
        TrustClass::UntrustedContent,
        "the run regained privileges it read untrusted content to lose. ADR-023: once a run has \
         read untrusted content, every model-composed target in that run is blocked — `once has` \
         is a property of the run, not of whatever survived the last trim"
    );

    // ── THE LATCH, NOW THAT THE ASSEMBLER CANNOT PRODUCE ITS INPUT ──────────────────────
    //
    // **Stated plainly, because it is a finding and not a caveat.** With E5 and F1 fixed there
    // is no lever left in the assembler that raises a view's floor. So nothing above this line
    // hands `latch_trust_floor` an observation cleaner than what the run has already seen, and
    // the monotonicity this test is named for is no longer *exercised* by the trim it was built
    // for. The latch has become defence in depth rather than the only thing standing there.
    //
    // That is a reason to assert it directly, not a reason to drop it: `SessionState` blocks are
    // shortened today, but a future path that *removes* one — or a fourth lever added without
    // this file being read — puts the latch back on the critical path. `UserAsserted` is the
    // cleanest observation there is, and the floor must ignore it.
    assert_eq!(
        run.latch_trust_floor(TrustClass::UserAsserted),
        None,
        "monotonic: no observation, however clean, raises a latched floor"
    );
    assert_eq!(run.trust_floor(), TrustClass::UntrustedContent);
    assert!(
        marlowe_permission::blocks_composed_targets(run.trust_floor()),
        "and the floor is asserted where it is ENFORCED — the same predicate the adjudicator \
         calls — never on the fact that it moved"
    );
}

/// A child inherits its parent's floor: a spawn is a narrowing with no widening path.
#[test]
fn a_child_cannot_be_less_tainted_than_the_run_that_spawned_it() {
    let mut parent = root(Budget::interactive());
    parent.latch_trust_floor(TrustClass::UntrustedContent);

    let child = Run::child(
        RunId::from_name("child"),
        &parent,
        SessionId::from_name("child-session"),
        CapabilityProfile::quarantined_reader(),
        Budget::interactive(),
        OrphanPolicy::Terminate,
        OutputContract::new("findings", &["findings"]),
    );

    assert_eq!(
        child.trust_floor(),
        TrustClass::UntrustedContent,
        "a child started clean would launder exactly what ADR-023 blocks: its task string was \
         model-composed from the parent's tainted window"
    );
}

/// **A tool the model can see but never execute is unrepresentable.**
///
/// Five instances: `done` (which cost a run 155 seconds of retrying a failure it could not read),
/// then `web`, `recall` and `use`, all three exposed by `interactive()` against a host with arms
/// for four tools. Each passed every unit test, because the registry, the profile and the host
/// were individually correct and nothing owned the seam between them.
#[test]
fn the_interactive_profile_exposes_nothing_the_tool_host_cannot_run() {
    struct FourTools;
    impl marlowe_loop::ToolHost for FourTools {
        fn execute(
            &mut self,
            _t: &marlowe_tools::ToolId,
            _a: &marlowe_permission::Args,
            _adj: &marlowe_permission::Adjudication,
        ) -> marlowe_loop::ToolOutcome {
            unreachable!("not called")
        }
        fn executes(&self) -> Vec<marlowe_tools::ToolId> {
            // **`web` joined in M2 C2f, `recall` in M2 Session D and `use` in M2 C3; this stub had
            // to follow all three times**, which is the guard working in the direction nobody
            // writes a test for: it fires when the shipped profile grows a tool the host does not
            // claim, not only when a host shrinks. Three times now the failure has been a correct
            // refusal rather than a bug.
            //
            // `recall`'s real executor is `marlowe_daemon::recall::RecallTools` and `use`'s is
            // `marlowe_daemon::skills::SkillTools`, both in the daemon: one needs the belief store
            // and the other the skill registry, and `marlowe-exec` must learn about neither. That
            // is why this crate's stub names them rather than running them — and why the daemon
            // verifies **the host it will actually use**, a gap that was open until Session D
            // closed it.
            // **Fourth time: `write`, when it was split out of `edit`.**
            // **Fifth time: `glob`.**
            ["read", "write", "edit", "glob", "grep", "bash", "web", "recall", "use"]
                .iter()
                .map(|t| marlowe_tools::ToolId::new(*t))
                .collect()
        }
    }

    marlowe_loop::verify_every_exposed_tool_is_runnable(
        marlowe_loop::CapabilityProfile::interactive().exposed_tools(),
        &FourTools,
    )
    .expect("the shipped profile must not offer a tool that cannot run");
}

/// **The negative control.** A guard that cannot fail is a comment.
#[test]
fn the_guard_names_a_tool_that_has_no_executor() {
    struct NoTools;
    impl marlowe_loop::ToolHost for NoTools {
        fn execute(
            &mut self,
            _t: &marlowe_tools::ToolId,
            _a: &marlowe_permission::Args,
            _adj: &marlowe_permission::Adjudication,
        ) -> marlowe_loop::ToolOutcome {
            unreachable!("not called")
        }
        fn executes(&self) -> Vec<marlowe_tools::ToolId> {
            vec![]
        }
    }

    let exposed = marlowe_tools::ExposedSet::new(vec![
        marlowe_tools::ToolId::new("web"),
        // Loop control: representable without a host executor, so it must NOT be named.
        marlowe_tools::ToolId::new("ask"),
    ])
    .unwrap();

    let err = marlowe_loop::verify_every_exposed_tool_is_runnable(&exposed, &NoTools)
        .expect_err("a host with no executors must reject an exposed `web`");
    assert_eq!(err.0, vec!["web".to_string()], "`ask` is loop control and is exempt");
    assert!(err.to_string().contains("web"), "the error must name the tool: {err}");
}

/// **A refused tool call is still a visible tool call.**
///
/// Both refusal paths — adjudication `Blocked` and a denied approval — returned before the
/// `ToolLine` emit, so the model was told and the user was not. A run that tried three times and
/// was refused three times rendered as a run that never tried: no line, no failure, just an
/// answer that mentioned restrictions. Reported live as "tool calls are now missing".
#[test]
fn a_blocked_tool_call_still_emits_a_tool_line() {
    use marlowe_loop::{ToolLineState, TurnEvent};

    let mut e = engine();
    let mut driver = ScriptDriver::new(vec![
        step(
            // Path scoping is `Unavailable` in this engine, so this is refused.
            ModelStep::one_call(
                ToolId::new("read"),
                marlowe_permission::Args::new().text("path", "/etc/passwd"),
            ),
            10,
        ),
        say("could not read it", 10),
    ]);
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

    let mut run = root(Budget::interactive());
    let mut state = SessionState::new(run.session, "Marlowe.");
    let mut prov = Provenance::new();
    let _ = e.run(&mut run, &mut state, &mut prov, &mut ports);

    assert!(tools.calls.is_empty(), "the call must actually have been refused, or this is vacuous");

    let lines: Vec<&TurnEvent> = sink
        .events
        .iter()
        .filter(|ev| matches!(ev, TurnEvent::ToolLine { .. }))
        .collect();
    assert_eq!(
        lines.len(),
        1,
        "a refused call must still produce exactly one §B6 line; got {lines:?}"
    );
    match lines[0] {
        TurnEvent::ToolLine { verb, state, .. } => {
            assert_eq!(verb, "read");
            assert!(
                matches!(state, ToolLineState::Failed(_)),
                "the line must read as a failure, not as a call that succeeded: {state:?}"
            );
        }
        other => panic!("not a tool line: {other:?}"),
    }
}

/// **A refused call is still a call the model made, and the refusal must be readable.**
///
/// Observed live: handed `[bash blocked] declined`, the model spent a turn concluding it had not
/// called bash at all — *"this must have been some automatic response or something odd with the
/// display"* — then apologised for a failure it could not describe. Two causes:
///
/// 1. The assistant turn carrying `tool_calls` was pushed **after** adjudication, so a refused
///    call produced a `tool` result with nothing that produced it.
/// 2. The reason was `format!("{reason:?}")` — the `Debug` rendering of an internal enum.
#[test]
fn a_refusal_tells_the_model_what_happened_and_whether_to_retry() {
    let mut e = engine();
    let mut driver = ScriptDriver::new(vec![
        step(
            // Path scoping is `Unavailable` here, so this is refused.
            ModelStep::one_call(
                ToolId::new("read"),
                marlowe_permission::Args::new().text("path", "/etc/passwd"),
            ),
            10,
        ),
        say("could not read it", 10),
    ]);
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

    let mut run = root(Budget::interactive());
    let mut state = SessionState::new(run.session, "Marlowe.");
    let mut prov = Provenance::new();
    let _ = e.run(&mut run, &mut state, &mut prov, &mut ports);

    // 1. The attempt is in the conversation, as an assistant turn with the call on it.
    let declared = state.volatile.iter().any(|b| {
        b.wire
            .as_ref()
            .is_some_and(|w| w.tool_calls.iter().any(|c| c.name == "read"))
    });
    assert!(
        declared,
        "a refused call must still be recorded as a call the model made, or the model cannot \
         tell it called anything: {:?}",
        state.volatile.iter().map(|b| (&b.source, &b.text)).collect::<Vec<_>>()
    );

    // 2. The refusal reads as English, not as a Debug dump.
    let refusal = state
        .volatile
        .iter()
        .find(|b| b.text.contains("blocked"))
        .map(|b| b.text.clone())
        .expect("the refusal reached the conversation");
    assert!(
        !refusal.contains("UndeclaredPath {") && !refusal.contains("detail:"),
        "the model was handed a Debug rendering of an internal enum: {refusal}"
    );
    assert!(
        refusal.contains("workspace"),
        "the refusal must say what the rule is: {refusal}"
    );
    assert!(
        refusal.to_lowercase().contains("retry"),
        "the refusal must say whether retrying can ever work — that is what the next action \
         depends on: {refusal}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────────────────
// A spawn is VISIBLE — the user sees it happen and sees what came back
// ─────────────────────────────────────────────────────────────────────────────────────────

/// **A spawn emitted no line at all, and the transcript could not be told apart from a lie.**
///
/// Watched live 2026-08-26 on `qwen3.5:9b`: the model said it had delegated, a child ran and
/// returned — the journal shows `run_spawned` and `run_completed` — and the screen showed **no
/// tool line and no result**. Every other tool goes through `prepare`, which emits
/// `TurnEvent::ToolLine`; a spawn is `ModelStep::Spawn`, loop control rather than a tool-host
/// call, so it took a path that emitted nothing.
///
/// From outside, "the model delegated and is summarising the child" and "the model claimed to
/// delegate and made the answer up" rendered **identically**. That is the one ambiguity a harness
/// must not leave, and it is the third instance of `Engine::spawn` sitting outside a path that
/// reports — B1 closed the run listing and the roster panel for the same reason.
#[test]
fn a_spawn_puts_a_line_on_the_screen_and_the_childs_result_in_it() {
    let mut e = engine();
    let mut driver = ScriptDriver::new(vec![
        step(
            ModelStep::Spawn(SpawnRequest {
                task: "summarise the run tool".into(),
                contract: OutputContract::new("findings", &["findings"]),
                orphan: OrphanPolicy::Terminate,
                share: BudgetShare::Standard,
                grant_tokens: None,
                tools: vec![],
                reads_untrusted: false,
                tools_declared: true,
                role: marlowe_loop::ModelRoute::Worker,
                disposition: marlowe_loop::Disposition::Work,
            }),
            100,
        ),
        // The child answers with a marker no summariser could invent, so "the result reached the
        // line" is a difference that was observed rather than assumed.
        say("PELICAN-4402 is what the child found", 100),
        say("parent done", 100),
    ]);
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

    let mut run = root(Budget::interactive());
    let mut state = SessionState::new(run.session, "Marlowe.");
    let mut prov = Provenance::new();
    let outcome = e.run(&mut run, &mut state, &mut prov, &mut ports);
    assert!(matches!(outcome, LoopOutcome::Completed(_)), "the spawn itself must succeed");

    let lines: Vec<_> = sink
        .events
        .iter()
        .filter_map(|ev| match ev {
            marlowe_loop::TurnEvent::ToolLine { verb, target, state, .. } if verb == "spawn" => {
                Some((target.clone(), state.clone()))
            }
            _ => None,
        })
        .collect();

    assert!(
        !lines.is_empty(),
        "a spawn emitted NO tool line — the user cannot tell delegation from a claim of it"
    );

    // Running first, so the line exists while the child is still working rather than appearing
    // only once it is over. A spawn blocks, so this is the only thing on screen for its duration.
    assert!(
        lines
            .iter()
            .any(|(_, s)| matches!(s, marlowe_loop::ToolLineState::Running { .. })),
        "no `Running` line: nothing marks the wait while the child works. Got {lines:?}"
    );

    // **The child's own result, not the model's account of it.** This is the assertion that would
    // have caught what was watched live: a parent claiming to quote a child while quoting nothing.
    let finished = lines
        .iter()
        .find(|(_, s)| !matches!(s, marlowe_loop::ToolLineState::Running { .. }))
        .expect("the line must resolve when the child returns, not stay Running forever");
    let rendered = format!("{:?}", finished.1);
    assert!(
        rendered.contains("PELICAN-4402"),
        "the child's result never reached the line, so the user still sees only the model's \
         summary of it. Got {rendered}"
    );

    // The name a person can say, the same one `/runs` and the window use — not a UUID.
    assert!(
        finished.0.contains('-') && !finished.0.contains("00000000"),
        "the line should carry the child's sayable name, got {:?}",
        finished.0
    );

    // **The child's prose must NOT be on the parent's screen.** It was: watched live, a child's
    // sentence appeared mid-stream in the parent's conversation, because `spawn` handed the child
    // the parent's sink. Section 10.2 says a subagent returns findings, not transcripts, and E4
    // forbids child-composed prose reaching a terminal.
    //
    // The marker is the control: the child definitely SAID it -- it is on the resolved line above,
    // which is the harness's own rendering of the returned result -- so its absence from the
    // streamed text is a difference that was observed, not a test passing on an empty sink.
    let streamed = sink.text();
    assert!(
        !streamed.contains("PELICAN-4402"),
        "the child streamed its prose onto the parent's surface. Got: {streamed:?}"
    );
}

/// **A spawn that does not say which tools the child gets is refused, not defaulted.**
///
/// ADR-057 amendment, 2026-08-26. The default was empty, and watched live that produced a child
/// with no tools asked to summarise a tool it could not look up — 12,332 tokens of reasoning and
/// no result. `tools` alone cannot tell an omitted `exposed_tools` from a deliberate empty one;
/// they arrive identically and mean opposite things.
#[test]
fn a_spawn_that_never_said_which_tools_the_child_gets_is_refused_by_name() {
    let mut e = engine();
    let mut driver = ScriptDriver::new(vec![
        step(
            ModelStep::Spawn(SpawnRequest {
                task: "do something".into(),
                contract: OutputContract::new("findings", &["findings"]),
                orphan: OrphanPolicy::Terminate,
                share: BudgetShare::Standard,
                grant_tokens: None,
                tools: vec![],
                reads_untrusted: false,
                // The subject of the test: the model never said.
                tools_declared: false,
                role: marlowe_loop::ModelRoute::Worker,
                disposition: marlowe_loop::Disposition::Work,
            }),
            100,
        ),
        say("parent done", 100),
    ]);
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

    let mut run = root(Budget::interactive());
    let mut state = SessionState::new(run.session, "Marlowe.");
    let mut prov = Provenance::new();
    let outcome = e.run(&mut run, &mut state, &mut prov, &mut ports);
    assert!(matches!(outcome, LoopOutcome::Completed(_)), "the parent survives the refusal");

    let blocks = e.assembler().assemble(&state).rendered();
    assert!(
        blocks.contains("exposed_tools"),
        "the refusal must NAME the missing parameter, or the model cannot fix it: {blocks}"
    );
    assert_eq!(
        run.spent.subagents, 0,
        "a refused spawn must not have started a child, but one was counted"
    );

    // **The control.** The same request WITH the declaration must spawn — otherwise this test
    // would pass on a build where `run` refuses everything.
    let mut e2 = engine();
    let mut driver2 = ScriptDriver::new(vec![
        step(
            ModelStep::Spawn(SpawnRequest {
                task: "do something".into(),
                contract: OutputContract::new("findings", &["findings"]),
                orphan: OrphanPolicy::Terminate,
                share: BudgetShare::Standard,
                grant_tokens: None,
                tools: vec![],
                reads_untrusted: false,
                tools_declared: true,
                role: marlowe_loop::ModelRoute::Worker,
                disposition: marlowe_loop::Disposition::Work,
            }),
            100,
        ),
        say("child done", 100),
        say("parent done", 100),
    ]);
    let mut sink2 = CollectingSink::default();
    let mut summarizer2 = EmptySummarizer;
    let mut tools2 = ScriptedTools::default();
    let mut approvals2 = FixedApprovals(true);
    let mut control2 = marlowe_loop::NoControl;
    let mut clock2 = FrozenClock(1_700_000_000_000);
    let mut recorder2 = MemoryRecorder::default();
    let mut ports2 = Ports {
        escalations: None,
        driver: &mut driver2,
        summarizer: &mut summarizer2,
        tools: &mut tools2,
        memory: None,
        approvals: &mut approvals2,
        sink: &mut sink2,
        control: &mut control2,
        clock: &mut clock2,
        recorder: &mut recorder2,
    };
    let mut run2 = root(Budget::interactive());
    let mut state2 = SessionState::new(run2.session, "Marlowe.");
    let mut prov2 = Provenance::new();
    let _ = e2.run(&mut run2, &mut state2, &mut prov2, &mut ports2);
    assert_eq!(
        run2.spent.subagents, 1,
        "an empty tool set that was DECLARED must still spawn — the refusal is about silence"
    );
}


// ─────────────────────────────────────────────────────────────────────────────────────────
// M3-DESIGN §1: a worker creates nothing, and the create grant IS holding `run`
// ─────────────────────────────────────────────────────────────────────────────────────────

/// **This failed at HEAD before M3 Session C, and it is SECURITY-AUDIT H2 applied to `run`
/// rather than a new finding.**
///
/// `ollama.rs` maps a `run` tool call to `ModelStep::Spawn` whatever the exposed set says, and
/// `ModelStep::Spawn` goes straight from the loop's match to `Engine::spawn` without ever
/// reaching `adjudicate` — which is where the exposure check lives. So a run that had never been
/// offered `run` could spawn simply by naming it. H2's own parenthetical, that *"`run` was
/// deliberately routed back through `ToolCall` for exactly this reason"*, is stale against that
/// mapping.
///
/// **Asserted on the COUNT, not on the message.** A message-only test is green on a build that
/// emits the refusal *and* spawns the child anyway.
///
/// *Mutation:* delete the `may_create_agents()` check at the top of `Engine::spawn` — the count
/// reads 1.
#[test]
fn a_run_without_the_create_grant_cannot_spawn() {
    // A worker's set: real tools, no `run`. `CapabilityProfile::new` would refuse `run` at this
    // level anyway, which is the other half of the same one definition.
    let worker = CapabilityProfile::new(
        marlowe_tools::ExposedSet::new(vec![ToolId::new("read"), ToolId::new("grep")]).unwrap(),
        marlowe_permission::EgressPolicy::DenyAll,
        marlowe_loop::InterruptPolicy::Unattended,
        marlowe_loop::ModelRoute::Worker,
        marlowe_loop::AgentLevel::Worker,
        false,
        false,
    )
    .unwrap();
    assert!(!worker.may_create_agents());

    let mut e = engine();
    let mut driver = ScriptDriver::new(vec![
        step(
            ModelStep::Spawn(SpawnRequest {
                task: "delegate this".into(),
                contract: OutputContract::new("findings", &["findings"]),
                orphan: OrphanPolicy::Terminate,
                share: BudgetShare::Standard,
                grant_tokens: None,
                tools: Vec::new(),
                reads_untrusted: false,
                tools_declared: true,
                role: marlowe_loop::ModelRoute::Worker,
                disposition: marlowe_loop::Disposition::Work,
            }),
            100,
        ),
        say("parent done", 100),
    ]);
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
        RunId::from_name("worker"),
        SessionId::from_name("worker-session"),
        worker,
        Budget::interactive(),
        OutputContract::answer(),
    );
    let mut state = SessionState::new(run.session, "Marlowe.");
    let mut prov = Provenance::new();
    let _ = e.run(&mut run, &mut state, &mut prov, &mut ports);

    assert_eq!(
        recorder.count(EventKind::RunSpawned),
        0,
        "a run that does not hold `run` created a child. §1: a worker creates nothing"
    );
    let rendered = e.assembler().assemble(&state).rendered();
    assert!(
        rendered.contains("cannot create agents"),
        "the refusal must name the create grant so the model can act on it:\n{rendered}"
    );

    // **The positive control, and it is the whole evidence.** The identical request from a run
    // that DOES hold `run` spawns. Without it the assertion above is green on a build that
    // cannot spawn at all.
    let mut e = engine();
    let mut driver = ScriptDriver::new(vec![
        step(
            ModelStep::Spawn(SpawnRequest {
                task: "delegate this".into(),
                contract: OutputContract::new("findings", &["findings"]),
                orphan: OrphanPolicy::Terminate,
                share: BudgetShare::Standard,
                grant_tokens: None,
                tools: Vec::new(),
                reads_untrusted: false,
                tools_declared: true,
                role: marlowe_loop::ModelRoute::Worker,
                disposition: marlowe_loop::Disposition::Work,
            }),
            100,
        ),
        say("child done", 100),
        say("parent done", 100),
    ]);
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
    let mut run = root(Budget::interactive());
    let mut state = SessionState::new(run.session, "Marlowe.");
    let mut prov = Provenance::new();
    let _ = e.run(&mut run, &mut state, &mut prov, &mut ports);
    assert_eq!(
        recorder.count(EventKind::RunSpawned),
        1,
        "Marlowe holds the create grant and must still be able to delegate"
    );
}
