//! Loop tests against a scripted provider and a recording tool host. No network, no inference.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use marlowe_model::{
    fold_events, CancelToken, ChatRequest, Completion, Message, ModelInfo, Part, Provider, ProviderError, ProviderKind,
    Role, StopReason, StreamEvent, ToolSpec, Usage,
};
use marlowe_proto::SessionId;
use serde_json::{json, Value};

use crate::context::{SUMMARY_PREFACE, SUMMARY_PROMPT};
use crate::run_tools::DENIED;
use crate::*;

// ---------------------------------------------------------------- scripted provider

enum Step {
    Events(Vec<StreamEvent>),
    Fail(ProviderError),
    /// Emits the events, then blocks until cancelled (or panics after 5 s).
    UntilCancel(Vec<StreamEvent>),
}

struct ScriptedProvider {
    steps: Mutex<VecDeque<Step>>,
    /// Replayed whenever the script is exhausted.
    repeat: Option<Vec<StreamEvent>>,
    requests: Mutex<Vec<ChatRequest>>,
}

impl ScriptedProvider {
    fn new(steps: Vec<Step>) -> Arc<Self> {
        Arc::new(Self { steps: Mutex::new(steps.into()), repeat: None, requests: Mutex::new(Vec::new()) })
    }

    fn repeating(events: Vec<StreamEvent>) -> Arc<Self> {
        Arc::new(Self { steps: Mutex::new(VecDeque::new()), repeat: Some(events), requests: Mutex::new(Vec::new()) })
    }

    fn requests(&self) -> Vec<ChatRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl Provider for ScriptedProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Ollama
    }

    fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(vec![model()])
    }

    fn stream(
        &self,
        req: &ChatRequest,
        cancel: &CancelToken,
        on: &mut dyn FnMut(StreamEvent),
    ) -> Result<Completion, ProviderError> {
        self.requests.lock().unwrap().push(req.clone());
        let step = self.steps.lock().unwrap().pop_front();
        let step = match (step, &self.repeat) {
            (Some(s), _) => s,
            (None, Some(r)) => Step::Events(r.clone()),
            (None, None) => panic!("the script ran out"),
        };
        match step {
            Step::Fail(e) => Err(e),
            Step::Events(evs) => {
                for ev in &evs {
                    if cancel.is_cancelled() {
                        return Err(ProviderError::cancelled());
                    }
                    on(ev.clone());
                }
                Ok(fold_events(&evs))
            }
            Step::UntilCancel(evs) => {
                for ev in &evs {
                    on(ev.clone());
                }
                let end = Instant::now() + Duration::from_secs(5);
                while !cancel.is_cancelled() {
                    assert!(Instant::now() < end, "never cancelled");
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(ProviderError::cancelled())
            }
        }
    }
}

fn text(t: &str) -> StreamEvent {
    StreamEvent::TextDelta(t.to_string())
}

fn call(index: u32, id: &str, name: &str, args: Value) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ToolCallStart { index, id: id.into(), name: name.into() },
        StreamEvent::ToolArgsDelta { index, json: args.to_string() },
        StreamEvent::ToolCallEnd { index },
    ]
}

fn answer(t: &str) -> Step {
    Step::Events(vec![text(t), StreamEvent::Stop(StopReason::EndTurn)])
}

fn model() -> ModelInfo {
    ModelInfo { context_window: Some(32_000), max_output: Some(2_000), ..ModelInfo::bare(ProviderKind::Ollama, "m") }
}

// ---------------------------------------------------------------- recording tools

#[derive(Default)]
struct TestTools {
    calls: Mutex<Vec<(String, String)>>,
    /// Each echo call waits until this many are in flight at once (0 = no wait).
    rendezvous: usize,
    in_flight: AtomicUsize,
    max_in_flight: AtomicUsize,
}

impl TestTools {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn ran(&self, name: &str) -> usize {
        self.calls.lock().unwrap().iter().filter(|(n, _)| n == name).count()
    }
}

impl ToolHost for TestTools {
    fn specs(&self) -> Vec<ToolSpec> {
        ["echo", "fail", "bash", "wait", "fetch"]
            .iter()
            .map(|n| ToolSpec { name: n.to_string(), description: format!("{n} tool"), input_schema: json!({"type": "object"}) })
            .collect()
    }

