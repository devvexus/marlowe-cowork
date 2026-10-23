//! Track F over the real wire: `/loop` schedules, `ask_user`, background agents waking the
//! session, `/clear`, `/compact`, `/model`, and the command and agent listings. A daemon on an
//! ephemeral port with a temp profile home and a scripted provider.

use std::collections::VecDeque;
use std::io::BufReader;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use marlowe_agent::{CallCtx, Consequence, ToolCallReq, ToolCtx, ToolHost, ToolOutcome, UserInfo};
use marlowe_brains::{Library, MemoryStore, ProviderFactory};
use marlowe_daemon::{Daemon, DaemonConfig, DaemonDeps, DaemonError};
use marlowe_model::{
    fold_events, CancelToken, ChatRequest, Completion, ModelInfo, ModelRef, Provider, ProviderError, ProviderKind, Role,
    StopReason, StreamEvent, ToolSpec,
};
use marlowe_proto::{paths, write_line, DaemonInfo, Envelope, Event, LineReader, Request, SessionId};

// ---------------------------------------------------------------- scripted provider

/// The conductor's calls take the script in order; a subagent's (its system prompt carries the
/// subagent frame) take `sub` first, else answer after a pause, so the conductor's turn has ended
/// by then.
#[derive(Default)]
struct Scripted {
    main: Mutex<VecDeque<Vec<StreamEvent>>>,
    sub: Mutex<VecDeque<Vec<StreamEvent>>>,
    requests: Mutex<Vec<ChatRequest>>,
}

impl Scripted {
    fn push(&self, evs: Vec<StreamEvent>) {
        self.main.lock().unwrap().push_back(evs);
    }

    fn push_sub(&self, evs: Vec<StreamEvent>) {
        self.sub.lock().unwrap().push_back(evs);
    }

    fn requests(&self) -> Vec<ChatRequest> {
        self.requests.lock().unwrap().clone()
    }
}

fn answer(t: &str) -> Vec<StreamEvent> {
    vec![StreamEvent::TextDelta(t.into()), StreamEvent::Stop(StopReason::EndTurn)]
}

fn call(name: &str, args: serde_json::Value) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ToolCallStart { index: 0, id: "c1".into(), name: name.into() },
        StreamEvent::ToolArgsDelta { index: 0, json: args.to_string() },
        StreamEvent::ToolCallEnd { index: 0 },
        StreamEvent::Stop(StopReason::ToolUse),
    ]
}

