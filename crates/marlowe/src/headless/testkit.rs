//! Test support for `marlowe -p`: a real in-process daemon with a scripted model and fake tools
//! ([`Rig`]), stdin that an SDK client writes while the run reads it ([`pipe`]), and a recording
//! fake daemon for what is about the flags themselves ([`Fake`]).

use std::collections::VecDeque;
use std::io::{BufRead, Read};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossbeam_channel::{unbounded, Receiver, Sender};
use marlowe_agent::{Consequence, ToolCallReq, ToolCtx, ToolHost, ToolOutcome, UserInfo};
use marlowe_brains::{Library, MemoryStore, ProviderFactory};
use marlowe_daemon::{Daemon, DaemonConfig, DaemonDeps};
use marlowe_model::{
    CancelToken, ChatRequest, Completion, CompletionBuilder, ModelInfo, Pricing, Provider, ProviderError, ProviderKind, StopReason, StreamEvent, ToolSpec, Usage,
};
use marlowe_proto::{
    paths, write_line, DaemonInfo, Envelope, Event, LibraryModel, LineReader, Request, SessionId, SessionSummary, SlotAssignment, StatusReport,
};
use serde_json::{json, Value};

use super::args::Args;
use super::driver::{self, Connector, Env};
use super::output::capture::Buf;

// ---------------------------------------------------------------- the scripted model

#[derive(Clone)]
pub enum Ev {
    Text(&'static str),
    Call(&'static str, &'static str, Value),
    Usage(u64, u64),
    /// Blocks until the turn is cancelled.
    WaitCancel,
    Sleep(u64),
}

#[derive(Default)]
pub struct Script {
    turns: Mutex<VecDeque<Vec<Ev>>>,
    requests: Mutex<Vec<ChatRequest>>,
}

impl Script {
    pub fn push(&self, evs: Vec<Ev>) {
        self.turns.lock().unwrap().push_back(evs);
    }

    pub fn requests(&self) -> Vec<ChatRequest> {
        self.requests.lock().unwrap().clone()
    }

    /// Every tool result the model has been shown, in order.
    pub fn tool_results(&self) -> Vec<String> {
        self.requests().last().map(|r| r.messages.iter().flat_map(|m| m.tool_results_iter().map(|t| t.content.clone()).collect::<Vec<_>>()).collect()).unwrap_or_default()
    }
}

impl Provider for Script {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Ollama
    }

    fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(vec![model_info()])
    }

    fn stream(&self, req: &ChatRequest, cancel: &CancelToken, on: &mut dyn FnMut(StreamEvent)) -> Result<Completion, ProviderError> {
        self.requests.lock().unwrap().push(req.clone());
        let evs = self.turns.lock().unwrap().pop_front().unwrap_or_else(|| vec![Ev::Text("ok"), Ev::Usage(10, 5)]);
        let mut b = CompletionBuilder::new();
        let mut emit = |e: StreamEvent, b: &mut CompletionBuilder| {
            b.push(&e);
            on(e);
        };
        let mut called = false;
        for (i, ev) in evs.into_iter().enumerate() {
            if cancel.is_cancelled() {
                return Err(ProviderError::cancelled());
            }
            match ev {
                Ev::Text(t) => emit(StreamEvent::TextDelta(t.into()), &mut b),
                Ev::Usage(i, o) => emit(StreamEvent::Usage(Usage { input: i, output: o, ..Default::default() }), &mut b),
                Ev::Call(id, name, args) => {
                    called = true;
                    emit(StreamEvent::ToolCallStart { index: i as u32, id: id.into(), name: name.into() }, &mut b);
                    emit(StreamEvent::ToolArgsDelta { index: i as u32, json: args.to_string() }, &mut b);
                    emit(StreamEvent::ToolCallEnd { index: i as u32 }, &mut b);
                }
                Ev::Sleep(ms) => std::thread::sleep(Duration::from_millis(ms)),
                Ev::WaitCancel => {
                    while !cancel.is_cancelled() {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    return Err(ProviderError::cancelled());
                }
            }
        }
        emit(StreamEvent::Stop(if called { StopReason::ToolUse } else { StopReason::EndTurn }), &mut b);
        Ok(b.finish())
    }
}

pub fn model_info() -> ModelInfo {
    let mut m = ModelInfo::bare(ProviderKind::Ollama, "scripted");
    m.supports_tools = true;
    m.context_window = Some(32_768);
    // $1 per million tokens each way, so a million tokens each way cost two dollars.
    m.pricing = Some(Pricing { input_per_mtok: 1.0, output_per_mtok: 1.0 });
    m
}

// ---------------------------------------------------------------- fake tools

pub struct Tools;