    fn call(&self, call: &ToolCallReq, ctx: &ToolCtx) -> ToolOutcome {
        self.calls.lock().unwrap().push((call.name.clone(), call.id.clone()));
        match call.name.as_str() {
            "echo" => {
                let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                self.max_in_flight.fetch_max(now, Ordering::SeqCst);
                let end = Instant::now() + Duration::from_secs(3);
                while self.rendezvous > 0
                    && self.max_in_flight.load(Ordering::SeqCst) < self.rendezvous
                    && Instant::now() < end
                {
                    std::thread::sleep(Duration::from_millis(2));
                }
                self.in_flight.fetch_sub(1, Ordering::SeqCst);
                let t = call.args["text"].as_str().unwrap_or("").to_string();
                ToolOutcome::ok(t.clone(), format!("echoed {t}"))
            }
            "fail" => ToolOutcome::error("boom: no such file"),
            "fetch" => ToolOutcome::ok("a page", "fetched"),
            "bash" => ToolOutcome::ok("ran", "exit 0"),
            "wait" => {
                let end = Instant::now() + Duration::from_secs(5);
                while !ctx.cancel.is_cancelled() && Instant::now() < end {
                    std::thread::sleep(Duration::from_millis(5));
                }
                ToolOutcome::error("stopped")
            }
            other => ToolOutcome::error(format!("unknown tool {other}")),
        }
    }

    fn consequence(&self, call: &ToolCallReq) -> Consequence {
        if call.name == "bash" {
            Consequence::Ask { summary: "run a command".into() }
        } else {
            Consequence::Free
        }
    }

    fn consequence_exposed(&self, call: &ToolCallReq, exposure: &Exposure) -> Consequence {
        match self.egress_host(call) {
            Some(h) if exposure.tainted && !exposure.approved_hosts.contains(&h) => {
                Consequence::Ask { summary: format!("fetch {h}") }
            }
            _ => self.consequence(call),
        }
    }

    fn taints(&self, call: &ToolCallReq) -> bool {
        call.name == "fetch"
    }

    fn egress_host(&self, call: &ToolCallReq) -> Option<String> {
        (call.name == "fetch").then(|| call.args["host"].as_str().unwrap_or("").to_string())
    }
}

struct RecordingGate {
    decision: ApprovalDecision,
    asked: Mutex<Vec<ApprovalReq>>,
}

impl RecordingGate {
    fn new(decision: ApprovalDecision) -> Arc<Self> {
        Arc::new(Self { decision, asked: Mutex::new(Vec::new()) })
    }
}

impl ApprovalGate for RecordingGate {
    fn ask(&self, req: ApprovalReq) -> ApprovalDecision {
        self.asked.lock().unwrap().push(req);
        self.decision
    }
}

// ---------------------------------------------------------------- harness

fn agent(provider: Arc<ScriptedProvider>, tools: Arc<TestTools>) -> Agent {
    Agent::new(provider, model(), tools, SystemPrompt::with_core("core"))
}

fn convo() -> Conversation {
    Conversation::new(SessionId::from("s1"), None)
}

struct Run {
    outcome: TurnOutcome,
    events: Vec<AgentEvent>,
}

fn run(a: &Agent, c: &mut Conversation, user: &str) -> Run {
    let (_tx, rx) = crossbeam_channel::unbounded();
    let mut events = Vec::new();
    let outcome = a.run_turn(c, user, &CancelToken::new(), &rx, &mut |e| events.push(e));
    Run { outcome, events }
}

fn tool_results(m: &Message) -> Vec<marlowe_model::ToolResult> {
    m.tool_results_iter().cloned().collect()
}

// ---------------------------------------------------------------- tests

