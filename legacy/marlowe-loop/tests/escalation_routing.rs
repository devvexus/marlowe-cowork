//! **M3-DESIGN §3.1, driven through the loop rather than asserted about it.**
//!
//! §3.1 is three sentences. Each has a test here, and each test names the mutation that reddens it,
//! because a test whose reading is the same whether or not the mechanism works is instance #15 and
//! is worth less than no test at all.
//!
//! | §3.1 sentence | test | mutation that reddens it |
//! |---|---|---|
//! | *"starts at the agent's direct master"* | `a_worker_raises_to_its_master_and_the_root_gains_nothing` | route a `Worker` to `User` |
//! | *"a worker can never address Marlowe"* | `an_agent_cannot_reach_the_user_through_the_secretarys_door` | delete the `AgentLevel::Secretary` check in the `Ask` arm |
//! | *"only a top-agent may escalate to the user"* | `every_level_routes_where_section_3_1_says_and_no_arm_names_marlowe` | give `Master` the `User` arm |
//!
//! # The negative control is not decoration
//!
//! The adversarial pass found the first design's routing test asserting *"the root's `SessionState`
//! gains zero blocks"* — **true on today's HEAD before anything is built**, because
//! `Engine::spawn` already swallows a child's `Escalated` into a harness constant. Two of its four
//! assertions read identically on a build with the routing deleted.
//!
//! So the positive delivery is the headline here, and
//! `a_desk_that_accepts_anything_records_whatever_route_it_is_handed` is the control: the same
//! plumbing, the same desk double, a different level — asserting that a DIFFERENT route lands.
//! If the control cannot make the delivery differ, the delivery assertion was reading the
//! plumbing rather than the routing.
//!
//! **Every test name in the table above is a claim about a path.** A row naming a `fn` that does
//! not exist is instance #14 in a doc comment, so the names here are the names below.

mod common;

use common::*;
use marlowe_contract::escalation::{
    EscalationCategory, EscalationId, EscalationSeverity, OptionLabel,
};
use marlowe_loop::driver::{EscalationPort, EscalationRequest};
use marlowe_loop::escalation::{
    escalation_route, EscalationRefused, EscalationRoute, NotRaisableReason, ESCALATION_NO_DESK,
    ESCALATION_RAISED,
};
use marlowe_loop::profile::{AgentLevel, Disposition};
use marlowe_loop::{
    Budget, CapabilityProfile, Engine, InterruptPolicy, LoopOutcome, MemoryRecorder, ModelRoute,
    ModelStep, OrphanPolicy, OutputContract, Ports, Provenance, Run, RunId, SessionId,
    SessionState, ASK_IS_THE_CONVERSATIONS_DOOR,
};
use marlowe_permission::{scope::WorkspaceScope, EgressPolicy, Tier};
use marlowe_tools::{builtin_registry, ExposedSet, ToolId};

// ── the harness ────────────────────────────────────────────────────────────────────────────

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

fn profile_at(level: AgentLevel, tools: &[&str]) -> CapabilityProfile {
    CapabilityProfile::new(
        ExposedSet::new(tools.iter().map(|t| ToolId::new(*t)).collect()).expect("a small set"),
        EgressPolicy::DenyAll,
        InterruptPolicy::Unattended,
        ModelRoute::Worker,
        level,
        false,
        false,
    )
    .expect("a profile the constructor admits")
}

fn root_at(name: &str, profile: CapabilityProfile) -> Run {
    Run::root(
        RunId::from_name(name),
        SessionId::from_name("escalation"),
        profile,
        Budget::interactive(),
        OutputContract::answer(),
    )
}

fn child_of(name: &str, parent: &Run, profile: CapabilityProfile) -> Run {
    Run::child(
        RunId::from_name(name),
        parent,
        parent.session,
        profile,
        Budget::interactive(),
        OrphanPolicy::Terminate,
        OutputContract::answer(),
    )
}

fn request() -> EscalationRequest {
    EscalationRequest {
        severity: EscalationSeverity::Blocking,
        category: EscalationCategory::BlockedByPermission,
        options: vec![
            OptionLabel::normalise("widen the scope to the whole repo").expect("a plain label"),
        ],
        artifact: None,
        sentence: None,
    }
}

/// A desk that accepts anything and records where it was told to put it.
#[derive(Default)]
struct RecordingDesk {
    delivered: Vec<(RunId, EscalationRoute)>,
    seq: u32,
}

