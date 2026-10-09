//! **Egress approval is a REACHABILITY decision. It confers no authority on what comes back.**
//!
//! A human who approves `docs.example.com` has said *you may reach that host*. They have not
//! become the host, and they have not signed its bytes. CONTRACTS §3.3 binds a trust class to the
//! authority of the **origin**, so an approved host returns `UntrustedContent` exactly as an
//! unapproved one would. The wrong version is plausible enough to pass review as ergonomics —
//! *"the human was shown the host and approved it, therefore `UserAsserted`"* — which is the
//! human's authority laundering the page's, and it is what this file exists to make fail.
//!
//! # Why the invariant needed a test at all, given that it holds structurally
//!
//! It holds today because the two halves never meet: `trust_for_channel` takes only a `Channel`,
//! `marlowe-permission`'s egress module imports no `TrustClass`, and `read_ref` stamps its class
//! without consulting any policy. **Nothing checked that nothing wires them together.** An
//! invariant that holds by absence is one refactor from holding by nothing, and it reports the
//! same reading either way.
//!
//! # What moved under this, and why the class is not read off `web`
//!
//! ADR-042. `web` no longer returns page content — its result is a `DocumentRef`, a hash and
//! counts, stamped `AgentObserved` because no substring of the page is in it. The page re-enters
//! through exactly one door, `read(ref=…)`, which is where `UntrustedContent` is stamped. So the
//! class of *what an approved host returned* is decided on a **different tool call** from the one
//! egress adjudicates: `read` declares no `Url` parameter, so the adjudicator's egress branch
//! never runs on it. That separation is the structural guarantee; this test is what notices if it
//! ever closes.
//!
//! # The delta over what was already asserted
//!
//! `line_numbers.rs` asserts this class too — under `EgressPolicy::DenyAll`, where no grant
//! exists to launder anything. **This asserts it with a grant in hand**, which is the only
//! configuration in which the wrong version is even expressible.
//!
//! # Every control here, and what each one stops
//!
//! - **C1, the grant is live at the enforcement site.** The same `web` call is adjudicated under
//!   three policies and must produce three different outcomes. Without the middle arm, a build
//!   with the egress branch deleted — or a policy that admitted everything — reads identically.
//! - **C2, a fetch really happened.** The outcome is not `failed` and its body carries a `ref`.
//! - **C3, the page really carried the marker.** Asserted on the extracted document *before*
//!   anything that depends on it, so an empty page cannot make an absence assertion pass.
//! - **C4, the dereference really returned the page.** `read` did not fail and the marker is in
//!   what came back. Without this, a build whose `read_ref` returned an empty document at
//!   `UntrustedContent` would satisfy the class assertion and prove nothing.
//! - **C5, the negative control.** `web`'s own result is asserted `AgentObserved` in the same
//!   test. A build that "fixed" this by stamping `UntrustedContent` on everything fetch-derived
//!   would satisfy the headline assertion and fail here — and would also make every `web` call
//!   spend a quarantined child, which is layer 1's cost model destroyed.
//! - **C6, the two classes discriminate.** `blocks_composed_targets` is asserted to differ across
//!   the pair, on the same predicate the adjudicator enforces on, so the pair is shown to
//!   separate rather than to agree everywhere.
//! - **C7, one constant.** The host granted and the URL fetched come from the same place, or the
//!   two halves of the test drift apart in silence.
//!
//! # What this file CANNOT cover, stated rather than implied
//!
//! A wrong version that added an `EgressPolicy` field to `FileSystemTools` and read it inside
//! `read_ref` would need a new constructor, which this test would not call, so this test would
//! stay green. The structural fact that the class-stamping site has no access to grant state is
//! what makes the invariant hold; this guards the two places the policy **is** in scope — the
//! adjudicator, here, and the loop, in `marlowe-loop/tests/egress_grant.rs`, whose third test
//! additionally covers the version that quarantines everything *except* approved hosts. Neither
//! file can see the other's mutation; both are needed.
//!
//! Nothing here reaches the network. `web_outcome` is the shipped `web` minus the socket.

use std::fs;
use std::path::PathBuf;

use marlowe_contract::TrustClass;
use marlowe_exec::{corpus, FileSystemTools};
use marlowe_extract::store::DocumentStore;
use marlowe_loop::{ToolBody, ToolHost, ToolOutcome};
use marlowe_net::Fetched;
use marlowe_permission::scope::WorkspaceScope;
use marlowe_permission::{
    Adjudication, Adjudicator, Args, BlockReason, EgressPolicy, Outcome, Request, TaintSet, Tier,
};
use marlowe_tools::{builtin_registry, ExposedSet, HostPattern, ToolId, ToolRegistry, BUILTIN_TOOLS};