#[test]
fn a_plain_answer_is_one_step() {
    let p = ScriptedProvider::new(vec![Step::Events(vec![
        text("Hello"),
        text(" there"),
        StreamEvent::Usage(Usage { input: 10, output: 2, ..Default::default() }),
        StreamEvent::Stop(StopReason::EndTurn),
    ])]);
    let a = agent(p.clone(), TestTools::new());
    let mut c = convo();
    let r = run(&a, &mut c, "hi");

    assert_eq!(r.outcome.stop, TurnStop::Done);
    assert_eq!(r.outcome.text, "Hello there");
    assert_eq!(r.outcome.steps, 1);
    assert_eq!(r.outcome.usage.input, 10);
    assert_eq!(
        r.events,
        vec![
            AgentEvent::TextDelta("Hello".into()),
            AgentEvent::TextDelta(" there".into()),
            AgentEvent::Usage(Usage { input: 10, output: 2, ..Default::default() }),
        ]
    );
    assert_eq!(c.messages, vec![Message::user("hi"), Message::assistant("Hello there")]);
    assert_eq!(c.next_turn, 1);

    let req = &p.requests()[0];
    assert_eq!(req.model, "m");
    assert_eq!(req.tools.len(), 5);
    assert_eq!(req.system[0].text, "core");
    assert!(req.system.last().unwrap().text.starts_with("<environment>"));
    assert!(req.system.last().unwrap().text.contains("ollama/m"));
}

#[test]
fn text_and_three_parallel_tool_calls_are_all_run_and_attributed() {
    let mut first = vec![text("Let me look.")];
    first.extend(call(0, "", "echo", json!({"text": "a"})));
    first.extend(call(1, "", "echo", json!({"text": "b"})));
    first.extend(call(2, "", "echo", json!({"text": "c"})));
    let p = ScriptedProvider::new(vec![Step::Events(first), answer("Done.")]);
    let tools = Arc::new(TestTools { rendezvous: 3, ..Default::default() });
    let a = agent(p.clone(), tools.clone());
    let mut c = convo();
    let r = run(&a, &mut c, "look");

    assert_eq!(r.outcome.stop, TurnStop::Done);
    assert_eq!(r.outcome.steps, 2);
    assert_eq!(r.outcome.text, "Let me look.\n\nDone.");
    assert_eq!(tools.max_in_flight.load(Ordering::SeqCst), 3, "the three calls ran at the same time");

    // user, assistant(text + 3 calls), tool(3 results), assistant
    assert_eq!(c.messages.len(), 4);
    let asst = &c.messages[1];
    assert_eq!(asst.joined_text(), "Let me look.", "text emitted alongside tool calls is kept");
    let ids: Vec<String> = asst.tool_calls().map(|t| t.id.clone()).collect();
    assert_eq!(ids.len(), 3);
    assert!(ids.iter().all(|i| !i.is_empty()));
    assert_eq!(ids.iter().collect::<std::collections::HashSet<_>>().len(), 3, "ids are unique");

    let results = tool_results(&c.messages[2]);
    assert_eq!(results.iter().map(|r| r.call_id.clone()).collect::<Vec<_>>(), ids);
    assert_eq!(results.iter().map(|r| r.content.as_str()).collect::<Vec<_>>(), ["a", "b", "c"]);

    let started: Vec<String> = r
        .events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolStarted { call_id, tool, summary } => {
                assert_eq!(tool, "echo");
                assert!(summary.starts_with("echo "));
                Some(call_id.clone())
            }
            _ => None,
        })
        .collect();
    assert_eq!(started, ids, "starts are reported in call order");
    let mut finished: Vec<(String, String)> = r
        .events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolFinished { call_id, ok: true, summary } => Some((call_id.clone(), summary.clone())),
            _ => None,
        })
        .collect();
    finished.sort();
    let mut expected: Vec<(String, String)> =
        ids.iter().cloned().zip(["echoed a", "echoed b", "echoed c"].map(String::from)).collect();
    expected.sort();
    assert_eq!(finished, expected);

    // The second request carries the results.
    let second = &p.requests()[1];
    assert_eq!(second.messages.last().unwrap().role, Role::Tool);
}

#[test]
fn provider_ids_are_kept_and_duplicates_replaced() {
    let mut first = call(0, "x", "echo", json!({"text": "1"}));
    first.extend(call(1, "x", "echo", json!({"text": "2"})));
    let p = ScriptedProvider::new(vec![Step::Events(first), answer("ok")]);
    let a = agent(p, TestTools::new());
    let mut c = convo();
    run(&a, &mut c, "go");
    let ids: Vec<String> = c.messages[1].tool_calls().map(|t| t.id.clone()).collect();
    assert_eq!(ids[0], "x");
    assert_ne!(ids[1], "x");
}

