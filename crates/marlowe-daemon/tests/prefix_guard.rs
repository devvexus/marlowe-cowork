//! The prefix guard: a local model keeps its prompt cache only while every request extends the
//! previous one byte for byte (qwen35 is hybrid: nothing but an exact prefix is reusable, and
//! Matthew's Ollama log showed the common prefix ending at token ~7,000 of 12,611).
//!
//! Two sessions of turns run through the real daemon, the real tools and the real Ollama request
//! builder with a scripted provider that records each body: a real git repository as the
//! workspace, a memory write, a file edit, a commit, a teams-mode toggle, a minute going by and a
//! daemon restart between turns. Every consecutive pair of requests is compared in its system
//! message, its tools in order, and its messages; the first byte that differs is reported with
//! the section that produced it.

use std::collections::VecDeque;
use std::io::BufReader;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use marlowe_agent::{Consequence, ToolCallReq, ToolCtx, ToolHost, ToolOutcome, UserInfo};
use marlowe_brains::{Library, MemoryStore, ProviderFactory};
use marlowe_daemon::{Daemon, DaemonConfig, DaemonDeps, DaemonError};
use marlowe_exec::specs::{self, HostSetup};
use marlowe_exec::BuiltinTools;
use marlowe_mind::{Kind, MemoryIn, Mind, MindConfig, Provenance, Scope};
use marlowe_model::{
    fold_events, CancelToken, ChatRequest, Completion, ModelInfo, Provider, ProviderError, ProviderKind, StopReason,
    StreamEvent, ToolSpec,
};
use marlowe_proto::{paths, write_line, DaemonInfo, Envelope, Event, LineReader, Request, SessionId};
use serde_json::Value;

const WAIT: Duration = Duration::from_secs(60);
const MODEL: &str = "marlowe-dusk:22b";

// ---------------------------------------------------------------- scripted provider

#[derive(Default)]
struct Recording {
    script: Mutex<VecDeque<Vec<StreamEvent>>>,
    requests: Mutex<Vec<ChatRequest>>,
}

impl Recording {
    fn push(&self, evs: Vec<StreamEvent>) {
        self.script.lock().unwrap().push_back(evs);
    }

    /// The conductor's requests, in order (a title or summary call has no tools).
    fn conductor(&self) -> Vec<ChatRequest> {
        self.requests.lock().unwrap().iter().filter(|r| !r.tools.is_empty()).cloned().collect()
    }
}

fn answer(t: &str) -> Vec<StreamEvent> {
    vec![StreamEvent::TextDelta(t.into()), StreamEvent::Stop(StopReason::EndTurn)]
}

fn call(id: &str, name: &str, args: Value) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ToolCallStart { index: 0, id: id.into(), name: name.into() },
        StreamEvent::ToolArgsDelta { index: 0, json: args.to_string() },
        StreamEvent::ToolCallEnd { index: 0 },
        StreamEvent::Stop(StopReason::ToolUse),
    ]
}

fn model() -> ModelInfo {
    let mut m = ModelInfo::bare(ProviderKind::Ollama, MODEL);
    m.supports_tools = true;
    m.context_window = Some(32_768);
    m
}

impl Provider for Recording {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Ollama
    }

    fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(vec![model()])
    }

    fn stream(&self, req: &ChatRequest, _: &CancelToken, on: &mut dyn FnMut(StreamEvent)) -> Result<Completion, ProviderError> {
        self.requests.lock().unwrap().push(req.clone());
        let evs = self.script.lock().unwrap().pop_front().unwrap_or_else(|| answer("ok"));
        for e in &evs {
            on(e.clone());
        }
        Ok(fold_events(&evs))
    }
}

// ---------------------------------------------------------------- the real tools, every call allowed

struct Free(BuiltinTools);

impl ToolHost for Free {
    fn specs(&self) -> Vec<ToolSpec> {
        self.0.specs()
    }

    fn call(&self, call: &ToolCallReq, ctx: &ToolCtx) -> ToolOutcome {
        self.0.call(call, ctx)
    }

    fn call_scoped(&self, call: &ToolCallReq, cx: &marlowe_agent::CallCtx) -> ToolOutcome {
        self.0.call_scoped(call, cx)
    }

    fn middleware(&self) -> Vec<Arc<dyn marlowe_agent::ToolMiddleware>> {
        self.0.middleware()
    }

    fn persona_blocks(&self) -> Vec<marlowe_agent::PersonaBlock> {
        self.0.persona_blocks()
    }

    fn prompt_hints(&self) -> Vec<String> {
        self.0.prompt_hints()
    }

    fn consequence(&self, _call: &ToolCallReq) -> Consequence {
        Consequence::Free
    }
}

// ---------------------------------------------------------------- the engine is a loaded fake: nothing reaches a real Ollama

