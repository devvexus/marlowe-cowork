//! Phase events end to end: a daemon with a scripted provider and injected delays; every
//! millisecond from Enter to the first token is inside a named span, the spans sum to the measured
//! time to first token, the timings persist with the turn, and Progress is never replayed. No
//! inference, no GPU.

use std::io::BufReader;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use marlowe_agent::{Consequence, ToolCallReq, ToolCtx, ToolHost, ToolOutcome, UserInfo};
use marlowe_brains::{Library, MemoryStore, ProviderFactory};
use marlowe_daemon::{Daemon, DaemonConfig, DaemonDeps, DaemonError};
use marlowe_mind::{Mind, MindConfig};
use marlowe_model::{
    CancelToken, ChatRequest, Completion, CompletionBuilder, ModelInfo, Provider, ProviderError, ProviderKind, StopReason,
    StreamEvent, ToolSpec, Usage,
};
use marlowe_proto::{
    paths, write_line, DaemonInfo, Envelope, Event, LineReader, PhaseEvent, PhaseKind, PhaseSpan, PhaseState, Request,
    SessionId, TurnNumbers,
};

/// Sleeps `before_first_frame` ms, then answers.
struct Slow {
    before_first_frame: u64,
    requests: Mutex<Vec<ChatRequest>>,
}

impl Provider for Slow {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Ollama
    }

    fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        let mut a = ModelInfo::bare(ProviderKind::Ollama, "scripted");
        a.supports_tools = true;
        a.context_window = Some(32_768);
        Ok(vec![a])
    }

    fn stream(&self, req: &ChatRequest, _cancel: &CancelToken, on: &mut dyn FnMut(StreamEvent)) -> Result<Completion, ProviderError> {
        self.requests.lock().unwrap().push(req.clone());
        std::thread::sleep(Duration::from_millis(self.before_first_frame));
        let mut b = CompletionBuilder::new();
        for e in [
            StreamEvent::TextDelta("Hel".into()),
            StreamEvent::TextDelta("lo".into()),
            StreamEvent::Usage(Usage { input: 10, output: 5, ..Default::default() }),
            StreamEvent::Stop(StopReason::EndTurn),
        ] {
            b.push(&e);
            on(e);
        }
        Ok(b.finish())
    }
}

struct FakeTools;

impl ToolHost for FakeTools {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec { name: "echo".into(), description: "echo".into(), input_schema: serde_json::json!({"type": "object"}) }]
    }
    fn call(&self, _call: &ToolCallReq, _ctx: &ToolCtx) -> ToolOutcome {
        ToolOutcome::ok("x", "x")
    }
    fn consequence(&self, _call: &ToolCallReq) -> Consequence {
        Consequence::Free
    }
}

