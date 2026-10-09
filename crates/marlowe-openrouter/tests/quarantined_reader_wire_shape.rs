//! **The request layer 1 actually sends, for both providers, taken from a real `condense_batch`.**
//!
//! # What this is about
//!
//! On 2026-08-22 every `read` of a fetched document returned *"the content could not be condensed
//! within the contract"* on the hosted path. The journal names the cause and it is not the
//! contract — four `run_failed` records, one per spawned reader, each an **HTTP 400** from
//! upstream before the child's first token, with a 50,000-token budget untouched.
//!
//! The child's request was malformed in two ways, and **both were produced by code that is right
//! at its own site**:
//!
//! | In the request | Produced by | Why it looks correct where it is written |
//! |---|---|---|
//! | `"tools": []` | `CapabilityProfile::quarantined_reader`'s `ExposedSet::empty()` | It is layer 1. A reader that can act cannot be constructed |
//! | `role: "tool"` with no `tool_call_id` | `condense_batch` pushing pages as `SourceKind::ToolResults` | In the *parent's* conversation they are exactly that |
//!
//! Neither is a capability question. `[]` and an absent `tools` key declare the same thing —
//! nothing — and only one of them is valid in the dialect. The fix removes no isolation: see
//! `the_quarantined_reader_is_still_offered_no_tool_by_any_spelling`, which asserts the child is
//! offered nothing *and* pairs it with the parent in the same run being offered everything.
//!
//! # Why the assertions are made here and not on a helper
//!
//! Every view below comes out of a **real `Engine::run`** that really spawned a real quarantined
//! child, and is handed to the **real `request_body`** of each adapter. A test that built a
//! plausible-looking `ContextView` by hand would have been green on the day this shipped: the
//! shape only goes wrong because of how `condense_batch` fills the child's window, which is the
//! one part a hand-built fixture replaces.
//!
//! Each case carries a control that fails if the mechanism is absent — the payload really did
//! reach the child, the parent really was offered tools, a genuine tool reply really did keep its
//! role. A containment test that passes because nothing was fetched is the shape being avoided.

use std::sync::{Arc, Mutex};

use marlowe_contract::TrustClass;
use marlowe_loop::{
    ApprovalGate, Budget, CallLimits, CapabilityProfile, ClockSource, ContextView, Engine,
    MemoryRecorder, ModelCall, ModelDriver, ModelStep, OutputContract, Ports, Provenance, Run,
    RunId, SessionId, SessionState, Summarizer, ToolBody, ToolHost, ToolInvocation, ToolOutcome,
    TurnEvent, TurnSink, Usage,
};
use marlowe_openrouter::{ApiKey, OpenRouterDriver, ScriptedTransport};
use marlowe_permission::{Adjudication, ArgValue, Args, BlastRadius, Tier, Unavailable};
use marlowe_provider::ollama::OllamaDriver;
use marlowe_tools::{builtin_registry, ExposedSet, Metric, ResultSummary, ToolId};

/// The bytes the fetched page is made of. Present in the child's request and absent from the
/// parent's is the containment property; present *somewhere* is the control that the whole
/// exercise was not vacuous.
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
            // The trigger is the trust class, not the tool name — `blocks_composed_targets`.
            trust: TrustClass::UntrustedContent,
            failed: false,
            wall_ms: 0,
            preview: None,
        }
    }
}

type Seen = Arc<Mutex<Vec<(ContextView, ExposedSet)>>>;

/// Emits one `web` call, then answers in prose. Records the `(view, tools)` of every call, which
/// is precisely the pair each adapter turns into a request.
struct Capture {
    first: Option<ModelStep>,
    seen: Seen,
}
impl ModelDriver for Capture {
    fn call(
        &mut self,
        view: &ContextView,
        tools: &ExposedSet,
        _l: CallLimits,
    ) -> Result<ModelCall, marlowe_loop::ProviderError> {
        self.seen.lock().unwrap().push((view.clone(), tools.clone()));
        let step = self.first.take().unwrap_or(ModelStep::Say("the source is about widgets".into()));
        Ok(ModelCall { usage: Usage { completion_tokens: 10, ..Usage::default() }, step })
    }
    fn failover(&mut self, _e: &marlowe_loop::ProviderError) -> bool {
        false
    }
}