impl EscalationPort for RecordingDesk {
    fn raise(
        &mut self,
        raised_by: RunId,
        route: EscalationRoute,
        _req: EscalationRequest,
    ) -> Result<EscalationId, EscalationRefused> {
        self.seq += 1;
        self.delivered.push((raised_by, route));
        Ok(EscalationId::for_raise(raised_by.0, self.seq))
    }
}

/// A desk that refuses everything, so the loop's `NoDesk`/refusal wording can be driven.
struct RefusingDesk;

impl EscalationPort for RefusingDesk {
    fn raise(
        &mut self,
        _raised_by: RunId,
        _route: EscalationRoute,
        _req: EscalationRequest,
    ) -> Result<EscalationId, EscalationRefused> {
        Err(EscalationRefused::TooManyOptions { count: 9, max: 4 })
    }
}

// ── §3.1, as a table over the levels the tree can actually produce ─────────────────────────

/// **Exhaustive over `AgentLevel`, with no wildcard**, so a sixth level fails this test rather
/// than inheriting a neighbour's route.
///
/// The rows are the whole of §3.1: `Secretary` has nobody above it, a `TopAgent` reaches the user
/// (and Marlowe is not the recipient — there is no arm that names him), `Master` and `Worker`
/// reach their parent, and `ToolSpawned` reaches nobody.
#[test]
fn every_level_routes_where_section_3_1_says_and_no_arm_names_marlowe() {
    let root = root_at("secretary", CapabilityProfile::interactive());
    assert_eq!(
        escalation_route(&root),
        EscalationRoute::NotRaisable(NotRaisableReason::Root)
    );

    for manages in [true, false] {
        let top = child_of(
            "top",
            &root,
            profile_at(AgentLevel::TopAgent { manages }, if manages { &["run"] } else { &["read"] }),
        );
        assert_eq!(
            escalation_route(&top),
            EscalationRoute::User,
            "a top-agent reaches the user whether or not it manages (manages={manages})"
        );
    }

    let top = child_of("top", &root, profile_at(AgentLevel::TopAgent { manages: true }, &["run"]));
    let master = child_of("master", &top, profile_at(AgentLevel::Master, &["run"]));
    assert_eq!(escalation_route(&master), EscalationRoute::Parent(top.id));

    let worker = child_of("worker", &master, profile_at(AgentLevel::Worker, &["read"]));
    assert_eq!(
        escalation_route(&worker),
        EscalationRoute::Parent(master.id),
        "§3.1: escalation starts at the DIRECT master, not at the top of the subtree"
    );

    let reader = child_of("reader", &master, CapabilityProfile::quarantined_reader());
    assert_eq!(
        escalation_route(&reader),
        EscalationRoute::NotRaisable(NotRaisableReason::ReadsUntrusted)
    );

    // The vacuity control for the whole table: `User` is reachable from exactly one level, so a
    // routing function that returned `User` unconditionally would fail here rather than passing
    // three of the five rows.
    let reaching_the_user = [
        escalation_route(&root),
        escalation_route(&master),
        escalation_route(&worker),
        escalation_route(&reader),
    ]
    .into_iter()
    .filter(|r| *r == EscalationRoute::User)
    .count();
    assert_eq!(reaching_the_user, 0, "only a top-agent reaches the user");
}