struct Engine;
impl marlowe_hw::ModelSource for Engine {
    fn files(&self, id: &str) -> Result<marlowe_hw::ModelFiles, String> {
        Err(format!("no files for {id} in this test"))
    }
    fn runtime(&self, _: &str, _: Option<&str>) -> marlowe_hw::RunnerObservation {
        marlowe_hw::RunnerObservation { loaded: true, ..Default::default() }
    }
    fn loaded_models(&self) -> Vec<String> {
        vec![MODEL.into()]
    }
}

struct NoGpu;
impl marlowe_hw::GpuSource for NoGpu {
    fn snapshot(&self) -> Option<marlowe_hw::GpuSnapshot> {
        None
    }
}

struct NoServer;
impl marlowe_hw::ServerControl for NoServer {
    fn apply(&self, _: &marlowe_hw::ServerChange) -> Result<String, String> {
        Err("not in this test".into())
    }
}

// ---------------------------------------------------------------- harness

fn temp(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let dir = std::env::temp_dir().join(format!("marlowe-prefix-{name}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git").args(args).current_dir(dir).output().expect("git runs");
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

fn commit_all(dir: &Path, message: &str) {
    git(dir, &["add", "-A"]);
    git(dir, &["-c", "user.name=T", "-c", "user.email=t@example.com", "commit", "-q", "-m", message]);
}

fn repo(home: &Path) -> PathBuf {
    let dir = home.join("work");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    git(&dir, &["init", "-q", "-b", "main"]);
    std::fs::write(dir.join("README.md"), "# Work\nA small project.\n").unwrap();
    std::fs::write(dir.join("src").join("lib.rs"), "pub fn one() -> u32 { 1 }\n").unwrap();
    std::fs::write(dir.join("CLAUDE.md"), "# Rules\nAlways run the tests before saying done.\n").unwrap();
    std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"work\"\nversion = \"0.1.0\"\nedition = \"2021\"\n").unwrap();
    commit_all(&dir, "first commit");
    dir
}

fn open_mind(home: &Path) -> Mind {
    Mind::open_with(home, MindConfig { models_dir: Some(home.join("no-models-here")), ..MindConfig::default() }).expect("mind opens")
}

struct Running {
    info: DaemonInfo,
    join: JoinHandle<Result<(), DaemonError>>,
}

fn start(home: &Path, provider: Arc<Recording>, mind: Mind) -> Running {
    let mut providers = ProviderFactory::new(Arc::new(MemoryStore::new()));
    providers.register(ProviderKind::Ollama, move |_| Ok(provider.clone() as Arc<dyn Provider>));
    providers.set_hardware(marlowe_brains::Hardware { models: Arc::new(Engine), gpu: Arc::new(NoGpu), server: Arc::new(NoServer) });
    let tools_home = home.to_path_buf();
    let deps = DaemonDeps {
        providers,
        tools: Arc::new(move |req| {
            let t = specs::configure(BuiltinTools::new(req.workspace.clone()), &HostSetup { home: &tools_home, workspace: req.workspace.as_deref() });
            Arc::new(Free(t)) as Arc<dyn ToolHost>
        }),
        user: UserInfo { name: Some("Tester".into()), address: None },
        mind: Some(mind),
        sentinel: None,
        voice: None,
        crew: None,
    };
    let cfg = DaemonConfig { legacy_offer: false, ..DaemonConfig::default() };
    let d = Daemon::start(deps, home, cfg).expect("daemon starts");
    let info = d.info();
    Running { info, join: std::thread::spawn(move || d.wait()) }
}

fn stop(r: Running) {
    let mut c = Client::connect(&r.info);
    c.send(&Request::Shutdown);
    c.until(|e| matches!(e, Event::ShuttingDown));
    r.join.join().unwrap().unwrap();
}

struct Client {
    w: TcpStream,
    r: LineReader<BufReader<TcpStream>>,
}

impl Client {
    fn connect(info: &DaemonInfo) -> Client {
        let w = TcpStream::connect(info.addr()).unwrap();
        w.set_read_timeout(Some(WAIT)).unwrap();
        let mut c = Client { r: LineReader::new(BufReader::new(w.try_clone().unwrap())), w };
        c.send(&Request::Hello { token: info.token.clone(), client: "test".into(), since_seq: None });
        assert!(matches!(c.next().event, Event::Welcome { .. }));
        c
    }

    fn send(&mut self, r: &Request) {
        write_line(&mut self.w, r).unwrap();
    }

    fn next(&mut self) -> Envelope {
        self.r.next::<Envelope>().expect("read").expect("open")
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

    fn ask(&mut self, session: Option<SessionId>, text: &str, workspace: &Path) -> SessionId {
        self.send(&Request::Ask {
            session,
            text: text.into(),
            workspace: Some(workspace.display().to_string()),
            images: Vec::new(),
            options: None,
            client_ms: None,
        });
        let envs = self.until(|e| matches!(e, Event::TurnFinished { .. }));
        envs.iter()
            .find_map(|e| match &e.event {
                Event::TurnStarted { session, .. } => Some(session.clone()),
                _ => None,
            })
            .expect("a TurnStarted")
    }

    fn teams(&mut self, session: &SessionId, on: bool) {
        self.send(&Request::SetTeams { session: session.clone(), on });
        self.until(|e| matches!(e, Event::TeamsChanged { .. }));
    }
}

// ---------------------------------------------------------------- byte comparison

/// What Ollama would be sent for `req` (`num_ctx` fixed: the guard is about content).
fn body(req: &ChatRequest) -> Value {
    marlowe_provider::ollama::request_body(req, 32_768, None)
}

/// Where two strings first differ, as a byte offset and a window of each.
fn first_byte(a: &str, b: &str) -> String {
    let at = a.bytes().zip(b.bytes()).position(|(x, y)| x != y).unwrap_or(a.len().min(b.len()));
    let window = |s: &str| {
        let from = (0..=at.saturating_sub(30)).rev().find(|i| s.is_char_boundary(*i)).unwrap_or(0);
        let to = (at + 50..=s.len()).find(|i| s.is_char_boundary(*i)).unwrap_or(s.len());
        s[from..to.min(s.len())].replace('\n', "\\n")
    };
    format!("byte {at}: was \"{}\", now \"{}\"", window(a), window(b))
}

/// `None` when `next`'s body starts with `prev`'s: same system message, same tools in the same
/// order, same messages, then more. Otherwise the first section that differs and its first byte.
fn broken(prev: &ChatRequest, next: &ChatRequest) -> Option<String> {
    let (a, b) = (body(prev), body(next));
    let (ta, tb) = (a["tools"].to_string(), b["tools"].to_string());
    if ta != tb {
        return Some(format!("tools JSON, {}", first_byte(&ta, &tb)));
    }
    let (ma, mb) = (a["messages"].as_array().unwrap(), b["messages"].as_array().unwrap());
    for (i, x) in ma.iter().enumerate() {
        let (xa, xb) = (x.to_string(), mb.get(i).map(|v| v.to_string()).unwrap_or_default());
        if xa != xb {
            let section = if i == 0 { "the system message".to_string() } else { format!("message {i} ({})", x["role"].as_str().unwrap_or("?")) };
            return Some(format!("{section}, {}", first_byte(&xa, &xb)));
        }
    }
    (mb.len() < ma.len()).then(|| "history got shorter".to_string())
}

// ---------------------------------------------------------------- the scenario

struct Step {
    label: &'static str,
    /// Requests recorded up to and including this turn's.
    upto: usize,
}

/// Runs the scenario; returns every conductor request and where each turn's requests end.
fn scenario() -> (Vec<ChatRequest>, Vec<Step>) {
    let home = temp("home");
    Library::open(&paths::cowork_db(&home)).unwrap().add(&model()).unwrap();
    let work = repo(&home);
    let provider = Arc::new(Recording::default());
    let mind = open_mind(&home);
    let remember = |m: &Mind, text: &str, kind: Kind| {
        m.remember(MemoryIn { text: text.into(), kind, scope: Scope::Global, source: Provenance::User { session: None } }).unwrap();
    };
    let mut steps = Vec::new();
    let mut mark = |p: &Recording, label: &'static str| steps.push(Step { label, upto: p.conductor().len() });

    let r = start(&home, provider.clone(), mind.clone());
    let mut c = Client::connect(&r.info);
    // Turn 1: a tool step and an answer.
    provider.push(call("c1", "read", serde_json::json!({"path": "README.md"})));
    provider.push(answer("It is a small project."));
    let session = c.ask(None, "What is in this repository?", &work);
    mark(&provider, "turn 1");

    // Between 1 and 2: a user fact is remembered, a file is edited and committed, teams mode goes on.
    remember(&mind, "I prefer tabs over spaces in every language.", Kind::Preference);
    std::fs::write(work.join("src").join("lib.rs"), "pub fn one() -> u32 { 1 }\npub fn two() -> u32 { 2 }\n").unwrap();
    commit_all(&work, "add two");
    c.teams(&session, true);
    provider.push(call("c2", "read", serde_json::json!({"path": "src/lib.rs"})));
    provider.push(answer("Two functions."));
    c.ask(Some(session.clone()), "What do the functions do?", &work);
    mark(&provider, "turn 2 (memory, edit, commit, teams on)");

    // Between 2 and 3: the clock passes a minute (the next turn's Now: line differs), an
    // uncommitted edit, another fact, teams off.
    std::fs::write(work.join("README.md"), "# Work\nA small project, edited.\n").unwrap();
    remember(&mind, "The training cluster is called Hyperion.", Kind::Fact);
    c.teams(&session, false);
    c.ask(Some(session.clone()), "And the readme?", &work);
    mark(&provider, "turn 3 (uncommitted edit, fact, teams off)");

    // Daemon restart between 3 and 4, with the project rules edited meanwhile: the session keeps the
    // rules its earlier requests carried (`/memory` reloads them on purpose).
    drop(c);
    stop(r);
    mind.shutdown().ok();
    std::fs::write(work.join("CLAUDE.md"), "# Rules\nNever run the tests.\n").unwrap();
    let mind = open_mind(&home);
    let r = start(&home, provider.clone(), mind.clone());
    let mut c = Client::connect(&r.info);
    c.ask(Some(session.clone()), "Carry on after the restart.", &work);
    mark(&provider, "turn 4 (after a daemon restart)");
    c.ask(Some(session), "One more.", &work);
    mark(&provider, "turn 5");
    drop(c);
    stop(r);
    mind.shutdown().ok();
    let reqs = provider.conductor();
    let _ = std::fs::remove_dir_all(&home);
    (reqs, steps)
}

#[test]
fn every_request_extends_the_one_before_through_memory_edits_commits_teams_and_a_restart() {
    let (reqs, steps) = scenario();
    assert!(reqs.len() >= 7, "{} requests", reqs.len());
    let turn_of = |i: usize| steps.iter().find(|s| i < s.upto).map(|s| s.label).unwrap_or("?");
    let mut breaks = Vec::new();
    for i in 1..reqs.len() {
        if let Some(why) = broken(&reqs[i - 1], &reqs[i]) {
            breaks.push(format!("request {i} ({}): {why}", turn_of(i)));
        }
    }
    assert!(breaks.is_empty(), "the prompt cache would break:\n{}", breaks.join("\n"));

    // Nothing that changes between turns is in the system prompt or the tools: the clock, the repo
    // state, the model line and the profile ride on the newest message of their turn.
    for (i, r) in reqs.iter().enumerate() {
        let system = r.system_text();
        for volatile in ["<environment>", "Now:", "<profile>\n", "uncommitted"] {
            assert!(!system.contains(volatile), "request {i}: {volatile} is in the system prompt");
        }
        assert!(system.contains("Always run the tests before saying done."), "request {i}: the project rules are not in the system prompt");
    }
    let b = body(&reqs[0]);
    eprintln!(
        "first request: system message {} chars, tools JSON {} chars ({} tools), first user message {} chars",
        b["messages"][0].to_string().len(),
        b["tools"].to_string().len(),
        b["tools"].as_array().map_or(0, Vec::len),
        b["messages"][1].to_string().len()
    );
    let user_text = |r: &ChatRequest, from: usize| r.messages[from..].iter().map(|m| m.joined_text()).collect::<Vec<_>>().join("\n");
    assert!(user_text(&reqs[0], 0).contains("Now: "), "the first turn carries the clock in its tail");
    // The remembered preference reaches the model in the tail of the turn after it was written, once.
    assert!(user_text(&reqs[2], 0).contains("I prefer tabs over spaces"), "the profile rides on the turn after the write");
    assert_eq!(user_text(&reqs[3], 0).matches("I prefer tabs over spaces").count(), 1, "an unchanged profile is not repeated");
}

fn request(system: &str, tool: &str, user: &str) -> ChatRequest {
    let mut r = ChatRequest::new(MODEL, vec![marlowe_model::Message::user(user)]);
    r.system = vec![marlowe_model::SystemBlock { text: system.into(), cache: false }];
    r.tools = vec![ToolSpec { name: tool.into(), description: "d".into(), input_schema: serde_json::json!({"type": "object"}) }];
    r
}

/// The guard itself sees a clock in the system prompt, a tool that changed and a rewritten message.
#[test]
fn the_comparison_names_the_section_that_broke() {
    let base = request("persona\n\n<environment>\nNow: 22:29\n</environment>", "read", "hello");
    let clock = request("persona\n\n<environment>\nNow: 22:30\n</environment>", "read", "hello");
    assert!(broken(&base, &clock).unwrap().starts_with("the system message, byte"), "{:?}", broken(&base, &clock));
    let tool = request("persona\n\n<environment>\nNow: 22:29\n</environment>", "write", "hello");
    assert!(broken(&base, &tool).unwrap().starts_with("tools JSON"));
    let mut longer = base.clone();
    longer.messages.push(marlowe_model::Message::user("more"));
    assert_eq!(broken(&base, &longer), None);
    let mut rewritten = longer.clone();
    rewritten.messages[0] = marlowe_model::Message::user("HELLO");
    assert!(broken(&longer, &rewritten).unwrap().starts_with("message 1 (user)"));
}