/// C7 — one decision, two constants derived from it. `HOST` is what a human approves; `URL` is
/// what gets fetched. The test asserts they agree before it asserts anything else.
const HOST: &str = "docs.example.com";
const URL: &str = "https://docs.example.com/p";

/// **Distinctive, not "some page text".** A generic body would let "the marker is absent" pass on
/// any build where the wording drifted, which is the vacuous-guard shape. This string appears
/// nowhere else in the workspace.
const MARKER: &str = "PAGE-BYTES-MARKER-c41d8e-authored-by-the-host-not-the-human";

fn page() -> Vec<u8> {
    format!(
        "<html><head><title>a document</title></head>\
         <body><h1>heading</h1><p>{MARKER}</p></body></html>"
    )
    .into_bytes()
}

fn fetched(bytes: Vec<u8>) -> Fetched {
    let n = bytes.len();
    Fetched {
        status: 200,
        content_type: Some("text/html; charset=utf-8".into()),
        bytes,
        final_url: URL.into(),
        redirect_to: None,
        wire_bytes: n,
        reused_connection: false,
    }
}

/// The real adjudicator and the real executor over one shared store, with the egress policy as a
/// **parameter** — which is the whole difference from `line_numbers.rs`'s fixture, where it is
/// hardcoded to `DenyAll`.
struct Fixture {
    root: PathBuf,
    registry: ToolRegistry,
    store: DocumentStore,
}