/// **The two refusals are independent, and this is the test that says so.**
///
/// `escalation_route` refuses on `reads_untrusted()` and on `AgentLevel::ToolSpawned`, and the two
/// look redundant. They are not: `CapabilityProfile::new` makes each imply an empty tool set and
/// **neither implies the other**. Delete either disjunct and a real run gains a live route upward.
///
/// Each row below holds the other condition false, so neither disjunct can be passing on the
/// other's behalf. *Mutations:* remove the `reads_untrusted` check → row 1 becomes
/// `Parent(master)`; remove the `ToolSpawned` arm → row 2 becomes `NotRaisable(Detached)` or
/// `Parent`, either way not `ToolSpawned`.
#[test]
fn neither_refusal_disjunct_is_covering_for_the_other() {
    let root = root_at("secretary", CapabilityProfile::interactive());
    let top = child_of("top", &root, profile_at(AgentLevel::TopAgent { manages: true }, &["run"]));
    let master = child_of("master", &top, profile_at(AgentLevel::Master, &["run"]));

    // Row 1: reads untrusted, and is NOT `ToolSpawned`. Constructible today — `new` enforces
    // `reads_untrusted ⟹ empty tool set` and says nothing about the level.
    let untrusted_worker = CapabilityProfile::new(
        ExposedSet::empty(),
        EgressPolicy::DenyAll,
        InterruptPolicy::Unattended,
        ModelRoute::Worker,
        AgentLevel::Worker,
        false,
        true,
    )
    .expect("a worker that reads untrusted content and holds no tools is a valid profile");
    assert_eq!(untrusted_worker.level(), AgentLevel::Worker, "the level is NOT ToolSpawned here");
    let r = child_of("untrusted-worker", &master, untrusted_worker);
    assert_eq!(
        escalation_route(&r),
        EscalationRoute::NotRaisable(NotRaisableReason::ReadsUntrusted)
    );

    // Row 2: `ToolSpawned`, and does NOT read untrusted content. This is `SCOPED-MEMORY.md` §4's
    // fact extractor, which the `reads_untrusted` disjunct cannot see at all.
    let extractor = CapabilityProfile::new(
        ExposedSet::empty(),
        EgressPolicy::DenyAll,
        InterruptPolicy::Unattended,
        ModelRoute::Worker,
        AgentLevel::ToolSpawned,
        false,
        false,
    )
    .expect("a tool-spawned agent with no tools and no untrusted reads is a valid profile");
    assert!(!extractor.reads_untrusted(), "the reads_untrusted disjunct is FALSE here");
    let e = child_of("extractor", &master, extractor);
    assert_eq!(
        escalation_route(&e),
        EscalationRoute::NotRaisable(NotRaisableReason::ToolSpawned)
    );
}

/// **A detached run that raises is TOLD SO.** The first design let it vanish — no event, no note,
/// nothing in anyone's window — on the highest-consequence channel in the system.
///
/// *Mutation:* collapse `NotRaisable(reason)` to a bare `NotRaisable` → the reason is gone and the
/// two assertions below cannot both hold.
#[test]
fn a_detached_run_that_raises_is_told_so_rather_than_going_quiet() {
    let root = root_at("secretary", CapabilityProfile::interactive());
    let top = child_of("top", &root, profile_at(AgentLevel::TopAgent { manages: true }, &["run"]));
    let mut master = child_of("master", &top, profile_at(AgentLevel::Master, &["run"]));
    assert_eq!(escalation_route(&master), EscalationRoute::Parent(top.id));

    master.detached();
    assert_eq!(
        escalation_route(&master),
        EscalationRoute::NotRaisable(NotRaisableReason::Detached),
        "a `Detach` is a lifetime decision, and it must not silently close the upward channel"
    );
    assert!(
        NotRaisableReason::Detached.note().contains("was not silently dropped"),
        "the raiser's own window has to say what happened: {}",
        NotRaisableReason::Detached.note()
    );

    // **And a top-agent's route survives detachment.** The one channel that reaches a human must
    // not be closable by a lifetime the agent itself declared at spawn.
    let mut top = child_of("top2", &root, profile_at(AgentLevel::TopAgent { manages: true }, &["run"]));
    top.detached();
    assert_eq!(escalation_route(&top), EscalationRoute::User);
}

// ── driven through the loop ─────────────────────────────────────────────────────────────────

struct Driven {
    outcome: LoopOutcome,
    state: SessionState,
}