impl ToolHost for Tools {
    fn specs(&self) -> Vec<ToolSpec> {
        ["echo", "bash", "write", "read"]
            .iter()
            .map(|n| ToolSpec { name: n.to_string(), description: n.to_string(), input_schema: json!({"type": "object"}) })
            .collect()
    }

    fn call(&self, call: &ToolCallReq, _ctx: &ToolCtx) -> ToolOutcome {
        ToolOutcome::ok(format!("{} ran with {}", call.name, call.args), format!("{} ran", call.name))
    }

    fn consequence(&self, call: &ToolCallReq) -> Consequence {
        match call.name.as_str() {
            "bash" | "write" => Consequence::Ask { summary: format!("{} {}", call.name, call.args) },
            _ => Consequence::Free,
        }
    }
}

// ---------------------------------------------------------------- the rig

pub fn temp_dir(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let d = std::env::temp_dir().join(format!("marlowe-print-{name}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

pub struct Rig {
    pub home: PathBuf,
    pub ws: PathBuf,
    pub script: Arc<Script>,
    daemon: Option<Daemon>,
    pub info: DaemonInfo,
}

impl Rig {
    pub fn new(name: &str) -> Rig {
        Self::with_tools(name, Arc::new(|| Arc::new(Tools) as Arc<dyn ToolHost>))
    }

    pub fn with_tools(name: &str, tools: Arc<dyn Fn() -> Arc<dyn ToolHost> + Send + Sync>) -> Rig {
        let home = temp_dir(name);
        let ws = home.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let lib = Library::open(&paths::cowork_db(&home)).unwrap();
        lib.add(&model_info()).unwrap();
        drop(lib);
        let script = Arc::new(Script::default());
        let p = script.clone();
        let mut providers = ProviderFactory::new(Arc::new(MemoryStore::new()));
        providers.register(ProviderKind::Ollama, move |_| Ok(p.clone() as Arc<dyn Provider>));
        let deps = DaemonDeps {
            providers,
            tools: Arc::new(move |_| tools()),
            user: UserInfo { name: Some("Tester".into()), address: None },
            mind: None,
            sentinel: None,
            voice: None,
            crew: None,
        };
        let cfg = DaemonConfig { legacy_offer: false, ..DaemonConfig::default() };
        let daemon = Daemon::start(deps, &home, cfg).expect("daemon starts");
        let info = daemon.info();
        Rig { home, ws, script, daemon: Some(daemon), info }
    }

    pub fn connector(&self) -> Connector {
        let info = self.info.clone();
        Box::new(move || Ok((info, Box::new(|| {}) as Box<dyn FnOnce()>)))
    }

    /// The argument vector of `marlowe -p --workspace <ws> <argv>`.
    pub fn argv(&self, argv: &[&str]) -> Vec<String> {
        let mut v: Vec<String> = vec!["-p".into(), "--workspace".into(), self.ws.display().to_string()];
        v.extend(argv.iter().map(|s| s.to_string()));
        v
    }

    /// Runs `marlowe -p <argv>` with the workspace as its cwd and no stdin.
    pub fn print(&self, argv: &[&str]) -> Outcome {
        self.print_with(argv, None)
    }

    pub fn print_with(&self, argv: &[&str], stdin: Option<Box<dyn BufRead + Send>>) -> Outcome {
        let v = self.argv(argv);
        let args = Args::parse(&v).unwrap_or_else(|e| panic!("{v:?}: {e}"));
        run_args(args, &self.home, &self.ws, stdin, self.connector())
    }

    /// The session ids the daemon stores, newest first.
    pub fn sessions(&self) -> Vec<SessionSummary> {
        let mut c = RawClient::connect(&self.info);
        c.send(&Request::ListSessions { limit: 100 });
        match c.until(|e| matches!(e, Event::SessionList { .. })) {
            Event::SessionList { sessions } => sessions,
            _ => unreachable!(),
        }
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        if let Some(d) = self.daemon.take() {
            d.shutdown();
            let _ = d.wait();
        }
    }
}

pub struct Outcome {
    pub code: i32,
    pub out: Buf,
    pub err: Buf,
}

impl Outcome {
    pub fn lines(&self) -> Vec<Value> {
        self.out.lines()
    }

    pub fn result(&self) -> Value {
        self.lines().into_iter().rev().find(|l| l["type"] == "result").unwrap_or_else(|| panic!("no result line in {:?} / stderr {:?}", self.out.text(), self.err.text()))
    }

    pub fn denials(&self) -> usize {
        self.result()["permission_denials"].as_array().map_or(0, Vec::len)
    }
}

pub fn run_args(args: Args, home: &Path, cwd: &Path, stdin: Option<Box<dyn BufRead + Send>>, connect: Connector) -> Outcome {
    let (out, err) = (Buf::default(), Buf::default());
    run_args_to(args, home, cwd, stdin, connect, out, err)
}

pub fn run_args_to(args: Args, home: &Path, cwd: &Path, stdin: Option<Box<dyn BufRead + Send>>, connect: Connector, out: Buf, err: Buf) -> Outcome {
    let env = Env {
        home: home.to_path_buf(),
        cwd: cwd.to_path_buf(),
        stdin,
        out: Box::new(out.clone()),
        err: Box::new(err.clone()),
        interrupt: Arc::new(AtomicBool::new(false)),
    };
    let code = driver::run(args, env, connect);
    Outcome { code, out, err }
}

// ---------------------------------------------------------------- stdin an SDK client writes

struct PipeReader {
    rx: Receiver<Vec<u8>>,
    buf: Vec<u8>,
    pos: usize,
}

#[derive(Clone)]
pub struct PipeWriter(Sender<Vec<u8>>);

impl PipeWriter {
    pub fn line(&self, v: Value) {
        let _ = self.0.send(format!("{v}\n").into_bytes());
    }

    /// Closes the input: the run sees end of file once it has read what was written.
    pub fn close(self) {}
}

pub fn pipe() -> (PipeWriter, Box<dyn BufRead + Send>) {
    let (tx, rx) = unbounded();
    (PipeWriter(tx), Box::new(std::io::BufReader::new(PipeReader { rx, buf: Vec::new(), pos: 0 })))
}

impl Read for PipeReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.pos >= self.buf.len() {
            match self.rx.recv() {
                Ok(b) => {
                    self.buf = b;
                    self.pos = 0;
                }
                Err(_) => return Ok(0),
            }
        }
        let n = out.len().min(self.buf.len() - self.pos);
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

pub fn user(text: &str) -> Value {
    json!({"type": "user", "message": {"role": "user", "content": text}})
}

pub fn interrupt(id: &str) -> Value {
    json!({"type": "control_request", "request_id": id, "request": {"subtype": "interrupt"}})
}

/// Replaces what varies between runs (ids, times, paths) so a stream can be compared whole.
pub fn golden(lines: &[Value]) -> Vec<Value> {
    fn scrub(v: &mut Value) {
        match v {
            Value::Object(m) => {
                for (k, x) in m.iter_mut() {
                    match k.as_str() {
                        "session_id" | "cwd" => *x = json!(format!("<{k}>")),
                        "duration_ms" | "duration_api_ms" => *x = json!(0),
                        _ => scrub(x),
                    }
                }
            }
            Value::Array(a) => a.iter_mut().for_each(scrub),
            _ => {}
        }
    }
    lines
        .iter()
        .cloned()
        .map(|mut l| {
            scrub(&mut l);
            l
        })
        .collect()
}

// ---------------------------------------------------------------- a raw client of a real daemon

pub struct RawClient {
    w: TcpStream,
    r: LineReader<std::io::BufReader<TcpStream>>,
}

impl RawClient {
    pub fn connect(info: &DaemonInfo) -> RawClient {
        let w = TcpStream::connect(info.addr()).unwrap();
        w.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        let mut c = RawClient { r: LineReader::new(std::io::BufReader::new(w.try_clone().unwrap())), w };
        c.send(&Request::Hello { token: info.token.clone(), client: "test".into(), since_seq: None });
        let env: Envelope = c.r.next().unwrap().expect("welcome");
        assert!(matches!(env.event, Event::Welcome { .. }));
        c
    }

    pub fn send(&mut self, r: &Request) {
        write_line(&mut self.w, r).unwrap();
    }

    pub fn until(&mut self, pred: impl Fn(&Event) -> bool) -> Event {
        loop {
            let env: Envelope = self.r.next().unwrap().expect("stream open");
            if pred(&env.event) {
                return env.event;
            }
        }
    }
}

// ---------------------------------------------------------------- a recording fake daemon

type Handler = Box<dyn Fn(&Request) -> Vec<Event> + Send + Sync>;

/// A daemon that records every request and answers from a handler. By default it behaves like a
/// working one: a library with one model, new sessions, an `Ask` that answers "ok", and the
/// M4.6 requests another track owns answered with the seam's "not available yet" error.
pub struct Fake {
    pub info: DaemonInfo,
    requests: Arc<Mutex<Vec<Request>>>,
}

pub fn fake_session() -> SessionId {
    SessionId("fake-session".into())
}

pub fn default_reply(r: &Request) -> Vec<Event> {
    let sid = fake_session();
    match r {
        Request::LibraryState | Request::AddModel { .. } | Request::SetSlot { .. } | Request::RemoveModel { .. } => vec![Event::LibraryState {
            models: vec![LibraryModel { model: model_info().model_ref(), info: model_info(), added_ms: 0 }],
            slots: vec![SlotAssignment { slot: marlowe_proto::Slot::Conductor, model: Some(model_info().model_ref()), effort: Default::default(), inherited: true }],
            providers: Vec::new(),
        }],
        Request::NewSession { workspace } => vec![Event::SessionOpened { session: sid, title: String::new(), workspace: workspace.clone() }],
        Request::OpenSession { session } => vec![Event::SessionOpened { session: session.clone(), title: String::new(), workspace: None }],
        Request::ListSessions { .. } => vec![Event::SessionList { sessions: Vec::new() }],
        Request::McpAdd { .. } | Request::McpList { .. } | Request::McpRemove { .. } => vec![Event::McpServers { list: Vec::new() }],
        Request::Status => vec![Event::Status(StatusReport {
            daemon_version: "fake".into(),
            pid: 0,
            uptime_ms: 0,
            clients: 1,
            active_sessions: Vec::new(),
            seq: 0,
            profile_home: String::new(),
        })],
        Request::Ask { session, .. } => {
            let session = session.clone().unwrap_or(sid);
            vec![
                Event::TurnStarted { session: session.clone(), turn: 0, model: model_info().model_ref(), user: String::new() },
                Event::TextDelta { session: session.clone(), turn: 0, text: "ok".into() },
                Event::ContextUsage { session: session.clone(), turn: 0, tokens: 10, window: 32768 },
                Event::TurnFinished { session, turn: 0, usage: Default::default(), stop: marlowe_proto::TurnStop::Done },
            ]
        }
        Request::SwitchModel { .. } | Request::SetMode { .. } | Request::AddDir { .. } | Request::SetSetting { .. } => {
            vec![Event::Error { session: None, message: "`op` is not available yet (M4.6 track X)".into() }]
        }
        _ => Vec::new(),
    }
}

impl Fake {
    pub fn start() -> Fake {
        Self::with(default_reply)
    }

    /// Answers each request with `handler`'s events (replies carry seq 0, the rest are broadcasts).
    pub fn with(handler: impl Fn(&Request) -> Vec<Event> + Send + Sync + 'static) -> Fake {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let info = DaemonInfo { port: listener.local_addr().unwrap().port(), token: "fake-token".into(), pid: std::process::id() };
        let requests = Arc::new(Mutex::new(Vec::new()));
        let log = requests.clone();
        let handler: Arc<Handler> = Arc::new(Box::new(handler));
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                let (log, handler) = (log.clone(), handler.clone());
                std::thread::spawn(move || serve(stream, log, handler));
            }
        });
        Fake { info, requests }
    }

    pub fn connector(&self) -> Connector {
        let info = self.info.clone();
        Box::new(move || Ok((info, Box::new(|| {}) as Box<dyn FnOnce()>)))
    }

    pub fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }

    /// The first request of the kind `pick` selects.
    pub fn find<T>(&self, pick: impl Fn(&Request) -> Option<T>) -> Option<T> {
        self.requests().iter().find_map(pick)
    }

    pub fn ask(&self) -> Option<(Option<SessionId>, String, Option<marlowe_proto::HeadlessOptions>)> {
        self.find(|r| match r {
            Request::Ask { session, text, options, .. } => Some((session.clone(), text.clone(), options.clone())),
            _ => None,
        })
    }
}

fn serve(stream: TcpStream, log: Arc<Mutex<Vec<Request>>>, handler: Arc<Handler>) {
    let mut w = stream.try_clone().unwrap();
    let mut r = LineReader::new(std::io::BufReader::new(stream));
    let mut seq = 0u64;
    let reply = |w: &mut TcpStream, seq: u64, event: Event| {
        let _ = write_line(w, &Envelope { seq, at_ms: 0, event });
    };
    loop {
        let Ok(Some(req)) = r.next::<Request>() else { return };
        if let Request::Hello { .. } = req {
            reply(&mut w, 0, Event::Welcome { daemon_version: "fake".into(), proto_version: 1, profile_home: String::new(), seq: 0 });
            continue;
        }
        log.lock().unwrap().push(req.clone());
        for ev in handler(&req) {
            let broadcast = matches!(ev, Event::TurnStarted { .. } | Event::TextDelta { .. } | Event::ContextUsage { .. } | Event::TurnFinished { .. } | Event::ToolStarted { .. } | Event::ToolFinished { .. } | Event::ApprovalRequested { .. });
            if broadcast {
                seq += 1;
            }
            reply(&mut w, if broadcast { seq } else { 0 }, ev);
        }
    }
}