impl Provider for Scripted {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Ollama
    }

    fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(Vec::new())
    }

    fn stream(&self, req: &ChatRequest, cancel: &CancelToken, on: &mut dyn FnMut(StreamEvent)) -> Result<Completion, ProviderError> {
        self.requests.lock().unwrap().push(req.clone());
        let sub = req.system.iter().any(|b| b.text.contains("<subagent>"));
        let scripted_sub = if sub { self.sub.lock().unwrap().pop_front() } else { None };
        let evs = if let Some(evs) = scripted_sub {
            evs
        } else if sub {
            let end = Instant::now() + Duration::from_millis(1_500);
            while Instant::now() < end {
                if cancel.is_cancelled() {
                    return Err(ProviderError::cancelled());
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            answer("BG-REPORT: three files changed")
        } else {
            self.main.lock().unwrap().pop_front().unwrap_or_else(|| answer("ok"))
        };
        for e in &evs {
            on(e.clone());
        }
        Ok(fold_events(&evs))
    }
}

// ---------------------------------------------------------------- tools: echo + the agent group's ask_user and agent

struct Tools;

impl ToolHost for Tools {
    fn specs(&self) -> Vec<ToolSpec> {
        ["echo", "ask_user", "agent", "danger"]
            .iter()
            .map(|n| ToolSpec { name: n.to_string(), description: n.to_string(), input_schema: serde_json::json!({"type": "object"}) })
            .collect()
    }

    fn call(&self, call: &ToolCallReq, ctx: &ToolCtx) -> ToolOutcome {
        self.call_scoped(call, &CallCtx::detached(ctx.clone()))
    }

    fn call_scoped(&self, call: &ToolCallReq, cx: &CallCtx) -> ToolOutcome {
        match call.name.as_str() {
            "echo" => ToolOutcome::ok(call.args.to_string(), "echoed"),
            "ask_user" => marlowe_agent::todo::ask_user_call(&call.args, cx),
            "agent" => marlowe_agent::subagent::agent_call(&call.args, cx),
            "danger" => ToolOutcome::ok("DANGER-RAN", "ran"),
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
    let home = std::env::temp_dir().join(format!("marlowe-daemon-f-{name}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    home
}

fn seed_models(home: &Path) {
    let lib = Library::open(&paths::cowork_db(home)).unwrap();
    for id in ["scripted", "other"] {
        let mut info = ModelInfo::bare(ProviderKind::Ollama, id);
        info.supports_tools = true;
        info.context_window = Some(32_768);
        lib.add(&info).unwrap();
    }
}

struct Running {
    info: DaemonInfo,
    join: JoinHandle<Result<(), DaemonError>>,
}

/// The engine's view: `scripted` is loaded, so relay turns (which never load a model) run.
struct Engine;

impl marlowe_hw::ModelSource for Engine {
    fn files(&self, model_id: &str) -> Result<marlowe_hw::ModelFiles, String> {
        Err(format!("no files for {model_id} in this test"))
    }
    fn runtime(&self, _: &str, _: Option<&str>) -> marlowe_hw::RunnerObservation {
        marlowe_hw::RunnerObservation { loaded: true, ..Default::default() }
    }
    fn loaded_models(&self) -> Vec<String> {
        vec!["scripted".into()]
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

fn loaded() -> marlowe_brains::Hardware {
    marlowe_brains::Hardware { models: Arc::new(Engine), gpu: Arc::new(NoGpu), server: Arc::new(NoServer) }
}

fn start(home: &Path, provider: Arc<Scripted>) -> Running {
    start_with(home, provider, None)
}

/// With `hardware`, the engine reports what is loaded (relay turns run only on a loaded model).
fn start_with(home: &Path, provider: Arc<Scripted>, hardware: Option<marlowe_brains::Hardware>) -> Running {
    let mut providers = ProviderFactory::new(Arc::new(MemoryStore::new()));
    providers.register(ProviderKind::Ollama, move |_| Ok(provider.clone() as Arc<dyn Provider>));
    if let Some(hw) = hardware {
        providers.set_hardware(hw);
    }
    let deps = DaemonDeps {
        providers,
        tools: Arc::new(|_| Arc::new(Tools) as Arc<dyn ToolHost>),
        user: UserInfo { name: Some("Tester".into()), address: None },
        mind: None,
        sentinel: None,
        voice: None,
        crew: None,
    };
    let d = Daemon::start(deps, home, DaemonConfig::default()).expect("daemon starts");
    let info = d.info();
    Running { info, join: std::thread::spawn(move || d.wait()) }
}

fn stop(r: Running) {
    let mut c = Client::connect(&r.info);
    c.send(&Request::Shutdown);
    c.until(|e| matches!(e, Event::ShuttingDown));
    r.join.join().unwrap().unwrap();
}

const WAIT: Duration = Duration::from_secs(30);

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

    /// Asks in `session` (a new one when `None`) and waits for the turn to finish; returns the
    /// session.
    fn ask(&mut self, session: Option<SessionId>, text: &str) -> SessionId {
        self.send(&Request::Ask { session, text: text.into(), workspace: None, images: Vec::new(), options: None, client_ms: None });
        let envs = self.until(|e| matches!(e, Event::TurnFinished { .. }));
        envs.iter()
            .find_map(|e| match &e.event {
                Event::TurnStarted { session, .. } => Some(session.clone()),
                _ => None,
            })
            .expect("a TurnStarted")
    }
}

fn messages_text(r: &ChatRequest) -> String {
    r.messages.iter().map(|m| m.joined_text()).collect::<Vec<_>>().join("\n")
}

// ---------------------------------------------------------------- tests

#[test]
fn parity_x19_a_loop_schedule_fires_twice_and_survives_a_restart() {
    let home = temp_home("x19");
    seed_models(&home);
    let p = Arc::new(Scripted::default());
    let r = start(&home, p.clone());
    let mut c = Client::connect(&r.info);
    let sid = c.ask(None, "hello");
    c.send(&Request::AddSchedule { session: Some(sid.clone()), every_secs: 1, text: "TICK".into() });
    let list = schedules(c.until(|e| matches!(e, Event::Schedules { .. })).pop().unwrap().event);
    assert_eq!((list.len(), list[0].every_secs, list[0].session.as_ref()), (1, 1, Some(&sid)));
    let id = list[0].id.clone();
    let ticks = |seen: &[Event]| seen.iter().filter(|e| matches!(e, Event::TurnFinished { session, .. } if *session == sid)).count();
    let seen = gather(&mut c, |seen| ticks(seen) >= 2);
    let started: Vec<&String> = seen
        .iter()
        .filter_map(|e| match e {
            Event::TurnStarted { user, .. } => Some(user),
            _ => None,
        })
        .collect();
    assert_eq!(started, ["TICK", "TICK"], "the schedule ran its text as two turns in its session");
    drop(c);
    stop(r);

    // Restart: the schedule is still there; removing it stops it.
    let r = start(&home, p.clone());
    let mut c = Client::connect(&r.info);
    c.send(&Request::ListSchedules);
    let list = schedules(c.until(|e| matches!(e, Event::Schedules { .. })).pop().unwrap().event);
    assert_eq!(list.iter().map(|s| s.id.clone()).collect::<Vec<_>>(), [id.clone()]);
    c.send(&Request::RemoveSchedule { id });
    assert!(schedules(c.until(|e| matches!(e, Event::Schedules { .. })).pop().unwrap().event).is_empty());
    c.send(&Request::AddSchedule { session: None, every_secs: 0, text: "x".into() });
    assert!(matches!(c.until(|e| matches!(e, Event::Error { .. })).pop().unwrap().event, Event::Error { .. }));
    drop(c);
    stop(r);
}

/// Reads events until `done` holds over everything seen so far.
fn gather(c: &mut Client, done: impl Fn(&[Event]) -> bool) -> Vec<Event> {
    let deadline = Instant::now() + WAIT;
    let mut seen = Vec::new();
    while Instant::now() < deadline {
        seen.push(c.next().event);
        if done(&seen) {
            return seen;
        }
    }
    panic!("timed out; saw {seen:#?}");
}

fn schedules(e: Event) -> Vec<marlowe_proto::ScheduleInfo> {
    match e {
        Event::Schedules { list } => list,
        other => panic!("expected Schedules, got {other:?}"),
    }
}

#[test]
fn parity_t23_ask_user_round_trips_over_the_wire() {
    let home = temp_home("t23");
    seed_models(&home);
    let p = Arc::new(Scripted::default());
    p.push(call(
        "ask_user",
        serde_json::json!({"questions": [{"question": "Which database?", "header": "DB", "options": [{"label": "sqlite"}, {"label": "postgres"}]}]}),
    ));
    p.push(answer("Using sqlite."));
    let r = start(&home, p.clone());
    let mut c = Client::connect(&r.info);
    c.send(&Request::Ask { session: None, text: "set up storage".into(), workspace: None, images: Vec::new(), options: None, client_ms: None });
    let (session, id, questions) = match c.until(|e| matches!(e, Event::QuestionAsked { .. })).pop().unwrap().event {
        Event::QuestionAsked { session, id, questions } => (session, id, questions),
        _ => unreachable!(),
    };
    assert_eq!(questions[0].options, ["sqlite", "postgres"]);
    c.send(&Request::AnswerQuestion { session, id: id.clone(), answers: vec!["sqlite".into()] });
    c.until(|e| matches!(e, Event::TurnFinished { .. }));
    let reqs = p.requests();
    let results: Vec<String> = reqs[1].messages.iter().flat_map(|m| m.tool_results_iter().map(|r| r.content.clone())).collect();
    assert!(results.iter().any(|r| r.contains("\"Which database?\" = \"sqlite\"")), "{results:?}");
    c.send(&Request::AnswerQuestion { session: SessionId("x".into()), id, answers: vec![] });
    assert!(matches!(c.until(|e| matches!(e, Event::Error { .. })).pop().unwrap().event, Event::Error { .. }), "answered once");
    drop(c);
    stop(r);
}

#[test]
fn parity_a4_t16_a_background_agent_report_starts_a_turn_in_the_idle_session() {
    let home = temp_home("a4");
    seed_models(&home);
    let p = Arc::new(Scripted::default());
    p.push(call("agent", serde_json::json!({"type": "general", "description": "long job", "prompt": "do the long job", "background": true})));
    p.push(answer("Started it; I will tell you when it is done."));
    p.push(answer("The background job finished: three files changed."));
    // The report's turn is a relay: it runs on the model the user's turn left loaded.
    let r = start_with(&home, p.clone(), Some(loaded()));
    let mut c = Client::connect(&r.info);
    let sid = c.ask(None, "kick it off");
    // The conductor's turn ended at once; the report then starts a turn of its own.
    let seen = gather(&mut c, |seen| seen.iter().any(|e| matches!(e, Event::TurnFinished { .. })));
    let relay = seen
        .iter()
        .find_map(|e| match e {
            Event::TurnStarted { session, user, .. } if *session == sid => Some(user.clone()),
            _ => None,
        })
        .expect("a notice turn");
    assert!(relay.contains("[agent agent-1") && relay.contains("BG-REPORT"), "{relay}");
    let last = p.requests().into_iter().filter(|r| !r.system.iter().any(|b| b.text.contains("<subagent>"))).last().unwrap();
    assert!(messages_text(&last).contains("BG-REPORT"));
    drop(c);
    stop(r);
}

#[test]
fn parity_c11_a_new_session_starts_empty() {
    let home = temp_home("c11");
    seed_models(&home);
    let p = Arc::new(Scripted::default());
    let r = start(&home, p.clone());
    let mut c = Client::connect(&r.info);
    let first = c.ask(None, "SESSION-ONE-SECRET");
    c.send(&Request::NewSession { workspace: None });
    let second = match c.until(|e| matches!(e, Event::SessionOpened { .. })).pop().unwrap().event {
        Event::SessionOpened { session, .. } => session,
        _ => unreachable!(),
    };
    assert_ne!(first, second);
    c.ask(Some(second), "hello");
    let last = p.requests().pop().unwrap();
    assert!(!messages_text(&last).contains("SESSION-ONE-SECRET"));
    assert_eq!(last.messages.iter().filter(|m| m.role == Role::User).count(), 1);
    drop(c);
    stop(r);
}

#[test]
fn parity_c8_c15_compact_and_model_switch_apply_at_the_next_step() {
    let home = temp_home("c8");
    seed_models(&home);
    let p = Arc::new(Scripted::default());
    let r = start(&home, p.clone());
    let mut c = Client::connect(&r.info);
    let sid = c.ask(None, "first question");
    c.send(&Request::Compact { session: sid.clone(), focus: Some("the parser".into()) });
    c.send(&Request::SwitchModel { session: sid.clone(), model: ModelRef::new(ProviderKind::Ollama, "other"), effort: None });
    p.push(answer("SUMMARY"));
    p.push(answer("second answer"));
    c.ask(Some(sid.clone()), "second question");
    let reqs = p.requests();
    let summariser = &reqs[reqs.len() - 2];
    assert!(messages_text(summariser).contains("focus on: the parser"), "{}", messages_text(summariser));
    let last = reqs.last().unwrap();
    assert_eq!(last.model, "other", "the switched model answers");
    assert!(messages_text(last).contains("SUMMARY"), "the summary arrives as a framed user message");
    assert!(!last.system.iter().any(|b| b.text.contains("SUMMARY")), "and not as a system block");
    c.send(&Request::SwitchModel { session: sid.clone(), model: ModelRef::new(ProviderKind::Ollama, "missing"), effort: None });
    assert!(matches!(c.until(|e| matches!(e, Event::Error { .. })).pop().unwrap().event, Event::Error { .. }));
    c.send(&Request::AddDir { session: sid, path: home.join("nope").display().to_string() });
    assert!(matches!(c.until(|e| matches!(e, Event::Error { .. })).pop().unwrap().event, Event::Error { .. }));
    drop(c);
    stop(r);
}

#[test]
fn parity_x10_a1_commands_and_agent_types_are_listed() {
    let home = temp_home("x10");
    seed_models(&home);
    let ws = home.join("ws");
    std::fs::create_dir_all(ws.join(".claude/commands")).unwrap();
    std::fs::create_dir_all(ws.join(".claude/agents")).unwrap();
    std::fs::write(ws.join(".claude/commands/ship.md"), "---\ndescription: Ship it\nargument-hint: <version>\n---\nShip $1").unwrap();
    std::fs::write(ws.join(".claude/agents/auditor.md"), "---\nname: auditor\ndescription: Audits\ntools: Read, Grep\nmodel: haiku\n---\nAudit.").unwrap();
    let r = start(&home, Arc::new(Scripted::default()));
    let mut c = Client::connect(&r.info);
    let workspace = Some(ws.display().to_string());
    c.send(&Request::ListCommands { workspace: workspace.clone() });
    let list = match c.until(|e| matches!(e, Event::Commands { .. })).pop().unwrap().event {
        Event::Commands { list } => list,
        _ => unreachable!(),
    };
    let ship = list.iter().find(|i| i.name == "ship").expect("ship");
    assert_eq!(ship.argument_hint.as_deref(), Some("<version>"));
    for b in ["init", "compact", "loop", "memory", "add-dir", "model", "clear"] {
        assert!(list.iter().any(|i| i.name == b), "{b}");
    }
    c.send(&Request::ListAgentTypes { workspace });
    let types = match c.until(|e| matches!(e, Event::AgentTypes { .. })).pop().unwrap().event {
        Event::AgentTypes { list } => list,
        _ => unreachable!(),
    };
    for n in ["explore", "plan", "general", "auditor"] {
        assert!(types.iter().any(|t| t.name == n), "{n}");
    }
    let auditor = types.iter().find(|t| t.name == "auditor").unwrap();
    assert_eq!((auditor.model.as_deref(), auditor.tools.clone()), (Some("haiku"), vec!["read".to_string(), "grep".into()]));
    drop(c);
    stop(r);
}

#[test]
fn parity_a4_a_background_agent_lost_to_a_restart_is_reported_on_the_next_turn() {
    let home = temp_home("a4lost");
    seed_models(&home);
    let p = Arc::new(Scripted::default());
    p.push(call("agent", serde_json::json!({"type": "general", "description": "long job", "prompt": "do the long job", "background": true})));
    p.push(answer("Started it."));
    let r = start(&home, p.clone());
    let mut c = Client::connect(&r.info);
    let sid = c.ask(None, "kick it off");
    // Shut down while the agent is still working (it takes 1.5 s): it is stopped, not reported.
    drop(c);
    stop(r);

    let r = start(&home, p.clone());
    let mut c = Client::connect(&r.info);
    p.push(answer("Noted; I will start it again."));
    c.ask(Some(sid.clone()), "anything new?");
    let last = p.requests().into_iter().filter(|r| !r.system.iter().any(|b| b.text.contains("<subagent>"))).last().unwrap();
    let text = messages_text(&last);
    assert!(text.contains("background agent agent-1") && text.contains("was stopped when Marlowe restarted"), "{text}");
    assert!(!text.contains("BG-REPORT"), "the stopped agent never reported");
    // Told once.
    p.push(answer("ok"));
    c.ask(Some(sid), "and now?");
    let last = p.requests().into_iter().filter(|r| !r.system.iter().any(|b| b.text.contains("<subagent>"))).last().unwrap();
    assert_eq!(messages_text(&last).matches("was stopped when Marlowe restarted").count(), 1);
    drop(c);
    stop(r);
}

/// Review fix: an interrupt ends a foreground subagent's pending approval at once (the default
/// approval timeout is ten minutes), and the conductor's turn finishes.
#[test]
fn an_interrupt_ends_a_pending_subagent_approval_at_once() {
    let home = temp_home("subcancel");
    seed_models(&home);
    let p = Arc::new(Scripted::default());
    p.push(call("agent", serde_json::json!({"type": "general", "description": "risky", "prompt": "do the risky thing", "background": false})));
    p.push_sub(call("danger", serde_json::json!({})));
    let r = start(&home, p.clone());
    let mut c = Client::connect(&r.info);
    c.send(&Request::Ask { session: None, text: "go".into(), workspace: None, images: Vec::new(), options: None, client_ms: None });
    let (session, summary) = match c.until(|e| matches!(e, Event::ApprovalRequested { .. })).pop().unwrap().event {
        Event::ApprovalRequested { session, summary, .. } => (session, summary),
        _ => unreachable!(),
    };
    assert!(summary.contains("subagent agent-1") && summary.contains("dangerous"), "{summary}");
    let asked = Instant::now();
    c.send(&Request::Interrupt { session });
    c.until(|e| matches!(e, Event::TurnFinished { .. }));
    let took = asked.elapsed();
    eprintln!("interrupt to turn end: {took:?}");
    assert!(took < Duration::from_millis(1_500), "the pending approval held the turn for {took:?}");
    drop(c);
    stop(r);
}

/// C15: a model chosen with `/model` gets the context gate's window, not its advertised one:
/// `big` advertises 262k but its policy holds the runner to 32k, so a 26k-token turn compacts.
#[test]
fn parity_c15_a_switched_model_works_against_the_gated_window() {
    use marlowe_proto::{ContextPolicy, KvType, PolicySource};

    let home = temp_home("c15w");
    seed_models(&home);
    let big = ModelRef::new(ProviderKind::Ollama, "big");
    {
        let lib = Library::open(&paths::cowork_db(&home)).unwrap();
        let mut info = ModelInfo::bare(ProviderKind::Ollama, "big");
        info.supports_tools = true;
        info.context_window = Some(262_144);
        lib.add(&info).unwrap();
        let policy = ContextPolicy { max_ctx: 32_768, kv_type: KvType::F16, parallel: 1, source: PolicySource::User, updated_ms: 0 };
        lib.set_policy(&big, &policy).unwrap();
    }
    let p = Arc::new(Scripted::default());
    let r = start(&home, p.clone());
    let mut c = Client::connect(&r.info);
    let sid = c.ask(None, "first question");
    c.send(&Request::SwitchModel { session: sid.clone(), model: big, effort: None });
    p.push(answer("SUMMARY-OF-EARLIER"));
    p.push(answer("after"));
    // About 26k tokens (chars/3): over 70% of 32k, far under 70% of 262k.
    let long = "The quick brown fox checks one more line of the log. ".repeat(1_500);
    c.ask(Some(sid), &long);
    let last = p.requests().pop().unwrap();
    assert_eq!(last.model, "big");
    assert!(
        messages_text(&last).contains("SUMMARY-OF-EARLIER"),
        "the turn was not compacted, so the agent sized it against the advertised 262k window"
    );
    drop(c);
    stop(r);
}