impl Fixture {
    fn new() -> Self {
        let root =
            std::env::temp_dir().join(format!("marlowe-egress-authority-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        Self { root, registry: builtin_registry().unwrap(), store: DocumentStore::new() }
    }

    /// Adjudicate for real, under the given policy. The decision comes from the permission layer,
    /// never from the test.
    fn adjudicate(&self, tool: &str, args: &Args, egress: &EgressPolicy) -> Adjudication {
        let exposed =
            ExposedSet::new(BUILTIN_TOOLS.iter().map(|t| ToolId::new(*t)).collect()).unwrap();
        let mut taint = TaintSet::new();
        for (name, _) in args.iter() {
            // `UserAsserted` throughout: the model did not compose these out of a page. The
            // question here is egress and trust class, not the (action, target) split.
            taint.insert(name.clone(), TrustClass::UserAsserted);
        }
        let mut adj = Adjudicator::new(WorkspaceScope::new().expect("verified platform"));
        adj.adjudicate(Request {
            manifest: self.registry.manifest(&ToolId::new(tool)).unwrap(),
            args,
            taint: &taint,
            exposed: &exposed,
            egress,
            workspace: &self.root,
            tier: Tier::Silent,
            novelty: None,
        })
    }

    fn tools(&self) -> FileSystemTools<WorkspaceScope> {
        FileSystemTools::new(WorkspaceScope::new().expect("verified platform"), &self.root)
            .with_store(self.store.clone())
    }

    /// Adjudicate, then execute — the handle comes from the permission layer, never from here.
    fn call(&self, tool: &str, args: Args, egress: &EgressPolicy) -> ToolOutcome {
        let adjudication = self.adjudicate(tool, &args, egress);
        self.tools().execute(&ToolId::new(tool), &args, &adjudication)
    }

    /// The adjudicated outcome for one call under one policy. Named so C1's three arms are
    /// visibly the same call with exactly one thing changed.
    fn egress_outcome(&self, args: &Args, egress: &EgressPolicy) -> Outcome {
        self.adjudicate("web", args, egress).decision.outcome
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn text(r: &ToolOutcome) -> String {
    match &r.body {
        ToolBody::Inline(s) => s.clone(),
        ToolBody::Reference { hash, bytes } => format!("<ref {hash} {bytes}>"),
    }
}

/// The hash **as the model reads it** — parsed out of the body `web` returned, not lifted from a
/// `DocumentRef` the test kept a copy of. Reading it from the same channel the model does is what
/// makes the dereference a continuation of the fetch rather than a second, unrelated setup.
fn ref_hash(body: &str) -> String {
    let (_, after) = body.rsplit_once("ref ").expect("`DocumentRef::render` emits `… ref <hash>`");
    after.split_whitespace().next().expect("a hash follows it").to_string()
}

/// A grant of `HOST`, in the shape ADR-032's approval flow would leave behind.
///
/// **Constructed, and no longer unreachable.** This doc said *"it cannot be earned: `EgressPolicy::
/// grant` has no production caller"*; since 2026-08-29 the loop records the grant on approval, so
/// this is the state a run is in after its first `yes` to that host. It is still written out here
/// rather than earned, because this crate has no loop to earn it in — and because the point of the
/// arm is the policy value, not how it was arrived at.
fn granted() -> EgressPolicy {
    EgressPolicy::AllowApproved { granted: vec![HostPattern::new(HOST)] }
}

// ══════════════════════════════════════════════════════════════════════════════════════════
// The property
// ══════════════════════════════════════════════════════════════════════════════════════════

#[test]
fn approving_a_host_does_not_raise_the_trust_class_of_what_it_returns() {
    assert!(URL.contains(HOST), "C7: the URL fetched and the host granted are one decision");
    let fx = Fixture::new();
    let args = Args::new().text("url", URL);

    // ── C1. THE GRANT IS LIVE AT THE ENFORCEMENT SITE ────────────────────────────────────
    //
    // Three policies, three outcomes, one call. This is what proves `EgressPolicy::grants` was
    // consulted and that the grant is what changed the answer — without the middle arm, a build
    // that admitted every host, or one with the egress branch deleted, reads exactly like this
    // one at the first arm alone.
    let with_grant = fx.egress_outcome(&args, &granted());
    assert!(
        !with_grant.is_blocked() && !with_grant.needs_approval(),
        "a granted host is reached without asking again: {with_grant:?}"
    );
    let ungranted = EgressPolicy::AllowApproved { granted: Vec::new() };
    let without_grant = fx.egress_outcome(&args, &ungranted);
    assert!(
        without_grant.needs_approval(),
        "an ungranted host under AllowApproved is UNASKED, not denied — and this is the arm that \
         makes the grant above load-bearing rather than decoration: {without_grant:?}"
    );
    let denied = fx.egress_outcome(&args, &EgressPolicy::DenyAll);
    assert!(
        matches!(
            &denied,
            Outcome::Blocked { reason: BlockReason::EgressNotAllowed { host } } if host == HOST
        ),
        "DenyAll is terminal, and it names the host it refused: {denied:?}"
    );

    // ── C3. THE PAGE REALLY CARRIED THE MARKER ───────────────────────────────────────────
    //
    // The premise, asserted before anything that depends on it. An empty page would make the
    // negative control below pass for entirely the wrong reason.
    let out = corpus::read(URL, fetched(page()));
    let document = out.document().expect("the page extracts").clone();
    assert!(document.text.contains(MARKER), "premise: the fetched page contains the marker");
    let chars = document.text.len();

    // ── the fetch, with the grant in hand ────────────────────────────────────────────────
    let tools = fx.tools();
    let fetch = tools.web_outcome(URL, 200, Some("text/html"), chars, 4_096, out);

    // C2 — a fetch really happened.
    assert!(!fetch.failed, "the fetch succeeded: {}", text(&fetch));
    let handed_back = text(&fetch);
    assert!(handed_back.contains("ref "), "and it handed back a reference: {handed_back}");

    // ── C5. THE NEGATIVE CONTROL ─────────────────────────────────────────────────────────
    //
    // `web`'s OWN result is `AgentObserved`, and the only thing licensing that is the absence of
    // any attacker-authored substring from it (ADR-042). A build that "fixed" the property below
    // by stamping `UntrustedContent` on everything fetch-derived would satisfy it and fail here.
    assert_eq!(
        fetch.trust,
        TrustClass::AgentObserved,
        "a reference is harness measurement, not page content: {handed_back}"
    );
    assert!(
        !handed_back.contains(MARKER),
        "…and the licence for that class is that no byte of the page is in it: {handed_back}"
    );

    // ── THE PROPERTY ─────────────────────────────────────────────────────────────────────
    //
    // Same host, same store, same approved grant — and the page comes back untrusted. The
    // approval bought reachability. It bought no authority.
    let hash = ref_hash(&handed_back);
    let read = fx.call("read", Args::new().text("ref", &hash), &granted());

    // C4 — the dereference really returned the page.
    assert!(!read.failed, "the dereference succeeded: {}", text(&read));
    assert!(
        text(&read).contains(MARKER),
        "premise: the page came back, so the class below is a class ON SOMETHING: {}",
        text(&read)
    );

    assert_eq!(
        read.trust,
        TrustClass::UntrustedContent,
        "APPROVAL IS REACHABILITY, NOT AUTHORITY. §3.3 binds a class to the authority of the \
         ORIGIN, and a human permitting a fetch has not become the origin. Anything above this \
         is the human's authority laundering the page's"
    );

    // ── C6. THE PAIR DISCRIMINATES ───────────────────────────────────────────────────────
    //
    // Asserted on `blocks_composed_targets` — the predicate the adjudicator enforces on, not a
    // restatement of the two classes above. If these ever agree, one of the assertions above is
    // passing for a reason that has nothing to do with what it names.
    assert!(
        marlowe_permission::blocks_composed_targets(read.trust),
        "the dereferenced page costs the run its composed targets"
    );
    assert!(
        !marlowe_permission::blocks_composed_targets(fetch.trust),
        "…and the reference does not, which is the whole of ADR-042's cost model"
    );
}
