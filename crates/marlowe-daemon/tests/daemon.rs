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
use marlowe_model::{
    CancelToken, ChatRequest, Completion, CompletionBuilder, Message, ModelInfo, Part, Provider, ProviderError,
    ProviderKind, Role, StopReason, StreamEvent, ToolSpec, Usage,
};
use marlowe_proto::{
    paths, write_line, ApprovalDecision, DaemonInfo, Envelope, Event, LineReader, Request, SessionId, TurnStop,
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
    let mut providers = ProviderFactory::new(Arc::new(MemoryStore::new()));
    providers.register(ProviderKind::Ollama, move |_| Ok(provider.clone() as Arc<dyn Provider>));
    DaemonDeps {
        providers,
        tools: Arc::new(|_| Arc::new(FakeTools) as Arc<dyn ToolHost>),
        user: UserInfo { name: Some("Tester".into()), address: None },
    }
}

struct Running {
    info: DaemonInfo,
    join: JoinHandle<Result<(), DaemonError>>,
}

fn start(home: &Path, provider: Arc<Scripted>, cfg: DaemonConfig) -> Running {
    let d = Daemon::start(deps(provider), home, cfg).expect("daemon starts");
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
        let w = TcpStream::connect(info.addr()).unwrap();
        w.set_read_timeout(Some(WAIT)).unwrap();
        let mut c = Client { r: LineReader::new(BufReader::new(w.try_clone().unwrap())), w, welcome_seq: 0 };
        c.send(&Request::Hello { token: info.token.clone(), client: "test".into(), since_seq });
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

fn texts(m: &[Message], role: Role) -> Vec<String> {
    m.iter().filter(|m| m.role == role).map(|m| m.joined_text()).collect()
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
    a.send(&Request::Ask { session: None, text: "hello there\nsecond line".into(), workspace: Some("C:/ws".into()) });
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
        if title == "hello there" && workspace.as_deref() == Some("C:/ws"))));
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

    a.send(&Request::Ask { session: None, text: "one".into(), workspace: None });
    let head = a.until(|e| matches!(e, Event::TextDelta { text, .. } if text == "first"));
    let sid = opened_session(&head);
    a.send(&Request::Ask { session: Some(sid.clone()), text: "two".into(), workspace: None });
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
    a.send(&Request::Ask { session: Some(sid.clone()), text: "three".into(), workspace: None });
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
    a.send(&Request::Ask { session: None, text: "remember me".into(), workspace: None });
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
    a.send(&Request::Ask { session: Some(sid.clone()), text: "still there?".into(), workspace: None });
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
    a.send(&Request::Ask { session: None, text: "one".into(), workspace: None });
    let sid = opened_session(&a.until(is_finished));
    std::thread::sleep(Duration::from_millis(300));
    a.send(&Request::Ask { session: Some(sid.clone()), text: "two".into(), workspace: None });
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
    fast.send(&Request::Ask { session: None, text: "flood".into(), workspace: None });
    let log = fast.until(is_finished);
    let deltas = log.iter().filter(|e| matches!(e.event, Event::TextDelta { .. })).count();
    assert_eq!(deltas, 6_000, "the reading client got everything");
    match fast.request(&Request::Status, |e| matches!(e, Event::Status(_))) {
        Event::Status(s) => assert_eq!(s.clients, 1, "the slow client was dropped"),
        _ => unreachable!(),
    }
    // Still serving.
    fast.send(&Request::Ask { session: None, text: "after".into(), workspace: None });
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

    a.send(&Request::Ask { session: None, text: "do it".into(), workspace: None });
    let asked = b.until(|e| matches!(e, Event::ApprovalRequested { .. }));
    let (approval, sid) = match &asked.last().unwrap().event {
        Event::ApprovalRequested { approval, session, tool, summary, .. } => {
            assert_eq!((tool.as_str(), summary.as_str()), ("danger", "do something dangerous"));
            (*approval, session.clone())
        }
        _ => unreachable!(),
    };
    // Any client may answer; an unknown id is refused.
    b.send(&Request::Approve { approval: marlowe_proto::ApprovalId(9_999), decision: ApprovalDecision::Allow });
    b.until(|e| matches!(e, Event::Error { message, .. } if message.contains("not pending")));
    b.send(&Request::Approve { approval, decision: ApprovalDecision::Allow });
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
    a.send(&Request::Ask { session: Some(sid), text: "again".into(), workspace: None });
    let log = a.until(is_finished);
    assert!(log.iter().any(|e| matches!(e.event, Event::ApprovalResolved { decision: ApprovalDecision::Deny, .. })));
    assert!(log.iter().any(|e| matches!(&e.event, Event::ToolFinished { ok: false, call_id, .. } if call_id == "c2")));
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
    a.send(&Request::Ask { session: None, text: "after a pause".into(), workspace: None });
    let got = a.until(is_finished);
    assert!(matches!(got.last().map(|e| &e.event), Some(Event::TurnFinished { stop: TurnStop::Done, .. })));
    stop(d, &home);
}