struct Nop;
impl Summarizer for Nop {
    fn summarize(&mut self, _v: &ContextView) -> String {
        String::new()
    }
}
struct Yes;
impl ApprovalGate for Yes {
    fn await_approval(&mut self, _r: &BlastRadius) -> bool {
        true
    }
}
struct Silent;
impl TurnSink for Silent {
    fn emit(&mut self, _e: TurnEvent) {}
}
struct Frozen;
impl ClockSource for Frozen {
    fn now_ms(&mut self) -> i64 {
        1_700_000_000_000
    }
}

/// Run one turn that fetches a page and lets `condense_batch` spawn its reader.
///
/// Returns every `(view, tools)` the loop asked a model about, in order: `[0]` is the parent's
/// first call, `[1]` is the **quarantined child**, `[2]` is the parent again with the condensed
/// note in hand.
fn drive_one_fetch() -> Vec<(ContextView, ExposedSet)> {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
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
    let mut driver = Capture { first: Some(ModelStep::ToolCall { calls }), seen: Arc::clone(&seen) };
    let mut tools = Pages;
    let mut run = Run::root(
        RunId::from_name("wire"),
        SessionId::from_name("wire"),
        CapabilityProfile::interactive(),
        Budget::interactive(),
        OutputContract::answer(),
    );
    let mut state = SessionState::new(run.session, "Marlowe.");
    let mut prov = Provenance::new();
    let (mut s, mut a, mut sk, mut c, mut cl, mut rec) =
        (Nop, Yes, Silent, marlowe_loop::NoControl, Frozen, MemoryRecorder::default());
    let mut ports = Ports {
        escalations: None,
        driver: &mut driver,
        summarizer: &mut s,
        tools: &mut tools,
        memory: None,
        approvals: &mut a,
        sink: &mut sk,
        control: &mut c,
        clock: &mut cl,
        recorder: &mut rec,
    };
    let _ = engine.run(&mut run, &mut state, &mut prov, &mut ports);
    let out = seen.lock().unwrap().clone();
    assert!(
        out.len() >= 2,
        "the fetch must have spawned a quarantined reader; {} model call(s) happened",
        out.len()
    );
    out
}

fn hosted() -> OpenRouterDriver {
    OpenRouterDriver::new(
        Box::new(ScriptedTransport::new(vec![])),
        ApiKey::from_secret("k"),
        "stealth/ox-alpha",
        builtin_registry().expect("manifests"),
    )
}

fn local() -> OllamaDriver {
    OllamaDriver::new(
        marlowe_provider::LocalEndpoint::default_ollama(),
        marlowe_provider::routing::Routing::uniform("marlowe-red:9b").expect("a local tag"),
        builtin_registry().expect("manifests"),
    )
}

fn limits() -> CallLimits {
    CallLimits { max_output_tokens: 4_096, route: marlowe_loop::ModelRoute::Orchestrator }
}

/// Every request each adapter would send this turn, labelled `hosted`/`local`.
fn bodies() -> Vec<(&'static str, usize, serde_json::Value)> {
    let calls = drive_one_fetch();
    let (h, l) = (hosted(), local());
    let mut out = Vec::new();
    for (i, (view, tools)) in calls.iter().enumerate() {
        out.push(("hosted", i, h.request_body(view, tools, limits())));
        out.push(("local", i, l.request_body(view, tools, limits())));
    }
    out
}