fn temp_home(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let home = std::env::temp_dir().join(format!("marlowe-daemon-phases-{name}-{}-{nanos}", std::process::id()));
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

struct Running {
    info: DaemonInfo,
    join: JoinHandle<Result<(), DaemonError>>,
}

/// `setup_ms` is slept in the tool-host factory, which the first turn waits for.
fn start(home: &Path, provider: Arc<Slow>, setup_ms: u64, mind: Option<Mind>) -> Running {
    let mut providers = ProviderFactory::new(Arc::new(MemoryStore::new()));
    providers.register(ProviderKind::Ollama, move |_| Ok(provider.clone() as Arc<dyn Provider>));
    let deps = DaemonDeps {
        providers,
        tools: Arc::new(move |_| {
            std::thread::sleep(Duration::from_millis(setup_ms));
            Arc::new(FakeTools) as Arc<dyn ToolHost>
        }),
        user: UserInfo { name: Some("Tester".into()), address: None },
        mind,
        sentinel: None,
        voice: None,
        crew: None,
    };
    let d = Daemon::start(deps, home, DaemonConfig { legacy_offer: false, ..DaemonConfig::default() }).expect("daemon starts");
    let info = d.info();
    Running { info, join: std::thread::spawn(move || d.wait()) }
}

const WAIT: Duration = Duration::from_secs(30);

struct Client {
    w: TcpStream,
    r: LineReader<BufReader<TcpStream>>,
}

impl Client {
    fn connect(info: &DaemonInfo, since_seq: Option<u64>) -> Client {
        let w = TcpStream::connect(info.addr()).unwrap();
        w.set_read_timeout(Some(WAIT)).unwrap();
        let mut c = Client { r: LineReader::new(BufReader::new(w.try_clone().unwrap())), w };
        c.send(&Request::Hello { token: info.token.clone(), client: "test".into(), since_seq });
        let env = c.next();
        assert!(matches!(env.event, Event::Welcome { .. }));
        c
    }

    fn send(&mut self, r: &Request) {
        write_line(&mut self.w, r).unwrap();
    }

    fn next(&mut self) -> Envelope {
        self.r.next::<Envelope>().expect("read an envelope").expect("stream open")
    }

    fn until(&mut self, pred: impl Fn(&Event) -> bool) -> Vec<Envelope> {
        let deadline = Instant::now() + WAIT;
        let mut out = Vec::new();
        while Instant::now() < deadline {
            let env = self.next();
            let hit = pred(&env.event);
            out.push(env);
            if hit {
                return out;
            }
        }
        panic!("timed out; saw {out:#?}");
    }
}

fn stop(r: Running) {
    let mut c = Client::connect(&r.info, None);
    c.send(&Request::Shutdown);
    c.until(|e| matches!(e, Event::ShuttingDown));
    r.join.join().unwrap().unwrap();
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64
}

fn ask(c: &mut Client, session: Option<SessionId>, text: &str, client_ms: Option<i64>) {
    c.send(&Request::Ask { session, text: text.into(), workspace: None, images: Vec::new(), options: None, client_ms });
}

fn is_finished(e: &Event) -> bool {
    matches!(e, Event::TurnFinished { .. })
}

fn phases(log: &[Envelope]) -> Vec<&PhaseEvent> {
    log.iter().filter_map(|e| if let Event::Phase(p) = &e.event { Some(p) } else { None }).collect()
}

fn timings(log: &[Envelope]) -> (Vec<PhaseSpan>, TurnNumbers) {
    log.iter()
        .find_map(|e| match &e.event {
            Event::TurnTimings { spans, numbers, .. } => Some((spans.clone(), numbers.clone())),
            _ => None,
        })
        .expect("a TurnTimings event")
}

#[test]
fn enter_to_first_token_is_covered_by_named_spans_that_sum_to_the_ttft() {
    let home = temp_home("coverage");
    seed_model(&home);
    let provider = Arc::new(Slow { before_first_frame: 70, requests: Mutex::new(Vec::new()) });
    let d = start(&home, provider, 45, None);
    let mut a = Client::connect(&d.info, None);
    // Enter was pressed 6 ms before the ask reached the daemon.
    ask(&mut a, None, "hello", Some(now_ms() - 6));
    let log = a.until(is_finished);

    // TurnTimings comes just before TurnFinished.
    let n = log.len();
    assert!(matches!(log[n - 2].event, Event::TurnTimings { .. }), "{:?}", log[n - 2].event);
    let (spans, numbers) = timings(&log);
    let top: Vec<&PhaseSpan> = spans.iter().filter(|s| s.parent.is_none()).collect();
    let kinds: Vec<PhaseKind> = top.iter().map(|s| s.kind).collect();
    for k in [PhaseKind::Sending, PhaseKind::Setup, PhaseKind::Admit, PhaseKind::Prompt, PhaseKind::Recall, PhaseKind::Fit, PhaseKind::Wire] {
        assert!(kinds.contains(&k), "no {k:?} span in {kinds:?}");
    }
    let by = |k: PhaseKind| top.iter().filter(|s| s.kind == k).map(|s| s.ms).sum::<u64>();
    assert!(by(PhaseKind::Sending) >= 5, "sending {}", by(PhaseKind::Sending));
    assert!(by(PhaseKind::Setup) >= 40, "the slow tool factory is the session setup: {}", by(PhaseKind::Setup));
    assert!(by(PhaseKind::Wire) >= 65, "the slow first frame is the wire: {}", by(PhaseKind::Wire));

    // No gap longer than one frame without a span, from Enter to the end of the wire phase.
    let frame = 16;
    let mut sorted = top.clone();
    sorted.sort_by_key(|s| s.start_ms);
    assert!(sorted[0].start_ms <= frame, "the first span starts at Enter: {sorted:?}");
    let mut covered_to = 0u64;
    for s in &sorted {
        assert!(s.start_ms <= covered_to + frame, "gap before {:?}: covered to {covered_to}, next starts {}", s.kind, s.start_ms);
        covered_to = covered_to.max(s.start_ms + s.ms);
    }
    // The summary sums to the measured time to first token (rounding is a ms per span).
    let ttft = numbers.ttft_ms.expect("a measured TTFT");
    let sum: u64 = top.iter().filter(|s| matches!(s.kind, PhaseKind::Sending | PhaseKind::Setup | PhaseKind::Admit | PhaseKind::Prompt | PhaseKind::Recall | PhaseKind::Fit | PhaseKind::Compact | PhaseKind::Wire)).map(|s| s.ms).sum();
    let slack = top.len() as u64 + frame;
    assert!(sum <= ttft + slack && ttft <= sum + slack, "spans sum to {sum} ms, TTFT {ttft} ms: {top:?}");

    // Phase events carry the session and turn and arrive in the order the work ran.
    let ps = phases(&log);
    assert!(ps.iter().all(|p| p.session.is_some()));
    let first_start = |k: PhaseKind| ps.iter().position(|p| p.kind == k && p.state == PhaseState::Start).unwrap();
    assert!(first_start(PhaseKind::Setup) < first_start(PhaseKind::Admit));
    assert!(first_start(PhaseKind::Admit) < first_start(PhaseKind::Prompt));
    assert!(first_start(PhaseKind::Prompt) < first_start(PhaseKind::Wire));
    let first_text = log.iter().position(|e| matches!(e.event, Event::TextDelta { .. })).unwrap();
    let wire_end = log.iter().position(|e| matches!(&e.event, Event::Phase(p) if p.kind == PhaseKind::Wire && p.state == PhaseState::End)).unwrap();
    assert!(wire_end < first_text, "the wire phase ends before the first token is published");

    // Harness overhead is measured, and excludes what the model and the user spent.
    let e2w = numbers.enter_to_wire_ms.expect("enter to wire");
    assert!(e2w >= 45 && e2w < 45 + 6 + 100, "the setup delay is overhead, the 70 ms first frame is not: {e2w}");
    assert!(numbers.token_to_bus_ms.is_some());

    // The timings persist with the turn: opening the session replays them on its first message.
    let sid = log.iter().find_map(|e| if let Event::TurnStarted { session, .. } = &e.event { Some(session.clone()) } else { None }).unwrap();
    a.send(&Request::OpenSession { session: sid });
    let t = a.until(|e| matches!(e, Event::Transcript { .. }));
    let Event::Transcript { turns, .. } = &t.last().unwrap().event else { unreachable!() };
    assert_eq!(turns[0].phases, spans);
    assert_eq!(turns[0].numbers.as_ref(), Some(&numbers));
    assert!(turns[1..].iter().all(|t| t.phases.is_empty() && t.numbers.is_none()), "only the turn's first message carries them");
    stop(d);
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn harness_overhead_with_nothing_slow_is_small() {
    let home = temp_home("overhead");
    seed_model(&home);
    let provider = Arc::new(Slow { before_first_frame: 0, requests: Mutex::new(Vec::new()) });
    let d = start(&home, provider, 0, None);
    let mut a = Client::connect(&d.info, None);
    ask(&mut a, None, "warm up", None);
    let log = a.until(is_finished);
    let sid = log.iter().find_map(|e| if let Event::TurnStarted { session, .. } = &e.event { Some(session.clone()) } else { None }).unwrap();
    let mut worst = (0, 0);
    for i in 0..5 {
        ask(&mut a, Some(sid.clone()), &format!("again {i}"), Some(now_ms()));
        let log = a.until(is_finished);
        let (_, n) = timings(&log);
        let (e2w, t2b) = (n.enter_to_wire_ms.unwrap(), n.token_to_bus_ms.unwrap());
        eprintln!("harness overhead: enter->wire {e2w} ms, token->bus {t2b} ms, ttft {} ms", n.ttft_ms.unwrap());
        worst = (worst.0.max(e2w), worst.1.max(t2b));
    }
    // Single-digit on an idle machine; the bound leaves room for a busy one.
    assert!(worst.0 < 60 && worst.1 < 25, "overhead {worst:?}");
    stop(d);
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn an_abstained_recall_still_says_so_and_the_memory_tail_is_reported() {
    let home = temp_home("recall");
    seed_model(&home);
    // No models: memory is degraded, so recall abstains at once; the write tail still reports.
    let mind = Mind::open_with(&home, MindConfig { models_dir: Some(home.join("no-models-here")), ..MindConfig::default() }).unwrap();
    let provider = Arc::new(Slow { before_first_frame: 0, requests: Mutex::new(Vec::new()) });
    let d = start(&home, provider, 0, Some(mind));
    let mut a = Client::connect(&d.info, None);
    ask(&mut a, None, "I always use uv, never pip.", None);
    let log = a.until(is_finished);
    let recalled = log
        .iter()
        .find_map(|e| match &e.event {
            Event::Recalled { count, abstained, total_ms, .. } => Some((*count, *abstained, *total_ms)),
            _ => None,
        })
        .expect("Recalled is sent even when nothing was recalled");
    assert_eq!((recalled.0, recalled.1), (0, true));
    let ps = phases(&log);
    let stage = |s: &str| ps.iter().filter(|p| p.kind == PhaseKind::Recall && p.stage.as_deref() == Some(s)).count();
    assert!(stage("profile") == 2 && stage("ingest") == 2, "profile and ingest are recall stages with a start and an end");
    let (spans, _) = timings(&log);
    assert!(spans.iter().any(|s| s.kind == PhaseKind::Recall && s.stage.as_deref() == Some("profile") && s.parent == Some(PhaseKind::Recall)));
    assert!(spans.iter().any(|s| s.kind == PhaseKind::Recall && s.stage.is_none() && s.parent.is_none()));
    // The Mind's write tail: the turn's words were ingested.
    let tail = ps.iter().filter(|p| p.kind == PhaseKind::MemoryWrite && p.stage.as_deref() == Some("ingest")).count();
    assert!(tail >= 1, "ingest reported: {ps:?}");
    stop(d);
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn progress_is_live_only_and_never_replayed() {
    let home = temp_home("replay");
    seed_model(&home);
    let provider = Arc::new(Slow { before_first_frame: 0, requests: Mutex::new(Vec::new()) });
    let d = start(&home, provider, 0, None);
    let mut a = Client::connect(&d.info, None);
    ask(&mut a, None, "hi", None);
    a.until(is_finished);
    let mut late = Client::connect(&d.info, Some(0));
    // Ephemeral events have seq 0 and are not in the ring; every replayed phase is numbered.
    let mut seen = Vec::new();
    loop {
        let env = late.next();
        let done = is_finished(&env.event);
        seen.push(env);
        if done {
            break;
        }
    }
    assert!(seen.iter().filter(|e| matches!(e.event, Event::Phase(_))).all(|e| e.seq > 0), "a replayed phase is numbered");
    assert!(seen.iter().filter_map(|e| if let Event::Phase(p) = &e.event { Some(p) } else { None }).all(|p| p.state != PhaseState::Progress));
    assert!(seen.iter().any(|e| matches!(e.event, Event::Phase(_))), "start and end events are replayed");
    stop(d);
    let _ = std::fs::remove_dir_all(&home);
}
