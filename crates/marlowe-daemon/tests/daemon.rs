//! End-to-end: a daemon on an ephemeral port with a temp profile home, a scripted provider and a
//! fake tool host, driven over the real wire.

use std::collections::VecDeque;
use std::io::BufReader;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, Receiver, Sender};
use marlowe_agent::{Consequence, ToolCallReq, ToolCtx, ToolHost, ToolOutcome, UserInfo};
use marlowe_brains::{Library, MemoryStore, ProviderFactory};
use marlowe_daemon::{Daemon, DaemonConfig, DaemonDeps, DaemonError};
use marlowe_mind::{Kind, MemoryIn, Mind, MindConfig, Provenance, Scope};
use marlowe_model::{
    CancelToken, ChatRequest, Completion, CompletionBuilder, Effort, Message, ModelInfo, ModelRef, Part, Provider,
    ProviderError, ProviderKind, Role, StopReason, StreamEvent, ToolSpec, Usage,
};
use marlowe_proto::{
    paths, write_line, ApprovalDecision, DaemonInfo, Envelope, Event, LineReader, Request, SessionId, Slot, TurnStop,
};

// ---------------------------------------------------------------- scripted provider

enum Step {
    Ev(StreamEvent),
    /// Blocks until released (or cancelled).
    Gate(Receiver<()>),
    /// Blocks until cancelled.
    WaitCancel,
    Sleep(u64),
}

#[derive(Default)]
struct Scripted {
    scripts: Mutex<VecDeque<Vec<Step>>>,
    requests: Mutex<Vec<ChatRequest>>,
}

impl Scripted {
    fn push(&self, steps: Vec<Step>) {
        self.scripts.lock().unwrap().push_back(steps);
    }

    fn requests(&self) -> Vec<ChatRequest> {
        self.requests.lock().unwrap().clone()
    }
}

fn text(s: &str) -> Step {
    Step::Ev(StreamEvent::TextDelta(s.into()))
}

fn usage() -> Step {
    Step::Ev(StreamEvent::Usage(Usage { input: 10, output: 5, ..Default::default() }))
}

fn tool_call(id: &str, name: &str, args: &str) -> Vec<Step> {
    vec![
        Step::Ev(StreamEvent::ToolCallStart { index: 0, id: id.into(), name: name.into() }),
        Step::Ev(StreamEvent::ToolArgsDelta { index: 0, json: args.into() }),
        Step::Ev(StreamEvent::ToolCallEnd { index: 0 }),
        Step::Ev(StreamEvent::Stop(StopReason::ToolUse)),
    ]
}

impl Provider for Scripted {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Ollama
    }

    fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        let mut a = ModelInfo::bare(ProviderKind::Ollama, "scripted");
        a.supports_tools = true;
        a.context_window = Some(32_768);
        Ok(vec![a, ModelInfo::bare(ProviderKind::Ollama, "other")])
    }

    fn stream(&self, req: &ChatRequest, cancel: &CancelToken, on: &mut dyn FnMut(StreamEvent)) -> Result<Completion, ProviderError> {
        self.requests.lock().unwrap().push(req.clone());
        let script = self.scripts.lock().unwrap().pop_front().unwrap_or_else(|| vec![text("ok"), usage()]);
        let mut b = CompletionBuilder::new();
        for step in script {
            if cancel.is_cancelled() {
                return Err(ProviderError::cancelled());
            }
            match step {
                Step::Ev(e) => {
                    b.push(&e);
                    on(e);
                }
                Step::Gate(rx) => loop {
                    if rx.recv_timeout(Duration::from_millis(10)).is_ok() {
                        break;
                    }
                    if cancel.is_cancelled() {
                        return Err(ProviderError::cancelled());
                    }
                },
                Step::WaitCancel => {
                    while !cancel.is_cancelled() {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    return Err(ProviderError::cancelled());
                }
                Step::Sleep(ms) => std::thread::sleep(Duration::from_millis(ms)),
            }
        }
        Ok(b.finish())
    }
}

// ---------------------------------------------------------------- fake tools

struct FakeTools;

impl ToolHost for FakeTools {
    fn specs(&self) -> Vec<ToolSpec> {
        ["echo", "danger"]
            .iter()
            .map(|n| ToolSpec { name: n.to_string(), description: n.to_string(), input_schema: serde_json::json!({"type": "object"}) })
            .collect()
    }

    fn call(&self, call: &ToolCallReq, _ctx: &ToolCtx) -> ToolOutcome {
        match call.name.as_str() {
            "echo" => ToolOutcome::ok(call.args.to_string(), "echoed"),
            "danger" => ToolOutcome::ok("danger ran", "ran"),
            other => ToolOutcome::error(format!("unknown tool {other}")),
        }
    }

    fn consequence(&self, call: &ToolCallReq) -> Consequence {
        if call.name == "danger" {
            Consequence::Ask { summary: "do something dangerous".into() }
        } else {
            Consequence::Free
        }
    }
}

// ---------------------------------------------------------------- harness