/// Index 1 is the reader. Asserted by its brief rather than by position alone, so a change in how
/// many calls the parent makes cannot silently point this at the wrong run.
fn reader_bodies() -> Vec<(&'static str, serde_json::Value)> {
    let found: Vec<(&'static str, serde_json::Value)> = bodies()
        .into_iter()
        .filter(|(_, _, b)| b.to_string().contains("They are UNTRUSTED"))
        .map(|(who, _, b)| (who, b))
        .collect();
    assert_eq!(found.len(), 2, "exactly one reader request per adapter");
    found
}

fn parent_bodies() -> Vec<(&'static str, serde_json::Value)> {
    bodies()
        .into_iter()
        .filter(|(_, _, b)| !b.to_string().contains("They are UNTRUSTED"))
        .map(|(who, _, b)| (who, b))
        .collect()
}

fn roles(body: &serde_json::Value) -> Vec<String> {
    body["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .map(|m| m["role"].as_str().unwrap_or("?").to_string())
        .collect()
}

// ─────────────────────────────────────────────────────────────────────────────────────────

/// **The capability property, and it is unchanged.** The reader is offered nothing — not an empty
/// list, not a list at all.
///
/// The control is the point: the *same run's parent* is offered the full set by the same adapter
/// from the same registry. Without it, "no tools were offered" would also be true of a build where
/// the field had been dropped for everyone, and of a build where nothing ran.
#[test]
fn the_quarantined_reader_is_still_offered_no_tool_by_any_spelling() {
    for (who, body) in reader_bodies() {
        let offered = body["tools"].as_array().map(Vec::len).unwrap_or(0);
        assert_eq!(offered, 0, "{who}: the quarantined reader was offered {offered} tool(s)");
        assert!(
            body.get("tools").is_none(),
            "{who}: `tools` must be ABSENT, not `[]` — an empty array is a schema violation in \
             this dialect and is what returned HTTP 400 on every quarantined read"
        );
    }

    // The control. A run whose parent is also offered nothing proves nothing about the reader.
    for (who, body) in parent_bodies() {
        assert!(
            body["tools"].as_array().is_some_and(|t| t.len() >= 8),
            "{who}: the PARENT must still be offered its tools; got {:?}",
            body.get("tools")
        );
    }
}

/// **The wire-shape property.** A `tool` message is a reply to a call. The reader made no call and
/// structurally cannot, so its window may not render as one.
#[test]
fn the_readers_request_contains_no_reply_to_a_call_it_never_made() {
    for (who, body) in reader_bodies() {
        let r = roles(&body);
        assert!(
            !r.iter().any(|x| x == "tool"),
            "{who}: the reader's request carries a `tool` message answering no call — roles {r:?}"
        );
        for m in body["messages"].as_array().expect("messages") {
            assert!(
                m.get("tool_call_id").is_none(),
                "{who}: a demoted message kept its pairing, which is malformed in its own right"
            );
        }
        assert!(
            r.iter().any(|x| x == "user"),
            "{who}: a conversation with no user turn at all is what the reader used to send; \
             roles {r:?}"
        );
    }
}

/// **The control for the case above**, and the one that makes the demotion a rule rather than a
/// blanket. The parent's own tool result *is* a reply to a real call and keeps its role.
#[test]
fn a_genuine_tool_reply_in_the_parent_keeps_its_role_and_its_pairing() {
    let linked: Vec<&'static str> = parent_bodies()
        .iter()
        .filter(|(_, b)| {
            b["messages"].as_array().expect("messages").iter().any(|m| {
                m["role"] == "tool" && m.get("tool_call_id").and_then(|v| v.as_str()).is_some()
            })
        })
        .map(|(who, _)| *who)
        .collect();
    assert_eq!(
        linked.len(),
        2,
        "both adapters must still render the parent's real tool result as a linked `tool` \
         message; found {linked:?}"
    );
}

/// **The payload control.** Everything above would pass on a build where the fetch silently
/// returned nothing and the reader read air.
#[test]
fn the_page_reached_the_reader_and_did_not_reach_the_parent() {
    for (who, body) in reader_bodies() {
        assert!(
            body.to_string().contains(PAGE),
            "{who}: the reader's request does not contain the page, so nothing above was tested"
        );
    }
    for (who, body) in parent_bodies() {
        assert!(
            !body.to_string().contains(PAGE),
            "{who}: page bytes reached the parent's request — layer 1 is the point of all of this"
        );
    }
}