#[test]
fn a_tool_error_is_fed_back_to_the_model() {
    let p = ScriptedProvider::new(vec![Step::Events(call(0, "f1", "fail", json!({"path": "nope"}))), answer("It failed.")]);
    let a = agent(p.clone(), TestTools::new());
    let mut c = convo();
    let r = run(&a, &mut c, "read nope");

    assert_eq!(r.outcome.stop, TurnStop::Done);
    let res = tool_results(&c.messages[2]);
    assert_eq!(res.len(), 1);
    assert!(res[0].is_error);
    assert!(res[0].content.contains("boom"));
    assert!(r.events.contains(&AgentEvent::ToolFinished {
        call_id: "f1".into(),
        ok: false,
        summary: "boom: no such file".into()
    }));
    let fed = &p.requests()[1].messages;
    assert!(tool_results(fed.last().unwrap())[0].is_error);
}

#[test]
fn malformed_arguments_are_reported_not_run() {
    let evs = vec![
        StreamEvent::ToolCallStart { index: 0, id: "m1".into(), name: "echo".into() },
        StreamEvent::ToolArgsDelta { index: 0, json: "{\"text\": ".into() },
    ];
    let tools = TestTools::new();
    let p = ScriptedProvider::new(vec![Step::Events(evs), answer("sorry")]);
    let a = agent(p, tools.clone());
    let mut c = convo();
    run(&a, &mut c, "go");
    assert_eq!(tools.ran("echo"), 0);
    let res = tool_results(&c.messages[2]);
    assert!(res[0].is_error && res[0].content.contains("not valid JSON"));
}

#[test]
fn a_denied_call_does_not_run_and_the_model_is_told() {
    let p = ScriptedProvider::new(vec![Step::Events(call(0, "b1", "bash", json!({"command": "rm -rf x"}))), answer("ok")]);
    let tools = TestTools::new();
    let gate = RecordingGate::new(ApprovalDecision::Deny);
    let mut a = agent(p, tools.clone());
    a.approvals = gate.clone();
    let mut c = convo();
    let r = run(&a, &mut c, "clean up");

    assert_eq!(tools.ran("bash"), 0);
    let asked = gate.asked.lock().unwrap();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].tool, "bash");
    assert_eq!(asked[0].call_id, "b1");
    assert_eq!(asked[0].summary, "run a command");
    assert!(asked[0].detail.contains("rm -rf x"));
    let res = tool_results(&c.messages[2]);
    assert_eq!(res[0].content, DENIED);
    assert!(res[0].is_error);
    assert!(r.events.contains(&AgentEvent::ToolFinished { call_id: "b1".into(), ok: false, summary: "denied".into() }));
    assert_eq!(r.outcome.stop, TurnStop::Done);
}

#[test]
fn the_default_gate_denies() {
    let p = ScriptedProvider::new(vec![Step::Events(call(0, "b1", "bash", json!({"command": "ls"}))), answer("ok")]);
    let tools = TestTools::new();
    let a = agent(p, tools.clone());
    let mut c = convo();
    run(&a, &mut c, "ls");
    assert_eq!(tools.ran("bash"), 0);
}

#[test]
fn allow_for_session_asks_once_and_lasts() {
    let mut first = call(0, "b1", "bash", json!({"command": "ls"}));
    first.extend(call(1, "b2", "bash", json!({"command": "pwd"})));
    let p = ScriptedProvider::new(vec![
        Step::Events(first),
        answer("ok"),
        Step::Events(call(0, "b3", "bash", json!({"command": "date"}))),
        answer("ok again"),
    ]);
    let tools = TestTools::new();
    let gate = RecordingGate::new(ApprovalDecision::AllowForSession);
    let mut a = agent(p, tools.clone());
    a.approvals = gate.clone();
    let mut c = convo();
    run(&a, &mut c, "go");
    assert_eq!(tools.ran("bash"), 2);
    assert_eq!(gate.asked.lock().unwrap().len(), 1, "the second call is covered by the first answer");
    assert!(c.is_allowed_for_session("bash"));

    run(&a, &mut c, "again");
    assert_eq!(tools.ran("bash"), 3);
    assert_eq!(gate.asked.lock().unwrap().len(), 1);
}

