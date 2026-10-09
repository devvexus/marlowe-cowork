//! **M3-DESIGN §3.2, and §3.1's *"approved at each level"*, at the desk.**
//!
//! > *"He cannot read it, cannot query it, cannot summarise it. A window opens: the user and the
//! > top-agent, directly. Marlowe is not in the room."*
//!
//! # What "cannot read it" is a fact about
//!
//! Not about a method name, and not about Marlowe's profile happening to expose no tool that calls
//! one — a registry is configuration, and an MCP descriptor, a skill or a debugging path could all
//! reach a method that existed.
//!
//! **The loop reaches the desk only as `dyn marlowe_loop::EscalationPort`**, whose one method takes
//! a request and returns an id. `marlowe-loop` cannot name `marlowe-daemon` at all, so this is the
//! dependency graph rather than a convention. [`the_port_the_loop_holds_returns_only_an_id`] is that
//! claim, mechanically.
//!
//! # The distinguishing assertion
//!
//! *"The notification arrived without the body"* and *"nothing arrived"* must not read the same
//! way, so the marker's absence is asserted **beside** the notice's presence, never instead of it.

use marlowe_contract::escalation::{
    EscalationCategory, EscalationSeverity, OptionLabel, ValidatedSentence,
};
use marlowe_daemon::escalation::{AdvanceError, EscalationDesk, Recipient};
use marlowe_loop::escalation::{EscalationRoute, NotRaisableReason};
use marlowe_loop::{EscalationPort, EscalationRequest, RunId};
use marlowe_view::escalation::{ArtifactPath, Choice, SourceEvidence, TerminationCost};
use marlowe_view::notice::{Notice, RenderContext};

/// A token that could only have come from the escalation's content.
const BODY_MARKER: &str = "ESCALATION-BODY-9f31";

fn request() -> EscalationRequest {
    EscalationRequest {
        severity: EscalationSeverity::Critical,
        category: EscalationCategory::SuspectedInjection,
        options: vec![
            OptionLabel::normalise(&format!("keep going: {BODY_MARKER}")).expect("a label"),
        ],
        artifact: None,
        sentence: Some(
            ValidatedSentence::normalise(&format!("the page said {BODY_MARKER}"))
                .expect("a sentence"),
        ),
    }
}

fn cost() -> TerminationCost {
    TerminationCost {
        runs: 2,
        age_ms: 5_000,
        spend_micros_usd: 100,
        artifacts: 0,
        irreversible: Vec::new(),
        survivors: Vec::new(),
    }
}

/// §3.2, on the bytes Marlowe would see.
#[test]
fn the_notification_carries_no_word_of_the_escalations_content() {
    let mut desk = EscalationDesk::new();
    let worker = RunId::from_name("worker");
    let master = RunId::from_name("master");
    let id = desk
        .raise(worker, EscalationRoute::Parent(master), request())
        .expect("a raisable route is accepted");

    let notice = desk.secretary_notice(id).expect("the escalation is pending");

    // **Present**, so "the notification arrived without the body" is distinguished from "nothing
    // arrived". Without this the absence assertion below is green on a desk that returns `None`.
    let ctx = RenderContext { commands: &[], keys: &[], control: None };
    let lines = notice.render(&ctx);
    assert_eq!(lines.len(), 1, "one sentence, §3.2: {lines:?}");
    assert!(lines[0].contains("critical"), "the severity is what Marlowe is told: {lines:?}");

    // **Absent**, and asserted over the whole rendered notice and its Debug form — the Debug form
    // because a field carrying the body would leak through any log line that printed the value.
    let rendered = lines.join("\n");
    assert!(!rendered.contains(BODY_MARKER), "the body reached the notification: {rendered}");
    assert!(
        !format!("{notice:?}").contains(BODY_MARKER),
        "the body is in the notice's fields even though it is not in its prose: {notice:?}"
    );

    // The name is harness-derived. A model-chosen display name here could read `Marlowe`, `SYSTEM`,
    // or the label of the row beside it.
    assert!(
        rendered.contains(&marlowe_loop::run::sayable(&worker.to_string())),
        "the raiser is named by `sayable`, the single definition of how a run is printed: \
         {rendered}"
    );

    // And the control: the marker IS reachable on the surface's side, so its absence above is a
    // property of `secretary_notice` rather than of the marker never having been stored.
    let view = desk
        .view_for(id, cost(), SourceEvidence::NoExternalSources, |h| {
            ArtifactPath(format!("artifacts/{}", h.as_str()))
        })
        .expect("the escalation is pending")
        .expect("one option is inside the ceiling");
    assert!(
        view.options()[0].label.as_str().contains(BODY_MARKER),
        "the option list never held the marker, so the absence assertions above prove nothing"
    );
    assert!(
        view.sentence.as_ref().is_some_and(|s| s.as_str().contains(BODY_MARKER)),
        "the sentence never reached the view, so `EscalationRequest::sentence` has no reader and          is a declaration (instance #16)"
    );
}

/// `Escalation::artifact` has a reader, and it is `view_for`'s `path_of`. **A hash-only handle in,
/// a path out** -- no title, no summary, no media label a producer chose.
#[test]
fn the_stored_artifact_handle_is_what_the_path_is_composed_from() {
    let mut desk = EscalationDesk::new();
    let handle = "0123456789abcdef0123456789abcdef";
    let mut req = request();
    req.artifact = Some(
        marlowe_contract::escalation::ArtifactHandle::parse(handle).expect("32 hex characters"),
    );
    let id = desk
        .raise(RunId::from_name("w"), EscalationRoute::User, req)
        .expect("a raisable route");
    let view = desk
        .view_for(id, cost(), SourceEvidence::NoExternalSources, |h| {
            ArtifactPath(format!("artifacts/{}", h.as_str()))
        })
        .expect("pending")
        .expect("one option");
    assert_eq!(view.artifact, Some(ArtifactPath(format!("artifacts/{handle}"))));

    // The control: with no handle stored, the closure is never called and the row is absent --
    // so the assertion above is reading the STORED handle rather than the closure's constant.
    let id2 = desk.raise(RunId::from_name("w2"), EscalationRoute::User, request()).expect("raised");
    let view2 = desk
        .view_for(id2, cost(), SourceEvidence::NoExternalSources, |_| {
            ArtifactPath("artifacts/should-not-appear".into())
        })
        .expect("pending")
        .expect("one option");
    assert_eq!(view2.artifact, None);
}