fn temp_home(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let home = std::env::temp_dir().join(format!("marlowe-daemon-it-{name}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    home
}

fn seed_model(home: &Path) {
    let lib = Library::open(&paths::cowork_db(home)).unwrap();
    let mut info = ModelInfo::bare(ProviderKind::Ollama, "scripted");
    info.supports_tools = true;
    info.context_window = Some(32_768);
    lib.add(&info).unwrap();
}

fn deps(provider: Arc<Scripted>) -> DaemonDeps {
    deps_with(provider, None)
}

fn deps_with(provider: Arc<Scripted>, mind: Option<Mind>) -> DaemonDeps {
    let mut providers = ProviderFactory::new(Arc::new(MemoryStore::new()));
    providers.register(ProviderKind::Ollama, move |_| Ok(provider.clone() as Arc<dyn Provider>));
    DaemonDeps {
        providers,
        tools: Arc::new(|_| Arc::new(FakeTools) as Arc<dyn ToolHost>),
        user: UserInfo { name: Some("Tester".into()), address: None },
        mind,
        sentinel: None,
        voice: None,
        crew: None,
    }
}

struct Running {
    info: DaemonInfo,
    join: JoinHandle<Result<(), DaemonError>>,
}

fn start(home: &Path, provider: Arc<Scripted>, cfg: DaemonConfig) -> Running {
    start_with(home, provider, cfg, None)
}

fn start_with(home: &Path, provider: Arc<Scripted>, cfg: DaemonConfig, mind: Option<Mind>) -> Running {
    let d = Daemon::start(deps_with(provider, mind), home, cfg).expect("daemon starts");
    let info = d.info();
    assert_eq!(DaemonInfo::read(home).unwrap().as_ref(), Some(&info), "daemon.json names it");
    Running { info, join: std::thread::spawn(move || d.wait()) }
}

fn stop(r: Running, home: &Path) {
    let mut c = Client::connect(&r.info, None);
    c.send(&Request::Shutdown);
    c.until(|e| matches!(e, Event::ShuttingDown));
    r.join.join().unwrap().unwrap();
    assert!(DaemonInfo::read(home).unwrap().is_none(), "daemon.json is removed on exit");
}

const WAIT: Duration = Duration::from_secs(30);

struct Client {
    w: TcpStream,
    r: LineReader<BufReader<TcpStream>>,
    welcome_seq: u64,
}

impl Client {
    fn connect(info: &DaemonInfo, since_seq: Option<u64>) -> Client {
        Self::connect_as(info, since_seq, "test")
    }

    /// `"tui"` is an interactive client: it counts as presence and may be briefed.
    fn connect_as(info: &DaemonInfo, since_seq: Option<u64>, name: &str) -> Client {
        let w = TcpStream::connect(info.addr()).unwrap();
        w.set_read_timeout(Some(WAIT)).unwrap();
        let mut c = Client { r: LineReader::new(BufReader::new(w.try_clone().unwrap())), w, welcome_seq: 0 };
        c.send(&Request::Hello { token: info.token.clone(), client: name.into(), since_seq });
        let env = c.next().expect("welcome");
        match env.event {
            Event::Welcome { seq, .. } => c.welcome_seq = seq,
            other => panic!("expected Welcome, got {other:?}"),
        }
        assert_eq!(env.seq, 0, "welcome is a reply");
        c
    }

    fn send(&mut self, r: &Request) {
        write_line(&mut self.w, r).unwrap();
    }

    fn next(&mut self) -> Option<Envelope> {
        self.r.next::<Envelope>().expect("read an envelope")
    }

    /// Every envelope up to and including the first whose event matches.
    fn until(&mut self, pred: impl Fn(&Event) -> bool) -> Vec<Envelope> {
        let deadline = Instant::now() + WAIT;
        let mut out = Vec::new();
        while Instant::now() < deadline {
            let env = self.next().unwrap_or_else(|| panic!("stream closed; saw {out:#?}"));
            let hit = pred(&env.event);
            out.push(env);
            if hit {
                return out;
            }
        }
        panic!("timed out; saw {out:#?}");
    }

    fn request(&mut self, r: &Request, pred: impl Fn(&Event) -> bool) -> Event {
        self.send(r);
        self.until(pred).pop().unwrap().event
    }
}

fn is_finished(e: &Event) -> bool {
    matches!(e, Event::TurnFinished { .. })
}

fn opened_session(envs: &[Envelope]) -> SessionId {
    envs.iter()
        .find_map(|e| match &e.event {
            Event::TurnStarted { session, .. } => Some(session.clone()),
            _ => None,
        })
        .expect("a TurnStarted")
}

/// The words of each message of `role`; a user message's per-turn tail (memory, environment) is not part of them.
fn texts(m: &[Message], role: Role) -> Vec<String> {
    m.iter().filter(|m| m.role == role).map(|m| strip_tail(&m.joined_text())).collect()
}

fn strip_tail(text: &str) -> String {
    match text.strip_prefix("<system-reminder>").and_then(|t| t.split_once("</system-reminder>")) {
        Some((_, words)) => words.to_string(),
        None => text.to_string(),
    }
}

/// Everything a request shows the model: the system prompt and every message with its tail.
fn whole(req: &ChatRequest) -> String {
    format!("{}\n{}", req.system_text(), req.messages.iter().map(|m| m.joined_text()).collect::<Vec<_>>().join("\n"))
}

// ---------------------------------------------------------------- tests

#[test]
fn two_clients_see_one_identical_stream_and_late_clients_replay_it() {
    let home = temp_home("fanout");
    seed_model(&home);
    let p = Arc::new(Scripted::default());
    p.push(vec![text("Hel"), text("lo"), usage(), Step::Ev(StreamEvent::Stop(StopReason::EndTurn))]);
    let d = start(&home, p.clone(), DaemonConfig::default());

    let mut a = Client::connect(&d.info, None);
    let mut b = Client::connect(&d.info, None);
    a.send(&Request::Ask { session: None, text: "hello there\nsecond line".into(), workspace: Some(home.display().to_string()), images: Vec::new(), options: None, client_ms: None });
    let got_a = a.until(is_finished);
    let got_b = b.until(is_finished);

    let reply_opened = got_a.iter().any(|e| e.seq == 0 && matches!(e.event, Event::SessionOpened { .. }));
    assert!(reply_opened, "the asking client gets a seq-0 SessionOpened");
    let bcast_a: Vec<&Envelope> = got_a.iter().filter(|e| e.seq > 0).collect();
    let bcast_b: Vec<&Envelope> = got_b.iter().filter(|e| e.seq > 0).collect();
    assert_eq!(bcast_a, bcast_b, "both clients see the same numbered stream");
    assert!(bcast_a.windows(2).all(|w| w[1].seq == w[0].seq + 1), "seq is dense and increasing");

    let evs: Vec<&Event> = bcast_a.iter().map(|e| &e.event).collect();
    assert!(evs.iter().any(|e| matches!(e, Event::SessionOpened { title, workspace, .. }
        if title == "hello there" && workspace.as_deref() == marlowe_proto::workspace::normalise(&home.display().to_string()).unwrap().to_str())));
    assert!(evs.iter().any(|e| matches!(e, Event::TurnStarted { user, turn: 0, model, .. }
        if user.starts_with("hello there") && model.to_string() == "ollama/scripted")));
    let deltas: String = evs
        .iter()
        .filter_map(|e| match e {
            Event::TextDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(deltas, "Hello");
    match evs.last().unwrap() {
        Event::TurnFinished { stop, usage, .. } => {
            assert_eq!(*stop, TurnStop::Done);
            assert_eq!(usage.output, 5);
        }
        e => panic!("{e:?}"),
    }
    // The system prompt carries the injected user block.
    assert!(p.requests()[0].system_text().contains("Tester"));

    // A late client replays everything after seq 0, in order, after its Welcome.
    let mut c = Client::connect(&d.info, Some(0));
    assert_eq!(c.welcome_seq, bcast_a.last().unwrap().seq);
    let replay = c.until(is_finished);
    assert_eq!(replay.iter().collect::<Vec<_>>(), bcast_a);

    match c.request(&Request::ListSessions { limit: 10 }, |e| matches!(e, Event::SessionList { .. })) {
        Event::SessionList { sessions } => {
            assert_eq!(sessions.len(), 1);
            assert_eq!(sessions[0].title, "hello there");
            assert_eq!(sessions[0].turns, 1);
        }
        _ => unreachable!(),
    }
    stop(d, &home);
}

#[test]
fn a_second_ask_is_queued_and_interrupt_cancels() {
    let home = temp_home("queue");
    seed_model(&home);
    let p = Arc::new(Scripted::default());
    let (release, gate): (Sender<()>, Receiver<()>) = bounded(1);
    p.push(vec![text("first"), Step::Gate(gate)]);
    p.push(vec![text("second")]);
    let d = start(&home, p.clone(), DaemonConfig::default());
    let mut a = Client::connect(&d.info, None);

    a.send(&Request::Ask { session: None, text: "one".into(), workspace: None, images: Vec::new(), options: None, client_ms: None });
    let head = a.until(|e| matches!(e, Event::TextDelta { text, .. } if text == "first"));
    let sid = opened_session(&head);
    a.send(&Request::Ask { session: Some(sid.clone()), text: "two".into(), workspace: None, images: Vec::new(), options: None, client_ms: None });
    match a.request(&Request::Status, |e| matches!(e, Event::Status(_))) {
        Event::Status(s) => assert_eq!(s.active_sessions, vec![sid.clone()]),
        _ => unreachable!(),
    }
    release.send(()).unwrap();

    // The queued ask is answered, in this turn (as a steer) or the next; never concurrently.
    let mut log = a.until(is_finished);
    let deadline = Instant::now() + WAIT;
    while p.requests().len() < 2 && Instant::now() < deadline {
        log.extend(a.until(is_finished));
    }
    let reqs = p.requests();
    assert_eq!(reqs.len(), 2, "two model calls");
    assert_eq!(texts(&reqs[1].messages, Role::User), vec!["one", "two"]);
    assert_eq!(texts(&reqs[1].messages, Role::Assistant), vec!["first"]);
    let mut open = 0;
    for e in &log {
        match e.event {
            Event::TurnStarted { .. } => {
                open += 1;
                assert_eq!(open, 1, "turns in one session never overlap");
            }
            Event::TurnFinished { .. } => open -= 1,
            _ => {}
        }
    }

    // Interrupt cancels a running turn promptly.
    p.push(vec![text("long"), Step::WaitCancel]);
    a.send(&Request::Ask { session: Some(sid.clone()), text: "three".into(), workspace: None, images: Vec::new(), options: None, client_ms: None });
    a.until(|e| matches!(e, Event::TextDelta { text, .. } if text == "long"));
    let t0 = Instant::now();
    a.send(&Request::Interrupt { session: sid.clone() });
    match a.until(is_finished).pop().unwrap().event {
        Event::TurnFinished { stop, .. } => assert_eq!(stop, TurnStop::Interrupted),
        _ => unreachable!(),
    }
    assert!(t0.elapsed() < Duration::from_secs(5));
    match a.request(&Request::Status, |e| matches!(e, Event::Status(_))) {
        Event::Status(s) => assert!(s.active_sessions.is_empty()),
        _ => unreachable!(),
    }
    stop(d, &home);
}

#[test]
fn a_restart_preserves_sessions_and_turns() {
    let home = temp_home("restart");
    seed_model(&home);
    let p = Arc::new(Scripted::default());
    p.push(vec![text("noted")]);
    let d = start(&home, p, DaemonConfig::default());
    let mut a = Client::connect(&d.info, None);
    a.send(&Request::Ask { session: None, text: "remember me".into(), workspace: None, images: Vec::new(), options: None, client_ms: None });
    let sid = opened_session(&a.until(is_finished));
    drop(a);
    stop(d, &home);

    let p2 = Arc::new(Scripted::default());
    let d = start(&home, p2.clone(), DaemonConfig::default());
    let mut a = Client::connect(&d.info, None);
    match a.request(&Request::ListSessions { limit: 5 }, |e| matches!(e, Event::SessionList { .. })) {
        Event::SessionList { sessions } => {
            assert_eq!(sessions.len(), 1);
            assert_eq!((sessions[0].id.clone(), sessions[0].title.as_str(), sessions[0].turns), (sid.clone(), "remember me", 1));
        }
        _ => unreachable!(),
    }
    a.send(&Request::OpenSession { session: sid.clone() });
    let got = a.until(|e| matches!(e, Event::Transcript { .. }));
    assert!(got.iter().any(|e| e.seq == 0 && matches!(&e.event, Event::SessionOpened { title, .. } if title == "remember me")));
    match &got.last().unwrap().event {
        Event::Transcript { turns, .. } => {
            let roles: Vec<Role> = turns.iter().map(|t| t.role).collect();
            assert_eq!(roles, vec![Role::User, Role::Assistant]);
            assert_eq!(turns[1].content, vec![Part::Text { text: "noted".into() }]);
        }
        _ => unreachable!(),
    }

    // The reloaded conversation continues where it left off.
    a.send(&Request::Ask { session: Some(sid.clone()), text: "still there?".into(), workspace: None, images: Vec::new(), options: None, client_ms: None });
    let log = a.until(is_finished);
    assert!(log.iter().any(|e| matches!(e.event, Event::TurnStarted { turn: 1, .. })));
    let req = &p2.requests()[0];
    assert_eq!(texts(&req.messages, Role::User), vec!["remember me", "still there?"]);
    assert_eq!(texts(&req.messages, Role::Assistant), vec!["noted"]);
    stop(d, &home);
}

#[test]
fn an_idle_session_is_evicted_and_reloads_on_the_next_ask() {
    let home = temp_home("idle");
    seed_model(&home);
    let p = Arc::new(Scripted::default());
    p.push(vec![text("first")]);
    p.push(vec![text("second")]);
    let cfg = DaemonConfig { session_idle: Duration::from_millis(50), ..DaemonConfig::default() };
    let d = start(&home, p.clone(), cfg);
    let mut a = Client::connect(&d.info, None);
    a.send(&Request::Ask { session: None, text: "one".into(), workspace: None, images: Vec::new(), options: None, client_ms: None });
    let sid = opened_session(&a.until(is_finished));
    std::thread::sleep(Duration::from_millis(300));
    a.send(&Request::Ask { session: Some(sid.clone()), text: "two".into(), workspace: None, images: Vec::new(), options: None, client_ms: None });
    let log = a.until(is_finished);
    assert!(log.iter().any(|e| matches!(&e.event, Event::TurnStarted { session, turn: 1, .. } if *session == sid)));
    let reqs = p.requests();
    assert_eq!(texts(&reqs[1].messages, Role::User), vec!["one", "two"], "history reloaded from the store");
    stop(d, &home);
}

#[test]
fn a_slow_client_is_dropped_and_the_daemon_carries_on() {
    let home = temp_home("slow");
    seed_model(&home);
    let p = Arc::new(Scripted::default());
    let chunk = "x".repeat(8 * 1024);
    let mut flood = Vec::new();
    for i in 0..6_000 {
        flood.push(text(&chunk));
        if i % 100 == 99 {
            flood.push(Step::Sleep(5));
        }
    }
    p.push(flood);
    let cfg = DaemonConfig {
        client_queue: 1_024,
        replay_ring: 512,
        write_timeout: Duration::from_secs(2),
        ..DaemonConfig::default()
    };
    let d = start(&home, p, cfg);

    // Greets, then never reads.
    let mut slow = TcpStream::connect(d.info.addr()).unwrap();
    write_line(&mut slow, &Request::Hello { token: d.info.token.clone(), client: "slow".into(), since_seq: None }).unwrap();

    let mut fast = Client::connect(&d.info, None);
    fast.send(&Request::Ask { session: None, text: "flood".into(), workspace: None, images: Vec::new(), options: None, client_ms: None });
    let log = fast.until(is_finished);
    let deltas = log.iter().filter(|e| matches!(e.event, Event::TextDelta { .. })).count();
    assert_eq!(deltas, 6_000, "the reading client got everything");
    match fast.request(&Request::Status, |e| matches!(e, Event::Status(_))) {
        Event::Status(s) => assert_eq!(s.clients, 1, "the slow client was dropped"),
        _ => unreachable!(),
    }
    // Still serving.
    fast.send(&Request::Ask { session: None, text: "after".into(), workspace: None, images: Vec::new(), options: None, client_ms: None });
    fast.until(is_finished);
    drop(slow);
    stop(d, &home);
}

#[test]
fn approvals_round_trip_and_time_out_to_deny() {
    let home = temp_home("approve");
    seed_model(&home);
    let p = Arc::new(Scripted::default());
    p.push(tool_call("c1", "danger", "{}"));
    p.push(vec![text("done")]);
    p.push(tool_call("c2", "danger", "{}"));
    p.push(vec![text("gave up")]);
    let cfg = DaemonConfig { approval_timeout: Duration::from_millis(400), ..DaemonConfig::default() };
    let d = start(&home, p.clone(), cfg);
    let mut a = Client::connect(&d.info, None);
    let mut b = Client::connect(&d.info, None);

    a.send(&Request::Ask { session: None, text: "do it".into(), workspace: None, images: Vec::new(), options: None, client_ms: None });
    let asked = b.until(|e| matches!(e, Event::ApprovalRequested { .. }));
    let (approval, sid) = match &asked.last().unwrap().event {
        Event::ApprovalRequested { approval, session, tool, summary, .. } => {
            assert_eq!((tool.as_str(), summary.as_str()), ("danger", "do something dangerous"));
            (*approval, session.clone())
        }
        _ => unreachable!(),
    };
    // Any client may answer; an unknown id is refused.
    b.send(&Request::Approve { approval: marlowe_proto::ApprovalId(9_999), decision: ApprovalDecision::Allow, reason: None });
    b.until(|e| matches!(e, Event::Error { message, .. } if message.contains("not pending")));
    b.send(&Request::Approve { approval, decision: ApprovalDecision::Allow, reason: None });
    let log = a.until(is_finished);
    assert!(log.iter().any(|e| matches!(e.event, Event::ApprovalResolved { approval: x, decision: ApprovalDecision::Allow } if x == approval)));
    assert!(log.iter().any(|e| matches!(&e.event, Event::ToolFinished { ok: true, call_id, .. } if call_id == "c1")));
    let results: Vec<String> = p.requests()[1]
        .messages
        .iter()
        .flat_map(|m| m.tool_results_iter().map(|r| r.content.clone()).collect::<Vec<_>>())
        .collect();
    assert_eq!(results, vec!["danger ran"]);

    // Unanswered: denied after the timeout, and the turn still finishes.
    a.send(&Request::Ask { session: Some(sid), text: "again".into(), workspace: None, images: Vec::new(), options: None, client_ms: None });
    let log = a.until(is_finished);
    assert!(log.iter().any(|e| matches!(e.event, Event::ApprovalResolved { decision: ApprovalDecision::Deny, .. })));
    assert!(log.iter().any(|e| matches!(&e.event, Event::ToolFinished { ok: false, call_id, .. } if call_id == "c2")));
    stop(d, &home);
}

/// 2026-10-21: a turn a workflow notice started asked to run a tool nobody was there to answer;
/// the user's follow-up waited behind it for the ten-minute timeout (589 s wall, 10 s of model
/// time). A message from the user now answers the request: it is denied and the turn takes the
/// message at its next step.
#[test]
fn a_new_message_denies_the_request_the_turn_waits_on_instead_of_queueing_behind_it() {
    let home = temp_home("moved-on");
    seed_model(&home);
    let p = Arc::new(Scripted::default());
    p.push(tool_call("c1", "danger", "{}"));
    p.push(vec![text("It found three folders.")]);
    let cfg = DaemonConfig { approval_timeout: Duration::from_secs(120), ..DaemonConfig::default() };
    let d = start(&home, p.clone(), cfg);
    let mut a = Client::connect(&d.info, None);
    a.send(&Request::Ask { session: None, text: "run the workflow".into(), workspace: None, images: Vec::new(), options: None, client_ms: None });
    let asked = a.until(|e| matches!(e, Event::ApprovalRequested { .. }));
    let sid = match &asked.last().unwrap().event {
        Event::ApprovalRequested { session, .. } => session.clone(),
        _ => unreachable!(),
    };
    let began = std::time::Instant::now();
    let mut b = Client::connect(&d.info, None);
    b.send(&Request::Ask { session: Some(sid), text: "What did the workflow find?".into(), workspace: None, images: Vec::new(), options: None, client_ms: None });
    let log = a.until(is_finished);
    assert!(began.elapsed() < Duration::from_secs(30), "not held until the timeout: {:?}", began.elapsed());
    assert!(log.iter().any(|e| matches!(e.event, Event::ApprovalResolved { decision: ApprovalDecision::Deny, .. })));
    let next = &p.requests()[1];
    let results: Vec<String> = next.messages.iter().flat_map(|m| m.tool_results_iter().map(|r| r.content.clone()).collect::<Vec<_>>()).collect();
    assert!(results.iter().any(|r| r.contains("new message")), "the model is told why: {results:?}");
    assert!(next.messages.iter().any(|m| m.joined_text().contains("What did the workflow find?")), "and it gets the message in the same turn");
    stop(d, &home);
}

#[test]
fn library_ops_reply_or_broadcast() {
    let home = temp_home("library");
    seed_model(&home);
    let p = Arc::new(Scripted::default());
    let d = start(&home, p, DaemonConfig::default());
    let mut a = Client::connect(&d.info, None);
    let mut b = Client::connect(&d.info, None);

    a.send(&Request::LibraryState);
    let got = a.until(|e| matches!(e, Event::LibraryState { .. }));
    let env = got.last().unwrap();
    assert_eq!(env.seq, 0, "a state query is a reply");
    match &env.event {
        Event::LibraryState { models, slots, providers } => {
            assert_eq!(models.len(), 1);
            assert!(slots.iter().all(|s| s.model.as_ref().map(|m| m.to_string()).as_deref() == Some("ollama/scripted")));
            assert!(!providers.is_empty());
        }
        _ => unreachable!(),
    }

    match a.request(&Request::ListCatalog { provider: ProviderKind::Ollama }, |e| matches!(e, Event::Catalog { .. })) {
        Event::Catalog { models, .. } => assert_eq!(models.len(), 2),
        _ => unreachable!(),
    }

    a.send(&Request::AddModel { provider: ProviderKind::Ollama, model_id: "other".into() });
    let seen = b.until(|e| matches!(e, Event::LibraryState { .. }));
    let env = seen.last().unwrap();
    assert!(env.seq > 0, "a library change is broadcast");
    match &env.event {
        Event::LibraryState { models, .. } => assert_eq!(models.len(), 2),
        _ => unreachable!(),
    }

    a.send(&Request::ListCatalog { provider: ProviderKind::Anthropic });
    a.until(|e| matches!(e, Event::Error { .. }));
    stop(d, &home);
}

#[test]
fn a_wrong_token_is_refused_and_a_second_daemon_will_not_start() {
    let home = temp_home("auth");
    seed_model(&home);
    let p = Arc::new(Scripted::default());
    let d = start(&home, p.clone(), DaemonConfig::default());

    let mut s = TcpStream::connect(d.info.addr()).unwrap();
    s.set_read_timeout(Some(WAIT)).unwrap();
    write_line(&mut s, &Request::Hello { token: "f".repeat(64), client: "x".into(), since_seq: None }).unwrap();
    let mut r = LineReader::new(BufReader::new(s.try_clone().unwrap()));
    match r.next::<Envelope>().unwrap() {
        Some(Envelope { event: Event::Error { message, .. }, .. }) => assert_eq!(message, marlowe_proto::auth::REFUSED),
        other => panic!("{other:?}"),
    }
    assert!(matches!(r.next::<Envelope>(), Ok(None) | Err(_)), "then closed");

    match Daemon::start(deps(p), &home, DaemonConfig::default()) {
        Err(DaemonError::AlreadyRunning(_)) => {}
        Err(e) => panic!("{e}"),
        Ok(_) => panic!("a second daemon started on the same home"),
    }
    stop(d, &home);
}

#[test]
fn a_client_that_idles_past_the_hello_timeout_stays_connected() {
    let home = temp_home("idle");
    seed_model(&home);
    let p = Arc::new(Scripted::default());
    p.push(vec![text("still here"), Step::Ev(StreamEvent::Stop(StopReason::EndTurn))]);
    let cfg = DaemonConfig { hello_timeout: Duration::from_millis(200), ..DaemonConfig::default() };
    let d = start(&home, p, cfg);

    let mut a = Client::connect(&d.info, None);
    std::thread::sleep(Duration::from_millis(800));
    a.send(&Request::Ask { session: None, text: "after a pause".into(), workspace: None, images: Vec::new(), options: None, client_ms: None });
    let got = a.until(is_finished);
    assert!(matches!(got.last().map(|e| &e.event), Some(Event::TurnFinished { stop: TurnStop::Done, .. })));
    stop(d, &home);
}

#[test]
fn an_interactive_client_is_briefed_once_into_the_days_session() {
    let home = temp_home("brief");
    seed_model(&home);
    let p = Arc::new(Scripted::default());
    let d = start(&home, p.clone(), DaemonConfig::default());

    let mut tui = Client::connect_as(&d.info, None, "tui");
    let env = tui.until(|e| matches!(e, Event::Brief { .. })).pop().unwrap();
    assert_eq!(env.seq, 0, "a brief is a reply to the client that arrived");
    let Event::Brief { text, spoken, session, .. } = env.event else { unreachable!() };
    // No Sentinel in this daemon: the brief says so instead of claiming a quiet night.
    assert!(text.ends_with("The Sentinel is not running, so I have nothing on your runs."), "{text}");
    assert_eq!(spoken, text);
    let sid = session.expect("the brief is stored in the day's session");
    match tui.request(&Request::OpenSession { session: sid.clone() }, |e| matches!(e, Event::Transcript { .. })) {
        Event::Transcript { turns, .. } => {
            assert_eq!(turns.len(), 1);
            assert_eq!(turns[0].role, Role::Assistant);
            assert_eq!(Message { role: turns[0].role, content: turns[0].content.clone() }.joined_text(), text);
        }
        _ => unreachable!(),
    }
    drop(tui);

    // Back within minutes on the same day: no second brief.
    let mut again = Client::connect_as(&d.info, None, "tui");
    again.send(&Request::Status);
    let seen = again.until(|e| matches!(e, Event::Status(_)));
    assert!(!seen.iter().any(|e| matches!(e.event, Event::Brief { .. })), "{seen:#?}");

    // `marlowe brief` asks explicitly: composed now, not stored.
    let mut cli = Client::connect(&d.info, None);
    match cli.request(&Request::Brief, |e| matches!(e, Event::Brief { .. })) {
        Event::Brief { text, session, .. } => {
            assert!(text.ends_with("The Sentinel is not running, so I have nothing on your runs."), "{text}");
            assert_eq!(session, None);
        }
        _ => unreachable!(),
    }
    // Without a Sentinel or a voice those requests say so.
    match cli.request(&Request::WatchList, |e| matches!(e, Event::Error { .. })) {
        Event::Error { message, .. } => assert!(message.contains("Sentinel"), "{message}"),
        _ => unreachable!(),
    }
    match cli.request(&Request::Speak { text: "hello".into() }, |e| matches!(e, Event::Error { .. })) {
        Event::Error { message, .. } => assert!(message.contains("voice"), "{message}"),
        _ => unreachable!(),
    }
    assert!(p.requests().is_empty(), "a quiet brief makes no model call");
    stop(d, &home);
}

fn user_event(e: &Event) -> Option<(Option<String>, Option<String>, bool)> {
    match e {
        Event::User { name, address, set } => Some((name.clone(), address.clone(), *set)),
        _ => None,
    }
}

#[test]
fn set_user_persists_reaches_the_next_turn_and_the_next_brief_greets_with_the_address() {
    let home = temp_home("user");
    seed_model(&home);
    let p = Arc::new(Scripted::default());
    let mut unnamed = deps(p.clone());
    unnamed.user = UserInfo::default();
    let d = Daemon::start(unnamed, &home, DaemonConfig::default()).unwrap();
    let info = d.info();
    let d = Running { info, join: std::thread::spawn(move || d.wait()) };

    // Unset: the Welcome flow says so, as a reply.
    let mut a = Client::connect(&d.info, None);
    let env = a.next().unwrap();
    assert_eq!(env.seq, 0);
    assert_eq!(user_event(&env.event), Some((None, None, false)));
    let mut b = Client::connect(&d.info, None);

    // A bad value is refused and nothing is written.
    match a.request(&Request::SetUser { name: Some("Matt\nhew".into()), address: None }, |e| matches!(e, Event::Error { .. })) {
        Event::Error { message, .. } => assert!(message.contains("control character"), "{message}"),
        _ => unreachable!(),
    }
    assert!(marlowe_daemon::user::load(&home).is_none());

    a.send(&Request::SetUser { name: Some(" Matthew ".into()), address: Some("sir".into()) });
    let set = |e: &Event| matches!(e, Event::User { set: true, .. });
    let env = a.until(set).pop().unwrap();
    assert!(env.seq > 0, "SetUser is broadcast");
    let want = (Some("Matthew".to_string()), Some("sir".to_string()), true);
    assert_eq!(user_event(&env.event), Some(want.clone()));
    assert_eq!(user_event(&b.until(set).pop().unwrap().event), Some(want.clone()), "every client hears it");
    assert_eq!(
        marlowe_daemon::user::load(&home),
        Some(UserInfo { name: Some("Matthew".into()), address: Some("sir".into()) }),
        "user.json holds it"
    );

    // Live: the brief greets with the address, and the next turn's user block carries both.
    match a.request(&Request::Brief, |e| matches!(e, Event::Brief { .. })) {
        Event::Brief { text, .. } => {
            let greeting = text.split_once('.').map(|(g, _)| g).unwrap_or(&text);
            assert!(greeting.ends_with(", sir"), "{text}");
        }
        _ => unreachable!(),
    }
    a.send(&Request::Ask { session: None, text: "hello".into(), workspace: None, images: Vec::new(), options: None, client_ms: None });
    a.until(is_finished);
    let system = p.requests()[0].system_text();
    assert!(system.contains("Name: Matthew") && system.contains("Formal address: sir"), "{system}");
    stop(d, &home);

    // After a restart the binary's loader finds it, and the Welcome flow sends it.
    let mut named = deps(p.clone());
    named.user = marlowe_daemon::user::load(&home).unwrap();
    let d = Daemon::start(named, &home, DaemonConfig::default()).unwrap();
    let info = d.info();
    let d = Running { info, join: std::thread::spawn(move || d.wait()) };
    let mut c = Client::connect(&d.info, None);
    assert_eq!(user_event(&c.next().unwrap().event), Some(want));
    // Clearing the address is a choice too: still `set`, and stored as such.
    c.send(&Request::SetUser { name: Some("Matthew".into()), address: None });
    let env = c.until(set).pop().unwrap();
    assert_eq!(user_event(&env.event), Some((Some("Matthew".into()), None, true)));
    assert_eq!(marlowe_daemon::user::load(&home), Some(UserInfo { name: Some("Matthew".into()), address: None }));
    stop(d, &home);
}

// ---------------------------------------------------------------- memory

/// The shipped ONNX models in the dev tree (git-ignored; CPU only).
fn models_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("models")
}

fn models_present() -> bool {
    let m = models_dir();
    m.join(marlowe_mind::EMBEDDER_DIR).join("model.onnx").is_file()
        && m.join(marlowe_mind::RERANKER_DIR).join("model.onnx").is_file()
}

fn open_mind(home: &Path, models: PathBuf) -> Mind {
    Mind::open_with(home, MindConfig { models_dir: Some(models), ..MindConfig::default() }).expect("mind opens")
}

/// No legacy offer: the machine's real old profile must not leak into tests.
fn mem_cfg() -> DaemonConfig {
    DaemonConfig { legacy_offer: false, ..DaemonConfig::default() }
}

fn recalled(log: &[Envelope], sid: &SessionId) -> Option<(u32, String)> {
    log.iter().find_map(|e| match &e.event {
        Event::Recalled { session, count, summary, .. } if session == sid => Some((*count, summary.clone())),
        _ => None,
    })
}

#[test]
fn a_statement_in_one_session_is_recalled_in_another_after_a_restart() {
    if !models_present() {
        eprintln!("skipped: models are not in {}", models_dir().display());
        return;
    }
    let home = temp_home("recall");
    seed_model(&home);
    let p = Arc::new(Scripted::default());
    p.push(vec![text("Noted.")]);
    p.push(vec![text("Noted too.")]);
    let d = start_with(&home, p, mem_cfg(), Some(open_mind(&home, models_dir())));
    let mut a = Client::connect(&d.info, None);
    let statement = "My sister Ingrid lives in Tromsø and works as a marine biologist studying orcas.";
    a.send(&Request::Ask { session: None, text: statement.into(), workspace: None, images: Vec::new(), options: None, client_ms: None });
    let sid_a = opened_session(&a.until(is_finished));
    // The shipped gate needs a rank-2 candidate to take its margin from; one belief abstains.
    let other = "The training cluster is called Hyperion and has eight A100 GPUs.";
    a.send(&Request::Ask { session: Some(sid_a.clone()), text: other.into(), workspace: None, images: Vec::new(), options: None, client_ms: None });
    a.until(is_finished);
    drop(a);
    // The daemon shuts the mind down (final snapshot) and releases the profile.
    stop(d, &home);

    let p2 = Arc::new(Scripted::default());
    let d = start_with(&home, p2.clone(), mem_cfg(), Some(open_mind(&home, models_dir())));
    let mut b = Client::connect(&d.info, None);
    b.send(&Request::Ask { session: None, text: "Where does my sister Ingrid live, and what is her job?".into(), workspace: None, images: Vec::new(), options: None, client_ms: None });
    let log = b.until(is_finished);
    let sid_b = opened_session(&log);
    assert_ne!(sid_a, sid_b, "a different session");
    let (count, summary) = recalled(&log, &sid_b).unwrap_or_else(|| panic!("no Recalled event; saw {log:#?}"));
    assert!(count >= 1 && summary.contains("Tromsø"), "{count} {summary:?}");
    let shown = whole(&p2.requests()[0]);
    assert!(shown.contains("Tromsø"), "the injected block reaches the provider:\n{shown}");

    // Explicit search finds it too, at user trust.
    match b.request(&Request::MemorySearch { query: "Ingrid Tromsø".into(), scope: None, limit: 5 }, |e| {
        matches!(e, Event::MemoryResults { .. })
    }) {
        Event::MemoryResults { hits } => {
            let h = hits.iter().find(|h| h.text.contains("Tromsø")).expect("the statement is stored");
            assert_eq!(h.trust, marlowe_proto::MemoryTrust::User);
        }
        _ => unreachable!(),
    }

    // A probe runs the same injection path outside a turn.
    match b.request(&Request::MemoryProbe { query: "Where does Ingrid live?".into(), workspace: None }, |e| {
        matches!(e, Event::MemoryProbed { .. })
    }) {
        Event::MemoryProbed { recalled, abstained, .. } => {
            assert!(abstained.is_none() && recalled.iter().any(|h| h.text.contains("Tromsø")), "{recalled:?} {abstained:?}");
        }
        _ => unreachable!(),
    }
    stop(d, &home);
}

#[test]
fn memory_tools_and_requests_work_without_models_and_say_so() {
    let home = temp_home("memops");
    seed_model(&home);
    // An empty models dir: degraded, lexical only, no auto-injection.
    let empty = home.join("no-models");
    std::fs::create_dir_all(&empty).unwrap();
    let p = Arc::new(Scripted::default());
    p.push(tool_call("m1", "remember", r#"{"text":"The training cluster is called Hyperion.","kind":"fact"}"#));
    p.push(vec![text("Stored.")]);
    let d = start_with(&home, p.clone(), mem_cfg(), Some(open_mind(&home, empty)));
    let mut a = Client::connect(&d.info, None);

    match a.request(&Request::MemoryStats, |e| matches!(e, Event::MemoryStats(_))) {
        Event::MemoryStats(s) => assert!(s.degraded.is_some(), "degraded mode is reported"),
        _ => unreachable!(),
    }
    a.send(&Request::Ask { session: None, text: "remember the cluster name".into(), workspace: None, images: Vec::new(), options: None, client_ms: None });
    let log = a.until(is_finished);
    assert!(log.iter().any(|e| matches!(&e.event, Event::ToolFinished { ok: true, call_id, .. } if call_id == "m1")), "{log:#?}");
    let changed = log.iter().find_map(|e| match &e.event {
        Event::MemoryChanged { id, change: marlowe_proto::MemoryChange::Remembered } => Some(id.clone()),
        _ => None,
    });
    let id = changed.expect("remember broadcasts MemoryChanged");
    let tools: Vec<String> = p.requests()[0].tools.iter().map(|t| t.name.clone()).collect();
    for t in marlowe_daemon::MEMORY_TOOLS {
        assert!(tools.iter().any(|n| n == t), "{t} is exposed: {tools:?}");
    }
    assert!(tools.iter().any(|n| n == "echo"), "composed with the session's own tools");

    let hits = match a.request(&Request::MemorySearch { query: "training cluster Hyperion".into(), scope: None, limit: 5 }, |e| {
        matches!(e, Event::MemoryResults { .. })
    }) {
        Event::MemoryResults { hits } => hits,
        _ => unreachable!(),
    };
    let h = hits.iter().find(|h| h.id == id).expect("the agent's memory is searchable");
    assert_eq!(h.trust, marlowe_proto::MemoryTrust::Inferred, "an agent write is inferred, not user");

    a.send(&Request::MemoryForget { id: id.clone() });
    a.until(|e| matches!(e, Event::MemoryChanged { id: x, change: marlowe_proto::MemoryChange::Forgotten } if *x == id));
    a.send(&Request::MemoryForget { id: "no-such-memory".into() });
    a.until(|e| matches!(e, Event::Error { session: None, .. }));
    // Degraded: a probe abstains and says why.
    match a.request(&Request::MemoryProbe { query: "training cluster".into(), workspace: None }, |e| {
        matches!(e, Event::MemoryProbed { .. })
    }) {
        Event::MemoryProbed { recalled, abstained, .. } => {
            assert!(recalled.is_empty() && abstained.is_some(), "{recalled:?} {abstained:?}");
        }
        _ => unreachable!(),
    }
    stop(d, &home);
}

#[test]
fn declining_the_memory_models_is_remembered_across_a_restart() {
    let home = temp_home("models-decline");
    seed_model(&home);
    let empty = home.join("no-models");
    std::fs::create_dir_all(&empty).unwrap();
    let stats = |c: &mut Client| match c.request(&Request::MemoryStats, |e| matches!(e, Event::MemoryStats(_))) {
        Event::MemoryStats(s) => s,
        _ => unreachable!(),
    };

    let d = start_with(&home, Arc::new(Scripted::default()), mem_cfg(), Some(open_mind(&home, empty.clone())));
    let mut a = Client::connect(&d.info, None);
    let before = stats(&mut a);
    assert!(matches!(before.models, marlowe_proto::ModelsState::Missing { .. }), "{:?}", before.models);
    // The answer is broadcast, so every window's memory line follows.
    a.send(&Request::ModelsDecline);
    let said = a.until(|e| matches!(e, Event::MemoryStats(s) if s.models == marlowe_proto::ModelsState::Declined));
    assert!(!said.is_empty());
    assert_eq!(stats(&mut a).models, marlowe_proto::ModelsState::Declined);
    assert!(home.join("onboarding.json").is_file(), "kept beside user.json");
    drop(a);
    stop(d, &home);

    let d = start_with(&home, Arc::new(Scripted::default()), mem_cfg(), Some(open_mind(&home, empty)));
    let mut b = Client::connect(&d.info, None);
    let after = stats(&mut b);
    assert_eq!(after.models, marlowe_proto::ModelsState::Declined, "not asked again");
    assert!(after.degraded.is_some(), "still keyword-only");
    stop(d, &home);
}

#[test]
fn an_evicted_session_is_written_up_as_an_episode() {
    let home = temp_home("episode");
    seed_model(&home);
    let empty = home.join("no-models");
    std::fs::create_dir_all(&empty).unwrap();
    let p = Arc::new(Scripted::default());
    p.push(vec![text("Will do: the gate ablation drops one reranker feature per run.")]);
    // Anything more would be a model call the episode must not make.
    p.push(vec![text("MODEL-WRITTEN SUMMARY")]);
    let mind = open_mind(&home, empty);
    let cfg = DaemonConfig { session_idle: Duration::from_millis(100), ..mem_cfg() };
    let d = start_with(&home, p.clone(), cfg, Some(mind.clone()));
    let mut a = Client::connect(&d.info, None);
    a.send(&Request::Ask { session: None, text: "plan the gate ablation".into(), workspace: None, images: Vec::new(), options: None, client_ms: None });
    let sid = opened_session(&a.until(is_finished));

    let deadline = Instant::now() + WAIT;
    let episode = loop {
        let eps = mind.episodes(0..i64::MAX).unwrap();
        if let Some(e) = eps.into_iter().find(|e| e.session == sid.as_str()) {
            break e;
        }
        assert!(Instant::now() < deadline, "no episode was written");
        std::thread::sleep(Duration::from_millis(50));
    };
    // Extractive: the opening request is the title, the day is in words, and no model wrote it.
    assert!(episode.summary.starts_with("Session on "), "{episode:?}");
    assert!(episode.summary.contains(": plan the gate ablation"), "{episode:?}");
    assert!(!episode.summary.contains("MODEL-WRITTEN"), "{episode:?}");
    assert_eq!(p.requests().len(), 1, "the turn was the only model call");
    stop(d, &home);
}

/// The ui-preview5 library that loaded valiance behind Matthew's back: valiance added first,
/// dawn second, conductor and quick on dawn, reflection and crew:general unset. A turn, an
/// eviction and its episode, and a brief call the model once, on dawn.
#[test]
fn the_preview_library_never_calls_valiance_in_the_background() {
    let home = temp_home("preview-valiance");
    {
        let lib = Library::open(&paths::cowork_db(&home)).unwrap();
        for id in ["marlowe-valiance:22b", "marlowe-dawn:9b"] {
            let mut info = ModelInfo::bare(ProviderKind::Ollama, id);
            info.supports_tools = true;
            info.context_window = Some(32_768);
            lib.add(&info).unwrap();
            std::thread::sleep(Duration::from_millis(5));
        }
        let dawn: ModelRef = "ollama/marlowe-dawn:9b".parse().unwrap();
        lib.set_slot(&Slot::Conductor, Some(&dawn), Effort::Medium).unwrap();
        lib.set_slot(&Slot::Quick, Some(&dawn), Effort::Low).unwrap();
        lib.set_slot(&Slot::Reflection, None, Effort::Low).unwrap();
        lib.set_slot(&Slot::Crew("general".into()), None, Effort::Medium).unwrap();
        assert_eq!(lib.default_model().unwrap().map(|m| m.model_id), Some("marlowe-valiance:22b".into()));
    }
    let p = Arc::new(Scripted::default());
    p.push(vec![text("The gate ablation drops one reranker feature per run.")]);
    p.push(vec![text("a second call would be the background")]);
    let mind = open_mind(&home, no_models(&home));
    let cfg = DaemonConfig { session_idle: Duration::from_millis(100), local_polish: true, ..mem_cfg() };
    let d = start_with(&home, p.clone(), cfg, Some(mind.clone()));
    let mut a = Client::connect(&d.info, None);
    a.send(&Request::Ask { session: None, text: "plan the gate ablation".into(), workspace: None, images: Vec::new(), options: None, client_ms: None });
    let sid = opened_session(&a.until(is_finished));
    let deadline = Instant::now() + WAIT;
    while !mind.episodes(0..i64::MAX).unwrap().iter().any(|e| e.session == sid.as_str()) {
        assert!(Instant::now() < deadline, "no episode was written");
        std::thread::sleep(Duration::from_millis(50));
    }
    a.request(&Request::Brief, |e| matches!(e, Event::Brief { .. }));
    let models: Vec<String> = p.requests().iter().map(|r| r.model.clone()).collect();
    assert_eq!(models, vec!["marlowe-dawn:9b".to_string()], "only the turn called a model, on the conductor's");
    stop(d, &home);
}

// ---------------------------------------------------------------- memory trust

fn no_models(home: &Path) -> PathBuf {
    let empty = home.join("no-models");
    std::fs::create_dir_all(&empty).unwrap();
    empty
}

fn tool_summary(log: &[Envelope], id: &str) -> Option<String> {
    log.iter().find_map(|e| match &e.event {
        Event::ToolFinished { call_id, summary, .. } if call_id == id => Some(summary.clone()),
        _ => None,
    })
}

/// Writes an External (untrusted) belief dated past the maturation window straight into the
/// closed profile, so it is an injection candidate when the mind reopens.
fn plant_matured_untrusted(home: &Path, text: &str) -> String {
    use marlowe_memory::{Author, BeliefKind, BeliefStore, BeliefWrite, MaturationPolicy, Tags};
    let p = marlowe_journal::Profile::open(&marlowe_mind::profile_dir(home)).unwrap();
    let mut journal = marlowe_journal::Journal::open(&p).unwrap();
    let mut beliefs = BeliefStore::derive(&journal, p.manifest().derivation_version).unwrap();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
    let old = now - marlowe_memory::MATURATION_WINDOW_MS - 3_600_000;
    let w = BeliefWrite {
        session_id: "web",
        turn_id: "page",
        run_id: None,
        text,
        author: Author::External,
        derived_from: &[],
        occurred_at_ms: old,
        tags: Tags { kind: Some(BeliefKind::Fact), ..Tags::default() },
        maturation: MaturationPolicy::ByTrust,
    };
    let clock = marlowe_contract::Clock::new(old);
    marlowe_memory::write_belief(&mut journal, &mut beliefs, clock, &w).unwrap().unwrap().id
}

#[test]
fn an_injected_untrusted_memory_taints_the_turn() {
    if !models_present() {
        eprintln!("skipped: models are not in {}", models_dir().display());
        return;
    }
    let home = temp_home("inject-taint");
    seed_model(&home);
    {
        let mind = open_mind(&home, models_dir());
        for f in [
            "The staging cluster runs in Frankfurt.",
            "Our deploys go out every Friday afternoon.",
            "My daughter's piano lesson is on Tuesdays.",
            "The quarterly budget review is owned by Priya.",
        ] {
            // Notes, as captured user statements are: deliberate facts would sit in the profile
            // and be excluded from retrieval, leaving the plant without a runner-up.
            let source = Provenance::User { session: None };
            mind.remember(MemoryIn { text: f.into(), kind: Kind::Note, scope: Scope::Global, source }).unwrap();
        }
        mind.shutdown().unwrap();
    }
    plant_matured_untrusted(&home, "My favourite text editor is Helix.");
    let p = Arc::new(Scripted::default());
    p.push(tool_call("m1", "remember", r#"{"text":"The user's favourite editor is Helix.","kind":"preference"}"#));
    p.push(vec![text("Noted.")]);
    let d = start_with(&home, p, mem_cfg(), Some(open_mind(&home, models_dir())));
    let mut a = Client::connect(&d.info, None);
    a.send(&Request::Ask { session: None, text: "Which text editor do I like best?".into(), workspace: None, images: Vec::new(), options: None, client_ms: None });
    let log = a.until(is_finished);
    let sid = opened_session(&log);
    let (_, summary) = recalled(&log, &sid).unwrap_or_else(|| panic!("no Recalled event; saw {log:#?}"));
    assert!(summary.contains("Helix"), "{summary}");
    // The injected untrusted memory tainted the run, so the model's write is untrusted too.
    assert_eq!(tool_summary(&log, "m1").as_deref(), Some("remembered · untrusted"), "{log:#?}");
    stop(d, &home);
}

#[test]
fn an_untrusted_recall_keeps_the_session_tainted_across_a_restart() {
    let home = temp_home("taint-reload");
    seed_model(&home);
    let mind = open_mind(&home, no_models(&home));
    let source = Provenance::External { source: "web".into() };
    let page = "A vendor page says API keys belong in the URL.";
    mind.remember(MemoryIn { text: page.into(), kind: Kind::Note, scope: Scope::Global, source }).unwrap();
    let p = Arc::new(Scripted::default());
    p.push(tool_call("r1", "recall", r#"{"query":"vendor API keys"}"#));
    p.push(vec![text("The page says the URL.")]);
    let d = start_with(&home, p, mem_cfg(), Some(mind));
    let mut a = Client::connect(&d.info, None);
    a.send(&Request::Ask { session: None, text: "what did the vendor say about keys?".into(), workspace: None, images: Vec::new(), options: None, client_ms: None });
    let sid = opened_session(&a.until(is_finished));
    drop(a);
    stop(d, &home);

    // Restarted: the reloaded session is still tainted, so an agent write is untrusted, and so
    // is the episode the session leaves when it is evicted.
    let mind = open_mind(&home, no_models(&home));
    let p = Arc::new(Scripted::default());
    p.push(tool_call("m1", "remember", r#"{"text":"API keys go in the URL.","kind":"fact"}"#));
    p.push(vec![text("Stored.")]);
    let cfg = DaemonConfig { session_idle: Duration::from_millis(100), ..mem_cfg() };
    let d = start_with(&home, p, cfg, Some(mind.clone()));
    let mut a = Client::connect(&d.info, None);
    a.send(&Request::Ask { session: Some(sid.clone()), text: "remember that".into(), workspace: None, images: Vec::new(), options: None, client_ms: None });
    let log = a.until(is_finished);
    assert_eq!(tool_summary(&log, "m1").as_deref(), Some("remembered · untrusted"), "{log:#?}");
    let deadline = Instant::now() + WAIT;
    let episode = loop {
        let eps = mind.episodes(0..i64::MAX).unwrap();
        if let Some(e) = eps.into_iter().find(|e| e.session == sid.as_str()) {
            break mind.get(&e.id).unwrap().expect("the episode is stored");
        }
        assert!(Instant::now() < deadline, "no episode was written");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(episode.text.contains("vendor"), "{episode:?}");
    assert_eq!(episode.trust, marlowe_proto::MemoryTrust::Untrusted, "{episode:?}");
    stop(d, &home);
}

#[test]
fn forgetting_what_the_user_said_asks_the_user_first() {
    let home = temp_home("forget-asks");
    seed_model(&home);
    let mind = open_mind(&home, no_models(&home));
    let source = Provenance::User { session: None };
    let id = mind.remember(MemoryIn { text: "My manager is Dana.".into(), kind: Kind::Fact, scope: Scope::Global, source }).unwrap().id;
    let args = format!(r#"{{"id":"{id}"}}"#);
    let p = Arc::new(Scripted::default());
    p.push(tool_call("f1", "forget", &args));
    p.push(vec![text("Kept it.")]);
    p.push(tool_call("f2", "forget", &args));
    p.push(vec![text("Forgotten.")]);
    let d = start_with(&home, p, mem_cfg(), Some(mind.clone()));
    let mut a = Client::connect(&d.info, None);

    let ask_and_answer = |a: &mut Client, session: Option<SessionId>, decision: ApprovalDecision| {
        a.send(&Request::Ask { session, text: "forget who my manager is".into(), workspace: None, images: Vec::new(), options: None, client_ms: None });
        let asked = a.until(|e| matches!(e, Event::ApprovalRequested { .. }));
        let sid = opened_session(&asked);
        match &asked.last().unwrap().event {
            Event::ApprovalRequested { approval, tool, summary, .. } => {
                assert_eq!(tool, "forget");
                assert!(summary.starts_with("forget a memory from you: My manager is Dana"), "{summary}");
                a.send(&Request::Approve { approval: *approval, decision, reason: None });
            }
            _ => unreachable!(),
        }
        (sid, a.until(is_finished))
    };
    let (sid, log) = ask_and_answer(&mut a, None, ApprovalDecision::Deny);
    assert!(log.iter().any(|e| matches!(&e.event, Event::ToolFinished { ok: false, call_id, .. } if call_id == "f1")));
    assert!(mind.get(&id).unwrap().is_some(), "denied: the memory stays");
    let (_, log) = ask_and_answer(&mut a, Some(sid), ApprovalDecision::Allow);
    let forgotten = |e: &Envelope| matches!(&e.event, Event::MemoryChanged { id: x, change: marlowe_proto::MemoryChange::Forgotten } if *x == id);
    assert!(log.iter().any(forgotten), "{log:#?}");
    assert!(mind.get(&id).unwrap().is_none(), "allowed: forgotten with the user's authority");
    stop(d, &home);
}

#[test]
fn a_denial_with_a_reason_tells_the_model_what_the_user_said() {
    let home = temp_home("deny-why");
    seed_model(&home);
    let mind = open_mind(&home, no_models(&home));
    let source = Provenance::User { session: None };
    let id = mind.remember(MemoryIn { text: "My manager is Dana.".into(), kind: Kind::Fact, scope: Scope::Global, source }).unwrap().id;
    let p = Arc::new(Scripted::default());
    p.push(tool_call("f1", "forget", &format!(r#"{{"id":"{id}"}}"#)));
    p.push(vec![text("Kept it.")]);
    let d = start_with(&home, p.clone(), mem_cfg(), Some(mind.clone()));
    let mut a = Client::connect(&d.info, None);
    a.send(&Request::Ask { session: None, text: "forget who my manager is".into(), workspace: None, images: Vec::new(), options: None, client_ms: None });
    let asked = a.until(|e| matches!(e, Event::ApprovalRequested { .. }));
    let Event::ApprovalRequested { approval, .. } = asked.last().unwrap().event.clone() else { unreachable!() };
    a.send(&Request::Approve { approval, decision: ApprovalDecision::Deny, reason: Some("keep it, I only asked what it was".into()) });
    a.until(is_finished);
    let second = &p.requests()[1];
    let seen: Vec<String> = second.messages.iter().flat_map(|m| m.tool_results_iter().map(|t| t.content.clone())).collect();
    assert_eq!(
        seen,
        vec!["The user denied this call and said: \"keep it, I only asked what it was\". Do what they said; do not retry the call unless they ask.".to_string()]
    );
    assert!(mind.get(&id).unwrap().is_some());
    stop(d, &home);
}

// ---------------------------------------------------------------- profile (M2.6)

/// The profile block of a request, if it carries one: it rides in the tail of the turn that first
/// showed it (the system prompt never holds it, so a memory write cannot break the prompt cache).
fn profile_block(req: &ChatRequest) -> Option<String> {
    req.messages.iter().map(|m| m.joined_text()).find_map(|t| {
        let from = t.find("<profile>\n")?;
        let to = t[from..].find("</profile>")? + from + "</profile>".len();
        Some(t[from..to].to_string())
    })
}

#[test]
fn parity_c5_c16_a_remembered_preference_is_in_the_first_request_of_the_next_session_after_a_restart() {
    let home = temp_home("profile-restart");
    seed_model(&home);
    let p = Arc::new(Scripted::default());
    p.push(tool_call("m1", "remember", r#"{"text":"use uv, not pip","kind":"preference"}"#));
    p.push(vec![text("Understood.")]);
    let d = start_with(&home, p.clone(), mem_cfg(), Some(open_mind(&home, no_models(&home))));
    let mut a = Client::connect(&d.info, None);
    a.send(&Request::Ask { session: None, text: "I always use uv, never pip.".into(), workspace: None, images: Vec::new(), options: None, client_ms: None });
    let log = a.until(is_finished);
    let sid_a = opened_session(&log);
    assert_eq!(tool_summary(&log, "m1").as_deref(), Some("remembered · inferred"), "{log:#?}");
    let first = &p.requests()[0];
    assert!(profile_block(first).is_none(), "nothing recorded yet");
    assert!(first.system_text().contains("<memory_tools>"), "the memory block is live with a Mind");
    let remember = first.tools.iter().find(|t| t.name == "remember").expect("remember is exposed");
    assert_eq!(remember.input_schema["required"], serde_json::json!(["text", "kind"]));
    drop(a);
    stop(d, &home);

    let p2 = Arc::new(Scripted::default());
    p2.push(vec![text("With uv.")]);
    p2.push(vec![text("Still uv.")]);
    let d = start_with(&home, p2.clone(), mem_cfg(), Some(open_mind(&home, no_models(&home))));
    let mut b = Client::connect(&d.info, None);
    b.send(&Request::Ask { session: None, text: "How should I install the deps?".into(), workspace: None, images: Vec::new(), options: None, client_ms: None });
    let sid_b = opened_session(&b.until(is_finished));
    assert_ne!(sid_a, sid_b);
    let reqs = p2.requests();
    let block = profile_block(&reqs[0]).unwrap_or_else(|| panic!("no profile block:\n{}", whole(&reqs[0])));
    assert!(block.lines().any(|l| l.starts_with("- preference \u{b7} ") && l.ends_with(": use uv, not pip")), "{block}");
    assert!(!reqs[0].system_text().contains("<profile>\n"), "the profile is not in the system prompt");

    // A second turn with no memory change sends a byte-identical block.
    b.send(&Request::Ask { session: Some(sid_b), text: "And for the tests?".into(), workspace: None, images: Vec::new(), options: None, client_ms: None });
    b.until(is_finished);
    let reqs = p2.requests();
    assert_eq!(reqs.len(), 2);
    assert_eq!(profile_block(&reqs[1]).unwrap().as_bytes(), block.as_bytes());

    // The Memory view's profile request lists the same item.
    match b.request(&Request::MemoryProfile { workspace: None }, |e| matches!(e, Event::MemoryProfile { .. })) {
        Event::MemoryProfile { hits, tokens, omitted } => {
            assert_eq!(hits.len(), 1, "{hits:?}");
            assert_eq!((hits[0].text.as_str(), hits[0].kind), ("use uv, not pip", marlowe_proto::MemoryKind::Preference));
            assert!(tokens > 0 && omitted == 0);
        }
        _ => unreachable!(),
    }
    stop(d, &home);
}

#[test]
fn a_tainted_session_remember_stays_out_of_the_profile_and_no_mind_means_no_memory_block() {
    let home = temp_home("profile-taint");
    seed_model(&home);
    let mind = open_mind(&home, no_models(&home));
    let source = Provenance::External { source: "web".into() };
    mind.remember(MemoryIn { text: "A forum says to pin numpy 1.x.".into(), kind: Kind::Note, scope: Scope::Global, source })
        .unwrap();
    let p = Arc::new(Scripted::default());
    p.push(tool_call("r1", "recall", r#"{"query":"numpy pin"}"#));
    p.push(tool_call("m1", "remember", r#"{"text":"pin numpy to 1.x","kind":"preference"}"#));
    p.push(vec![text("Noted.")]);
    p.push(vec![text("Hello.")]);
    let d = start_with(&home, p.clone(), mem_cfg(), Some(mind.clone()));
    let mut a = Client::connect(&d.info, None);
    a.send(&Request::Ask { session: None, text: "what does the forum say about numpy?".into(), workspace: None, images: Vec::new(), options: None, client_ms: None });
    let log = a.until(is_finished);
    assert_eq!(tool_summary(&log, "m1").as_deref(), Some("remembered · untrusted"), "{log:#?}");
    // A new session: the tainted write is not in its context.
    a.send(&Request::Ask { session: None, text: "hello".into(), workspace: None, images: Vec::new(), options: None, client_ms: None });
    a.until(is_finished);
    let reqs = p.requests();
    let last = reqs.last().unwrap();
    assert!(profile_block(last).is_none(), "{}", last.system_text());
    assert!(!last.system_text().contains("numpy"));
    assert!(mind.profile(marlowe_mind::ProfileQuery::default()).unwrap().ids.is_empty());
    drop(a);
    stop(d, &home);

    // Without a Mind there are no memory tools, so the block that describes them is absent.
    let home = temp_home("profile-nomind");
    seed_model(&home);
    let p = Arc::new(Scripted::default());
    let d = start(&home, p.clone(), mem_cfg());
    let mut a = Client::connect(&d.info, None);
    a.send(&Request::Ask { session: None, text: "hi".into(), workspace: None, images: Vec::new(), options: None, client_ms: None });
    a.until(is_finished);
    let req = &p.requests()[0];
    assert!(!req.system_text().contains("<memory_tools>") && profile_block(req).is_none());
    stop(d, &home);
}

#[test]
fn retrieval_in_a_turn_never_repeats_a_profile_item() {
    if !models_present() {
        eprintln!("skipped: models are not in {}", models_dir().display());
        return;
    }
    let home = temp_home("profile-exclude");
    seed_model(&home);
    let mind = open_mind(&home, models_dir());
    let mut profile_ids = Vec::new();
    for f in ["My favourite text editor is Helix.", "The staging cluster runs in Frankfurt.", "Our deploys go out on Fridays."] {
        let source = Provenance::User { session: None };
        profile_ids.push(mind.remember(MemoryIn { text: f.into(), kind: Kind::Fact, scope: Scope::Global, source }).unwrap().id);
    }
    let p = Arc::new(Scripted::default());
    p.push(vec![text("Noted.")]);
    p.push(vec![text("Noted.")]);
    p.push(vec![text("Helix.")]);
    let d = start_with(&home, p.clone(), mem_cfg(), Some(mind.clone()));
    let mut a = Client::connect(&d.info, None);
    for said in ["I mostly edit code in Helix these days.", "My daughter's piano lesson is on Tuesdays."] {
        a.send(&Request::Ask { session: None, text: said.into(), workspace: None, images: Vec::new(), options: None, client_ms: None });
        a.until(is_finished);
    }
    a.send(&Request::Ask { session: None, text: "Which text editor do I like best?".into(), workspace: None, images: Vec::new(), options: None, client_ms: None });
    a.until(is_finished);
    let req = p.requests().pop().unwrap();
    let block = profile_block(&req).expect("the profile is in context");
    assert!(block.contains("Helix"), "{block}");
    for id in &profile_ids {
        assert!(!whole(&req).contains(&format!("[Memory {id} ")), "{id} injected twice:\n{}", whole(&req));
    }
    stop(d, &home);
}

#[test]
fn a_workspace_that_does_not_exist_is_refused_and_a_real_one_is_stored_canonical() {
    let home = temp_home("ws-normal");
    seed_model(&home);
    let p = Arc::new(Scripted::default());
    p.push(vec![text("ok"), usage(), Step::Ev(StreamEvent::Stop(StopReason::EndTurn))]);
    let d = start(&home, p.clone(), DaemonConfig::default());
    let mut c = Client::connect(&d.info, None);

    let gone = home.join("no-such-folder");
    c.send(&Request::Ask { session: None, text: "hi".into(), workspace: Some(gone.display().to_string()), images: Vec::new(), options: None, client_ms: None });
    let got = c.until(|e| matches!(e, Event::Error { .. }));
    let msg = got.iter().find_map(|e| match &e.event {
        Event::Error { message, .. } => Some(message.clone()),
        _ => None,
    });
    assert!(msg.as_deref().is_some_and(|m| m.contains("workspace") && m.contains("does not exist")), "{msg:?}");
    match c.request(&Request::ListSessions { limit: 10 }, |e| matches!(e, Event::SessionList { .. })) {
        Event::SessionList { sessions } => assert!(sessions.is_empty(), "no session was made for a missing folder"),
        _ => unreachable!(),
    }

    // A real folder in a roundabout spelling is stored as the canonical path.
    let roundabout = format!("{}/./", home.display());
    c.send(&Request::Ask { session: None, text: "hi".into(), workspace: Some(roundabout), images: Vec::new(), options: None, client_ms: None });
    let got = c.until(is_finished);
    let stored = got.iter().find_map(|e| match &e.event {
        Event::SessionOpened { workspace, .. } => workspace.clone(),
        _ => None,
    });
    assert_eq!(stored.as_deref(), marlowe_proto::workspace::normalise(&home.display().to_string()).unwrap().to_str());
    stop(d, &home);
}