#[test]
fn an_untrusted_read_gates_new_hosts_and_an_approved_host_stays_open() {
    let p = ScriptedProvider::new(vec![
        Step::Events(call(0, "f1", "fetch", json!({"host": "a.com"}))),
        Step::Events(call(0, "f2", "fetch", json!({"host": "b.com"}))),
        Step::Events(call(0, "f3", "fetch", json!({"host": "b.com"}))),
        answer("done"),
    ]);
    let tools = TestTools::new();
    let gate = RecordingGate::new(ApprovalDecision::Allow);
    let mut a = agent(p, tools.clone());
    a.approvals = gate.clone();
    let mut c = convo();
    run(&a, &mut c, "go");
    assert_eq!(tools.ran("fetch"), 3);
    let asked: Vec<String> = gate.asked.lock().unwrap().iter().map(|r| r.summary.clone()).collect();
    assert_eq!(asked, vec!["fetch b.com"], "only the first call to a new host after the taint asks");
    assert!(c.exposure.tainted);
    assert!(c.exposure.approved_hosts.contains("b.com"));

    // A conversation reloaded from history is tainted again.
    let mut reloaded = Conversation::from_messages(c.session.clone(), None, c.messages.clone(), c.next_turn);
    assert!(!reloaded.exposure.tainted);
    reloaded.recompute_exposure(tools.as_ref());
    assert!(reloaded.exposure.tainted);
}

#[test]
fn cancel_mid_stream_keeps_the_partial_text() {
    let p = ScriptedProvider::new(vec![Step::UntilCancel(vec![text("partial ")])]);
    let a = agent(p, TestTools::new());
    let mut c = convo();
    let cancel = CancelToken::new();
    let (_tx, rx) = crossbeam_channel::unbounded();
    let canceller = cancel.clone();
    let mut events = Vec::new();
    let out = a.run_turn(&mut c, "write a lot", &cancel, &rx, &mut |e| {
        if matches!(e, AgentEvent::TextDelta(_)) {
            canceller.cancel();
        }
        events.push(e);
    });
    assert_eq!(out.stop, TurnStop::Interrupted);
    assert_eq!(out.text, "partial ");
    assert_eq!(c.messages.last().unwrap(), &Message::assistant("partial "));
    assert_eq!(events, vec![AgentEvent::TextDelta("partial ".into())]);
}

#[test]
fn cancel_from_another_thread_stops_a_running_tool() {
    let p = ScriptedProvider::new(vec![Step::Events(call(0, "w1", "wait", json!({})))]);
    let a = agent(p.clone(), TestTools::new());
    let mut c = convo();
    let cancel = CancelToken::new();
    let remote = cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        remote.cancel();
    });
    let (_tx, rx) = crossbeam_channel::unbounded();
    let started = Instant::now();
    let out = a.run_turn(&mut c, "wait", &cancel, &rx, &mut |_| {});
    assert!(started.elapsed() < Duration::from_secs(4));
    assert_eq!(out.stop, TurnStop::Interrupted);
    assert_eq!(p.requests().len(), 1, "no model call after the interrupt");
    // The call and its result stay paired in history.
    assert_eq!(c.messages.last().unwrap().role, Role::Tool);
}

#[test]
fn a_turn_cancelled_before_it_starts_makes_no_call() {
    let p = ScriptedProvider::new(vec![]);
    let a = agent(p.clone(), TestTools::new());
    let mut c = convo();
    let cancel = CancelToken::new();
    cancel.cancel();
    let (_tx, rx) = crossbeam_channel::unbounded();
    let out = a.run_turn(&mut c, "go", &cancel, &rx, &mut |_| {});
    assert_eq!(out.stop, TurnStop::Interrupted);
    assert_eq!(out.steps, 0);
    assert!(p.requests().is_empty());
    assert_eq!(c.messages, vec![Message::user("go")]);
}

#[test]
fn the_step_limit_stops_a_model_that_never_stops_calling_tools() {
    let p = ScriptedProvider::repeating(call(0, "", "echo", json!({"text": "again"})));
    let tools = TestTools::new();
    let a = agent(p.clone(), tools.clone());
    let mut c = convo();
    let r = run(&a, &mut c, "loop");
    assert_eq!(r.outcome.stop, TurnStop::StepLimit);
    assert_eq!(r.outcome.steps, MAX_STEPS);
    assert_eq!(p.requests().len(), MAX_STEPS as usize);
    assert_eq!(tools.ran("echo"), MAX_STEPS as usize);
}