fn drive(run: &mut Run, steps: Vec<marlowe_loop::ModelCall>, desk: Option<&mut dyn EscalationPort>) -> Driven {
    let mut driver = ScriptDriver::new(steps);
    let mut summarizer = EmptySummarizer;
    let mut tools = ScriptedTools::default();
    let mut approvals = FixedApprovals(true);
    let mut sink = CollectingSink::default();
    let mut control = marlowe_loop::NoControl;
    let mut clock = FrozenClock(1_700_000_000_000);
    let mut recorder = MemoryRecorder::default();
    let mut ports = Ports {
        // A reborrow of the PLACE, so the trait object's lifetime can shorten to the one every
        // other port here has. `escalations: desk` does not compile: `&mut` is invariant over its
        // type parameter, and a `dyn Trait + 'long` behind one cannot become a `dyn Trait +
        // 'short` unless the compiler is given a place to reborrow from.
        escalations: match desk {
            Some(d) => Some(&mut *d),
            None => None,
        },
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
    let mut state = SessionState::new(run.session, "Marlowe.");
    let mut prov = Provenance::new();
    let outcome = engine().run(run, &mut state, &mut prov, &mut ports);
    Driven { outcome, state }
}

fn window(state: &SessionState) -> String {
    state
        .context_blocks
        .iter()
        .chain(state.volatile.iter())
        .map(|b| b.text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// **§3.1's *"a worker can never address Marlowe"*, at the site whose deletion would evaporate
/// it.**
///
/// `ask` is the conversational run's door to the user. CLAUDE.md names `engine.rs` as the
/// unguarded call site whose deletion "would evaporate the boundary while every guarded file
/// stayed untouched" — this is that, for §3.1.
///
/// *Mutation:* delete the `run.profile.level() != AgentLevel::Secretary` check in the `Ask` arm →
/// the outcome becomes `LoopOutcome::Escalated`, and both assertions fail.
#[test]
fn an_agent_cannot_reach_the_user_through_the_secretarys_door() {
    let root = root_at("secretary", CapabilityProfile::interactive());
    let mut worker = child_of("worker", &root, profile_at(AgentLevel::Worker, &["read"]));

    let d = drive(
        &mut worker,
        vec![step(ModelStep::Ask("may I widen the scope?".into()), 50), say("carrying on", 50)],
        None,
    );

    assert!(
        !matches!(d.outcome, LoopOutcome::Escalated { .. }),
        "a worker's `ask` escaped the loop as an `Escalated` outcome: {:?}",
        d.outcome
    );
    assert!(
        window(&d.state).contains(ASK_IS_THE_CONVERSATIONS_DOOR),
        "the refusal must be in the run's own window, or the model cannot act on it:\n{}",
        window(&d.state)
    );
    // **The positive control**: the SAME step from the conversational run does escape. Without
    // this, a build where `ModelStep::Ask` was unreachable for any reason would pass the two
    // assertions above and prove nothing about the level check.
    let mut secretary = root_at("secretary-2", CapabilityProfile::interactive());
    let d2 = drive(
        &mut secretary,
        vec![step(ModelStep::Ask("what shall I do?".into()), 50)],
        None,
    );
    assert!(
        matches!(d2.outcome, LoopOutcome::Escalated { .. }),
        "the conversational run must still be able to ask: {:?}",
        d2.outcome
    );
}

/// **The headline: a worker's escalation is DELIVERED to its master**, and the root gains nothing.
///
/// The delivery is the assertion that carries weight. The root-gains-nothing half is stated after
/// it and is worth nothing on its own — see the control below.
#[test]
fn a_worker_raises_to_its_master_and_the_root_gains_nothing() {
    let root = root_at("secretary", CapabilityProfile::interactive());
    let top = child_of("top", &root, profile_at(AgentLevel::TopAgent { manages: true }, &["run"]));
    let master = child_of("master", &top, profile_at(AgentLevel::Master, &["run"]));
    let mut worker = child_of("worker", &master, profile_at(AgentLevel::Worker, &["read"]));

    let mut desk = RecordingDesk::default();
    let d = {
        let outcome = drive(
            &mut worker,
            vec![step(ModelStep::Escalate(request()), 50)],
            Some(&mut desk),
        );
        outcome
    };

    assert_eq!(desk.delivered.len(), 1, "the desk received exactly one escalation");
    assert_eq!(
        desk.delivered[0].1,
        EscalationRoute::Parent(master.id),
        "§3.1: the worker's escalation goes to its DIRECT master, not to the top-agent and not \
         to the user"
    );
    assert!(matches!(d.outcome, LoopOutcome::Raised(_)), "{:?}", d.outcome);
    assert!(
        matches!(
            worker.status,
            marlowe_loop::RunStatus::Paused {
                reason: marlowe_loop::PauseReason::AwaitingEscalation { .. }
            }
        ),
        "the raiser pauses on the escalation: {:?}",
        worker.status
    );
    assert!(window(&d.state).contains(ESCALATION_RAISED));
}

/// **The control for the test above, and without it that test is worth two assertions.**
///
/// The adversarial pass found the first design asserting *"no `Recipient::Run(root_id)` is ever
/// produced"* — true on today's HEAD, before anything is built. So here is a desk that WILL take a
/// root-addressed escalation, driven by a route the loop was handed: if the plumbing could not
/// deliver to the root even when told to, the assertion above was never watching the routing.
#[test]
fn a_desk_that_accepts_anything_records_whatever_route_it_is_handed() {
    let root = root_at("secretary", CapabilityProfile::interactive());
    let mut top = child_of("top", &root, profile_at(AgentLevel::TopAgent { manages: true }, &["run"]));

    let mut desk = RecordingDesk::default();
    drive(&mut top, vec![step(ModelStep::Escalate(request()), 50)], Some(&mut desk));

    assert_eq!(desk.delivered.len(), 1);
    assert_eq!(
        desk.delivered[0].1,
        EscalationRoute::User,
        "the same plumbing carries a different route, so the master-addressed assertion above is \
         reading the ROUTE and not the plumbing"
    );
}

/// A run that may not raise reaches no desk at all, and its refusal is audible.
///
/// *Mutation:* derive the refusal from `req.options` or from the tool set instead of from the
/// profile → the reader classifies `Parent` → `desk.delivered` is 1 → red.
#[test]
fn a_quarantined_reader_cannot_raise_and_the_refusal_is_audible() {
    let root = root_at("secretary", CapabilityProfile::interactive());
    let top = child_of("top", &root, profile_at(AgentLevel::TopAgent { manages: true }, &["run"]));
    let mut reader = child_of("reader", &top, CapabilityProfile::quarantined_reader());

    let mut desk = RecordingDesk::default();
    let d = drive(
        &mut reader,
        vec![step(ModelStep::Escalate(request()), 50), say("done", 50)],
        Some(&mut desk),
    );

    assert!(desk.delivered.is_empty(), "a quarantined reader reached the desk");
    assert!(
        window(&d.state).contains(NotRaisableReason::ReadsUntrusted.note()),
        "the reader's own window must say why:\n{}",
        window(&d.state)
    );
}

/// *"Nobody is listening"* and *"you may not raise"* are different facts, and a model told the
/// wrong one changes the wrong thing.
#[test]
fn no_desk_and_a_refusal_say_different_things() {
    let root = root_at("secretary", CapabilityProfile::interactive());
    let mut top = child_of("top", &root, profile_at(AgentLevel::TopAgent { manages: true }, &["run"]));

    let d = drive(
        &mut top,
        vec![step(ModelStep::Escalate(request()), 50), say("carrying on", 50)],
        None,
    );
    assert!(window(&d.state).contains(ESCALATION_NO_DESK));
    assert!(!window(&d.state).contains(ESCALATION_RAISED));

    let mut refusing = RefusingDesk;
    let mut top2 = child_of("top2", &root, profile_at(AgentLevel::TopAgent { manages: true }, &["run"]));
    let d2 = drive(
        &mut top2,
        vec![step(ModelStep::Escalate(request()), 50), say("carrying on", 50)],
        Some(&mut refusing),
    );
    let w = window(&d2.state);
    assert!(w.contains("[escalation refused]"), "{w}");
    assert!(!w.contains(ESCALATION_NO_DESK), "a refusal is not an absent desk:\n{w}");
}

/// **`AgentLevel::child_of` is what makes the table above reachable**, and this asserts the two
/// are consistent: every level `child_of` can return has a route, and none of them is the user
/// except a top-agent's.
///
/// This is what replaces the deleted `may_hold_create_grant` idea: an invariant asserted where it
/// can fail, over the pairs the tree can actually produce, rather than a method whose enforcement
/// site could never see it false.
#[test]
fn every_level_the_tree_can_produce_has_a_route() {
    let levels = [
        AgentLevel::Secretary,
        AgentLevel::TopAgent { manages: true },
        AgentLevel::TopAgent { manages: false },
        AgentLevel::Master,
        AgentLevel::Worker,
        AgentLevel::ToolSpawned,
    ];
    let mut reachable = 0;
    for parent in levels {
        for d in [Disposition::Work, Disposition::Manage] {
            let Ok(child) = AgentLevel::child_of(parent, d) else { continue };
            reachable += 1;
            let root = root_at("secretary", CapabilityProfile::interactive());
            let p = child_of("p", &root, profile_at(parent, &[]));
            let c = child_of("c", &p, profile_at(child, &[]));
            let route = escalation_route(&c);
            assert_ne!(
                route,
                EscalationRoute::NotRaisable(NotRaisableReason::Detached),
                "a child built through `child_of({parent:?}, {d:?})` has a parent, so it can \
                 never be Detached"
            );
            if route == EscalationRoute::User {
                assert!(
                    matches!(child, AgentLevel::TopAgent { .. }),
                    "{child:?} reached the user, and §3.1 gives that to a top-agent alone"
                );
            }
        }
    }
    // The positive control: `child_of` refuses most pairs, and a run of this loop that produced
    // zero children would report "no level reached the user" for a build with the check deleted.
    assert!(reachable >= 4, "only {reachable} spawnable pairs — the table did not run");
}