/// **The structural claim, mechanically.** The loop holds a `dyn EscalationPort`, and that trait's
/// only method returns an `EscalationId`.
///
/// A `body(&self, id) -> Option<&str>` on the desk would not break this — but it would not be
/// reachable from a loop either, because the loop never holds an `EscalationDesk`. That is the
/// containment: it cannot be reached by adding a caller, only by changing the trait, which is a
/// change to a §13-guarded file.
#[test]
fn the_port_the_loop_holds_returns_only_an_id() {
    let mut desk = EscalationDesk::new();
    let port: &mut dyn EscalationPort = &mut desk;
    let id = port
        .raise(RunId::from_name("w"), EscalationRoute::User, request())
        .expect("a raisable route");
    // The whole surface of the type the loop can see, exercised. There is nothing else to call.
    let _: marlowe_contract::EscalationId = id;
}

/// §3.1: *"approved at each level"*, and the authority for "is this yours" is the desk.
///
/// **This is ADR-036 §5's authority rule at a saturated floor.** Every value inside an escalation
/// is `UntrustedContent`, so ADR-023's floor reads "blocked" for every pending id and
/// discriminates none of them. What survives saturation is who addressed it here.
///
/// *Mutation:* drop the `Pending::at` check → the sibling's advance succeeds → red.
#[test]
fn only_the_run_an_escalation_is_addressed_to_can_advance_it() {
    let mut desk = EscalationDesk::new();
    let worker = RunId::from_name("worker");
    let master = RunId::from_name("master");
    let top = RunId::from_name("top");
    let sibling = RunId::from_name("sibling");

    let id = desk.raise(worker, EscalationRoute::Parent(master), request()).expect("raised");
    assert_eq!(desk.addressed_to(id), Some(Recipient::Run(master)));

    assert_eq!(
        desk.advance(id, sibling, EscalationRoute::Parent(top)),
        Err(AdvanceError::NotYours),
        "a run that names an id it was never handed must not be able to pull it out of a \
         sibling's chain"
    );
    assert_eq!(
        desk.addressed_to(id),
        Some(Recipient::Run(master)),
        "the refused advance moved it anyway"
    );

    // The positive control: the same call from the right run works, so `NotYours` is about the
    // authority and not about `advance` being broken.
    assert_eq!(desk.advance(id, master, EscalationRoute::Parent(top)), Ok(Recipient::Run(top)));
    assert_eq!(desk.advance(id, top, EscalationRoute::User), Ok(Recipient::User));
    assert_eq!(
        desk.advance(id, top, EscalationRoute::User),
        Err(AdvanceError::AlreadyWithTheUser)
    );
}

/// The desk re-reads the route even though its one production caller has already checked it.
///
/// **Not redundant**: `EscalationDesk` is a `pub` type in another crate, and a second caller that
/// skipped the check would otherwise deliver an escalation the tree refuses.
#[test]
fn a_desk_refuses_a_route_the_tree_refuses() {
    let mut desk = EscalationDesk::new();
    let err = desk
        .raise(
            RunId::from_name("reader"),
            EscalationRoute::NotRaisable(NotRaisableReason::ReadsUntrusted),
            request(),
        )
        .expect_err("a quarantined reader's route is not deliverable");
    assert!(matches!(
        err,
        marlowe_loop::EscalationRefused::NotRaisable(NotRaisableReason::ReadsUntrusted)
    ));
    assert!(desk.pending_ids().is_empty());
}

/// Resolving removes it, and the resolution names the raiser rather than carrying its words.
#[test]
fn resolving_returns_a_choice_and_not_a_body() {
    let mut desk = EscalationDesk::new();
    let worker = RunId::from_name("worker");
    let id = desk
        .raise(worker, EscalationRoute::Parent(RunId::from_name("m")), request())
        .expect("raised");
    let r = desk.resolve(id, Choice::Terminate).expect("pending");
    assert_eq!(r.raised_by, worker);
    assert_eq!(r.choice, Choice::Terminate);
    assert!(!format!("{r:?}").contains(BODY_MARKER));
    assert!(desk.pending_ids().is_empty());
    assert!(desk.secretary_notice(id).is_none(), "a resolved escalation is no longer pending");
}

/// Two raises from the same run get different ids, and an id is reproducible from what the journal
/// records — which is the precondition for `from_journal`, which this session did not build.
#[test]
fn ids_are_distinct_and_derived_rather_than_drawn() {
    let mut desk = EscalationDesk::new();
    let w = RunId::from_name("worker");
    let m = RunId::from_name("m");
    let a = desk.raise(w, EscalationRoute::Parent(m), request()).expect("raised");
    let b = desk.raise(w, EscalationRoute::Parent(m), request()).expect("raised");
    assert_ne!(a, b);
    assert_eq!(desk.pending_ids().len(), 2);
    assert_eq!(a, marlowe_contract::EscalationId::for_raise(w.0, 1));
    assert_eq!(b, marlowe_contract::EscalationId::for_raise(w.0, 2));
}