#[test]
fn a_token_budget_stops_the_turn() {
    let mut evs = call(0, "", "echo", json!({"text": "x"}));
    evs.push(StreamEvent::Usage(Usage { input: 600, output: 10, ..Default::default() }));
    let p = ScriptedProvider::repeating(evs);
    let mut a = agent(p, TestTools::new());
    a.budget.max_tokens = Some(1_000);
    let mut c = convo();
    let r = run(&a, &mut c, "go");
    assert_eq!(r.outcome.stop, TurnStop::Budget);
    assert_eq!(r.outcome.steps, 2);
}

#[test]
fn compaction_fires_at_the_trigger_and_keeps_the_live_turn() {
    let p = ScriptedProvider::new(vec![answer("SUMMARY OF EARLIER"), answer("fresh answer")]);
    let mut info = model();
    info.context_window = Some(2_000);
    info.max_output = Some(200);
    let a = Agent::new(p.clone(), info, TestTools::new(), SystemPrompt::with_core("core"));
    let mut c = convo();
    for i in 0..10 {
        c.push(Message::user(format!("question {i} {}", "q".repeat(600))));
        c.push(Message::assistant(format!("answer {i} {}", "a".repeat(600))));
    }
    let r = run(&a, &mut c, "now?");

    assert_eq!(r.outcome.stop, TurnStop::Done);
    assert_eq!(r.outcome.text, "fresh answer");
    let compacted = r.events.iter().find_map(|e| match e {
        AgentEvent::Compacted { before_tokens, after_tokens } => Some((*before_tokens, *after_tokens)),
        _ => None,
    });
    let (before, after) = compacted.expect("compaction ran");
    assert!(after < before);
    assert_eq!(c.summary.as_deref(), Some("SUMMARY OF EARLIER"));
    assert_eq!(c.messages[0], Message::user("now?"), "the live turn survives compaction");
    assert_eq!(c.messages.len(), 2);

    let reqs = p.requests();
    assert_eq!(reqs.len(), 2);
    assert!(reqs[0].tools.is_empty(), "the summariser call has no tools");
    let prompt = reqs[0].messages[0].joined_text();
    assert!(prompt.contains(SUMMARY_PROMPT) && prompt.contains("answer 9"));
    assert!(!prompt.contains("now?"));
    let sys = reqs[1].system_text();
    assert!(sys.contains(SUMMARY_PREFACE) && sys.contains("SUMMARY OF EARLIER"));
    assert_eq!(reqs[1].messages, vec![Message::user("now?")]);
}

#[test]
fn a_small_conversation_is_not_compacted() {
    let p = ScriptedProvider::new(vec![answer("hi")]);
    let a = agent(p.clone(), TestTools::new());
    let mut c = convo();
    let r = run(&a, &mut c, "hello");
    assert!(!r.events.iter().any(|e| matches!(e, AgentEvent::Compacted { .. })));
    assert_eq!(p.requests().len(), 1);
}

#[test]
fn a_steer_during_the_final_answer_gets_its_own_reply() {
    let p = ScriptedProvider::new(vec![answer("first"), answer("second")]);
    let a = agent(p.clone(), TestTools::new());
    let mut c = convo();
    let (tx, rx) = crossbeam_channel::unbounded();
    let mut sent = false;
    let mut events = Vec::new();
    let out = a.run_turn(&mut c, "go", &CancelToken::new(), &rx, &mut |e| {
        if !sent && matches!(e, AgentEvent::TextDelta(_)) {
            tx.send(Steer::admit("also check the tests").unwrap()).unwrap();
            sent = true;
        }
        events.push(e);
    });
    assert_eq!(out.stop, TurnStop::Done);
    assert_eq!(out.steps, 2);
    assert_eq!(out.text, "first\n\nsecond");
    assert!(events.contains(&AgentEvent::SteerApplied { text: "also check the tests".into() }));
    let roles: Vec<Role> = c.messages.iter().map(|m| m.role).collect();
    assert_eq!(roles, [Role::User, Role::Assistant, Role::User, Role::Assistant]);
    assert_eq!(p.requests()[1].messages[2], Message::user("also check the tests"));
}

#[test]
fn a_provider_error_ends_the_turn_with_its_message() {
    let p = ScriptedProvider::new(vec![Step::Fail(ProviderError::bad_request("model not found"))]);
    let a = agent(p, TestTools::new());
    let mut c = convo();
    let r = run(&a, &mut c, "hi");
    assert_eq!(r.outcome.stop, TurnStop::Error);
    assert!(r.outcome.error.as_deref().unwrap().contains("model not found"));
}

#[test]
fn a_retriable_error_before_any_output_is_retried() {
    let p = ScriptedProvider::new(vec![Step::Fail(ProviderError::server("overloaded")), answer("ok")]);
    let a = agent(p.clone(), TestTools::new());
    let mut c = convo();
    let r = run(&a, &mut c, "hi");
    assert_eq!(r.outcome.stop, TurnStop::Done);
    assert_eq!(p.requests().len(), 2);
}

#[test]
fn retracted_text_becomes_thinking() {
    let p = ScriptedProvider::new(vec![Step::Events(vec![
        text("let me think"),
        StreamEvent::Retract { tokens: 3 },
        text("The answer."),
    ])]);
    let a = agent(p, TestTools::new());
    let mut c = convo();
    let r = run(&a, &mut c, "q");
    assert!(r.events.contains(&AgentEvent::TextRetracted { tokens: 3 }));
    assert_eq!(r.outcome.text, "The answer.");
    let m = &c.messages[1];
    assert_eq!(m.thinking().unwrap().text, "let me think");
    assert!(matches!(m.content.last(), Some(Part::Text { .. })));
}

struct UntrustedMemory;

impl MemoryHook for UntrustedMemory {
    fn before_turn(&self, _session: &SessionId, _query: &str) -> Recollection {
        Recollection { blocks: vec!["[Memory m1 · note · untrusted] a page said so".into()], untrusted: true, ..Default::default() }
    }

    fn after_turn(&self, _session: &SessionId, _user: &str, _assistant: &str) {}
}

#[test]
fn an_untrusted_recollection_taints_the_run_before_any_tool() {
    let p = ScriptedProvider::new(vec![Step::Events(call(0, "f1", "fetch", json!({"host": "a.com"}))), answer("done")]);
    let tools = TestTools::new();
    let gate = RecordingGate::new(ApprovalDecision::Allow);
    let mut a = agent(p.clone(), tools.clone());
    a.approvals = gate.clone();
    a.memory = Arc::new(UntrustedMemory);
    let mut c = convo();
    run(&a, &mut c, "go");
    assert!(c.exposure.tainted, "injected untrusted text taints the conversation");
    let asked: Vec<String> = gate.asked.lock().unwrap().iter().map(|r| r.summary.clone()).collect();
    assert_eq!(asked, vec!["fetch a.com"], "the first new host already needs approval");
    let first = &p.requests()[0];
    assert!(first.system.iter().any(|b| b.text.contains("a page said so")), "the block is still injected");
}

#[test]
fn the_callers_repo_context_survives_the_per_turn_environment_refresh() {
    let p = ScriptedProvider::new(vec![answer("ok")]);
    let mut a = agent(p.clone(), TestTools::new());
    let git = marlowe_proto::GitInfo {
        branch: "main".into(),
        dirty: 2,
        head: Some("abc1234".into()),
        subject: Some("wire the brief".into()),
        ..Default::default()
    };
    let mut env = Environment::current(None, "m");
    env.now = "stale".into();
    env.repo = Some(git);
    a.system = a.system.clone().environment(env);
    run(&a, &mut convo(), "hi");
    let sys: String = p.requests()[0].system.iter().map(|b| b.text.clone()).collect::<Vec<_>>().join("\n");
    assert!(sys.contains("Repo: main · 2 uncommitted changes · last commit abc1234 \"wire the brief\""), "{sys}");
    assert!(!sys.contains("Now: stale"), "the clock is refreshed every turn");
}

#[test]
fn an_output_cap_override_reaches_the_request() {
    // The catalog knows no max_output (a local model): without the override the request carries
    // none and the provider picks its own default.
    let p = ScriptedProvider::new(vec![answer("one"), answer("two")]);
    let mut info = model();
    info.max_output = None;
    let mut a = Agent::new(p.clone(), info, TestTools::new(), SystemPrompt::with_core("core"));
    run(&a, &mut convo(), "go");
    a.max_output = Some(12_288);
    run(&a, &mut convo(), "go");
    let reqs = p.requests();
    assert_eq!(reqs[0].max_output_tokens, None);
    assert_eq!(reqs[1].max_output_tokens, Some(12_288));
}
