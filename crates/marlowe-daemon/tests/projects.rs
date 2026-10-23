//! Projects through the daemon, end to end over the real wire: a scripted conductor calls
//! `manage_project`, the project parks on its plan card, the card is answered the way a client
//! answers it, forced-form employees (answered from their schemas) run it to its end on a world
//! with no web, and the result reaches the conversation as a relay turn with the report path.
//! Real tool host, real research functions; no network, no model, no GPU.

use std::collections::VecDeque;
use std::io::BufReader;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use marlowe_agent::{ToolHost, UserInfo};
use marlowe_brains::{Library, MemoryStore, ProviderFactory};
use marlowe_crew::{CrewConfig, CrewSetup, ProjectSetup};
use marlowe_daemon::{Daemon, DaemonConfig, DaemonDeps, DaemonError, HostRequest};
use marlowe_model::{CancelToken, ChatRequest, Completion, CompletionBuilder, ModelInfo, Provider, ProviderError, ProviderKind, StopReason, StreamEvent, Usage};
use marlowe_project::research::{Admit, Failure, Fetched, PageFetch, ResearchDeps, SystemResolver, Web};
use marlowe_proto::{paths, write_line, DaemonInfo, Envelope, Event, LineReader, Request};
use serde_json::{json, Value};

const WAIT: Duration = Duration::from_secs(90);

// ---------------------------------------------------------------- the scripted model

/// The conductor answers from a queue; a request with a response format is a project employee
/// and is answered with a value that fits its schema.
#[derive(Default)]
struct Scripted {
    conductor: Mutex<VecDeque<Vec<StreamEvent>>>,
    seen: Mutex<Vec<ChatRequest>>,
    employee_calls: Mutex<u32>,
}

impl Scripted {
    fn say(&self, events: Vec<StreamEvent>) {
        self.conductor.lock().unwrap().push_back(events);
    }

    fn conductor_requests(&self) -> Vec<ChatRequest> {
        self.seen.lock().unwrap().iter().filter(|r| r.format.is_none()).cloned().collect()
    }
}

fn reply(s: &str) -> Vec<StreamEvent> {
    vec![StreamEvent::TextDelta(s.into()), StreamEvent::Usage(Usage { input: 10, output: 5, ..Default::default() }), StreamEvent::Stop(StopReason::EndTurn)]
}

fn tool_call(id: &str, name: &str, args: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ToolCallStart { index: 0, id: id.into(), name: name.into() },
        StreamEvent::ToolArgsDelta { index: 0, json: args.into() },
        StreamEvent::ToolCallEnd { index: 0 },
        StreamEvent::Usage(Usage { input: 10, output: 5, ..Default::default() }),
        StreamEvent::Stop(StopReason::ToolUse),
    ]
}

/// A value that satisfies `schema`: the first enum value, `x` for text, one element per array.
fn sample(schema: &Value) -> Value {
    if let Some(c) = schema.get("const") {
        return c.clone();
    }
    if let Some(e) = schema.get("enum").and_then(Value::as_array) {
        return e.iter().find(|v| !v.is_null()).cloned().unwrap_or(Value::Null);
    }
    let ty = match schema.get("type") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).find(|s| *s != "null").unwrap_or("null").to_string(),
        _ => "object".into(),
    };
    match ty.as_str() {
        "object" => Value::Object(schema.get("properties").and_then(Value::as_object).into_iter().flatten().map(|(k, v)| (k.clone(), sample(v))).collect()),
        "array" => {
            let n = schema.get("minItems").and_then(Value::as_u64).unwrap_or(0).max(1).min(schema.get("maxItems").and_then(Value::as_u64).unwrap_or(1));
            Value::Array((0..n).map(|_| sample(schema.get("items").unwrap_or(&Value::Null))).collect())
        }
        "string" => Value::String("x".repeat(schema.get("minLength").and_then(Value::as_u64).unwrap_or(1).max(1) as usize)),
        "integer" => json!(schema.get("minimum").and_then(Value::as_f64).map(|m| m.ceil() as i64).unwrap_or(0).max(0)),
        "number" => json!(schema.get("minimum").and_then(Value::as_f64).unwrap_or(0.0)),
        "boolean" => json!(false),
        _ => Value::Null,
    }
}

impl Provider for Scripted {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Ollama
    }

    fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(vec![model()])
    }

    fn stream(&self, req: &ChatRequest, cancel: &CancelToken, on: &mut dyn FnMut(StreamEvent)) -> Result<Completion, ProviderError> {
        self.seen.lock().unwrap().push(req.clone());
        if cancel.is_cancelled() {
            return Err(ProviderError::cancelled());
        }
        let mut b = CompletionBuilder::new();
        let events = match &req.format {
            Some(schema) => {
                *self.employee_calls.lock().unwrap() += 1;
                vec![StreamEvent::TextDelta(sample(schema).to_string()), StreamEvent::Usage(Usage { input: 120, output: 30, ..Default::default() }), StreamEvent::Stop(StopReason::EndTurn)]
            }
            None => self.conductor.lock().unwrap().pop_front().unwrap_or_else(|| reply("ok")),
        };
        for e in events {
            b.push(&e);
            on(e);
        }
        Ok(b.finish())
    }
}

fn model() -> ModelInfo {
    let mut m = ModelInfo::bare(ProviderKind::Ollama, "scripted");
    m.supports_tools = true;
    m.context_window = Some(32_768);
    m
}

// ---------------------------------------------------------------- a world with no web

struct NoWeb;

impl Web for NoWeb {
    fn channels(&self) -> Vec<marlowe_research::ChannelId> {
        vec![marlowe_research::ChannelId::Web]
    }

    fn search(&self, _c: marlowe_research::ChannelId, _q: &marlowe_research::ChannelQuery, _cancel: &CancelToken) -> Result<Vec<marlowe_research::SourceRef>, marlowe_research::ChannelError> {
        Ok(Vec::new())
    }
}

struct NoPages;

impl PageFetch for NoPages {
    fn read(&self, url: &str, _admit: Admit<'_>, _cancel: &CancelToken) -> Fetched {
        Fetched::failed(url, Failure::Unreachable, "no network in this test")
    }
}

// ---------------------------------------------------------------- harness

struct Engine(Arc<AtomicBool>);

impl marlowe_hw::ModelSource for Engine {
    fn files(&self, model_id: &str) -> Result<marlowe_hw::ModelFiles, String> {
        Err(format!("no files for {model_id} in this test"))
    }
    fn runtime(&self, _: &str, _: Option<&str>) -> marlowe_hw::RunnerObservation {
        marlowe_hw::RunnerObservation { loaded: self.0.load(Ordering::SeqCst), ..Default::default() }
    }
    fn loaded_models(&self) -> Vec<String> {
        if self.0.load(Ordering::SeqCst) { vec!["scripted".into()] } else { Vec::new() }
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

fn temp_home(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let home = std::env::temp_dir().join(format!("marlowe-proj-it-{name}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    Library::open(&paths::cowork_db(&home)).unwrap().add(&model()).unwrap();
    home
}

fn deps(provider: Arc<Scripted>, home: &Path, projects: bool) -> DaemonDeps {
    deps_with(provider, home, projects, Arc::new(AtomicBool::new(true)))
}

/// `up`: whether the engine has the scripted model loaded.
fn deps_with(provider: Arc<Scripted>, home: &Path, projects: bool, up: Arc<AtomicBool>) -> DaemonDeps {
    let mut providers = ProviderFactory::new(Arc::new(MemoryStore::new()));
    providers.register(ProviderKind::Ollama, move |_| Ok(provider.clone() as Arc<dyn Provider>));
    providers.set_hardware(marlowe_brains::Hardware { models: Arc::new(Engine(up)), gpu: Arc::new(NoGpu), server: Arc::new(NoServer) });
    let h = home.to_path_buf();
    let research: marlowe_crew::ResearchFactory = Arc::new(move |memory| {
        Arc::new(ResearchDeps::new(h.join("projects"), h.join("output"), Arc::new(NoWeb), Arc::new(NoPages), Arc::new(SystemResolver), memory))
    });
    let crew_tools = Arc::new(|_: &marlowe_crew::sandbox::Sandbox, _: Option<&Path>| Arc::new(marlowe_exec::BuiltinTools::new(None)) as Arc<dyn ToolHost>);
    let setup = CrewSetup { tools: crew_tools, config: CrewConfig::default(), research: None, projects: projects.then_some(ProjectSetup { research }) };
    let home_for_tools = home.to_path_buf();
    DaemonDeps {
        providers,
        tools: Arc::new(move |req: &HostRequest| {
            let mut tools = marlowe_exec::BuiltinTools::new(req.workspace.clone());
            if let Some(m) = &req.model {
                tools = tools.with_model_exposure(m);
            }
            let setup = marlowe_exec::specs::HostSetup { home: &home_for_tools, workspace: req.workspace.as_deref() };
            Arc::new(marlowe_exec::specs::configure(tools, &setup)) as Arc<dyn ToolHost>
        }),
        user: UserInfo { name: Some("Tester".into()), address: None },
        mind: None,
        sentinel: None,
        voice: None,
        crew: Some(setup),
    }
}

struct Running {
    info: DaemonInfo,
    join: JoinHandle<Result<(), DaemonError>>,
}

fn start(home: &Path, provider: Arc<Scripted>, projects: bool) -> Running {
    let d = Daemon::start(deps(provider, home, projects), home, DaemonConfig::default()).expect("daemon starts");
    let info = d.info();
    Running { info, join: std::thread::spawn(move || d.wait()) }
}

fn stop(r: Running) {
    let mut c = Client::connect(&r.info);
    c.send(&Request::Shutdown);
    c.until_event(|e| matches!(e, Event::ShuttingDown));
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
        self.r.next::<Envelope>().expect("read an envelope").expect("stream open")
    }

    fn until_event(&mut self, pred: impl Fn(&Event) -> bool) -> Vec<Envelope> {
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

fn ask(c: &mut Client, text: &str) {
    c.send(&Request::Ask { session: None, text: text.into(), workspace: None, images: Vec::new(), options: None, client_ms: None });
}

fn plan_json() -> String {
    json!({
        "action": "create",
        "plan": {
            "objective": "Find the newest and upcoming Kimi models and say what is confirmed and what is rumoured.",
            "questions": ["Which Kimi models (Moonshot AI) are newest or upcoming?", "Is a successor to the newest Kimi announced?"],
            "fields": [{"key": "model_name"}, {"key": "status", "values": ["released", "announced", "rumoured", "unknown"]}]
        }
    })
    .to_string()
}

fn tool_names(req: &ChatRequest) -> Vec<String> {
    req.tools.iter().map(|t| t.name.clone()).collect()
}

// ---------------------------------------------------------------- tests

#[test]
fn a_natural_request_becomes_a_parked_plan_the_card_runs_it_and_the_result_reaches_the_conversation() {
    let home = temp_home("e2e");
    let p = Arc::new(Scripted::default());
    p.say(tool_call("c1", "manage_project", &plan_json()));
    p.say(reply("I made a plan to look into the newest Kimi models. Shall I go ahead?"));
    p.say(reply("The project finished. Its report is in the path above; nothing was found on the web in this run."));
    let d = start(&home, p.clone(), true);
    let mut a = Client::connect(&d.info);

    ask(&mut a, "do some deep research on the newest Kimi models and tell me what is confirmed");
    let first = a.until_event(|e| matches!(e, Event::TurnFinished { .. }));
    let result = first
        .iter()
        .find_map(|e| match &e.event {
            Event::ToolFinished { ok, .. } => Some(*ok),
            _ => None,
        })
        .expect("the tool call finished");
    assert!(result, "{first:#?}");
    // The conductor saw the tool and the projects block; the project is parked, nothing was asked of an employee.
    let reqs = p.conductor_requests();
    assert!(tool_names(&reqs[0]).contains(&"manage_project".to_string()), "{:?}", tool_names(&reqs[0]));
    assert!(reqs[0].system_text().contains("<projects>") && reqs[0].system_text().contains("manage_project"), "the block is appended while the tool is live");
    assert!(reqs[0].messages.last().unwrap().joined_text().contains("The user asks for research"), "the research reminder rides on the user turn's tail");
    assert_eq!(*p.employee_calls.lock().unwrap(), 0, "nothing runs before approval");

    a.send(&Request::ListProjects);
    let listed = a.until_event(|e| matches!(e, Event::ProjectList { .. }));
    let Event::ProjectList { projects } = &listed.last().unwrap().event else { unreachable!() };
    assert_eq!(projects.len(), 1);
    let id = projects[0].id.clone();
    assert_eq!(projects[0].status, "awaiting_plan");

    // The card is answered the way a client answers it.
    a.send(&Request::AnswerProjectGate { project: id.clone(), gate: "g_1".into(), choice: "approve".into(), text: None });
    // Every checkin the project raises is approved until the relay turn arrives.
    let session_turn = loop {
        let seen = a.until_event(|e| matches!(e, Event::ProjectGateRaised { .. } | Event::TurnStarted { .. }));
        match &seen.last().unwrap().event {
            Event::ProjectGateRaised { project, gate, kind, .. } => {
                assert_eq!(project, &id);
                if kind == "checkin" {
                    a.send(&Request::AnswerProjectGate { project: id.clone(), gate: gate.clone(), choice: "approve".into(), text: None });
                }
            }
            Event::TurnStarted { user, .. } => break user.clone(),
            _ => unreachable!(),
        }
    };
    assert!(session_turn.starts_with(&format!("[crew · run project {id} · ")), "{session_turn}");
    assert!(session_turn.contains(&format!("Project {id} finished")), "{session_turn}");
    let path = session_turn.lines().find_map(|l| l.strip_prefix("Report: ")).expect("the relay names the report");
    assert!(Path::new(path).is_file(), "the report exists at {path}");
    a.until_event(|e| matches!(e, Event::TurnFinished { .. }));

    // The whole state is one request away, and the project is finished.
    a.send(&Request::GetProject { project: id.clone() });
    let got = a.until_event(|e| matches!(e, Event::ProjectState { .. }));
    let Event::ProjectState { state, .. } = &got.last().unwrap().event else { unreachable!() };
    assert_eq!(state["status"], "finished");
    assert!(*p.employee_calls.lock().unwrap() > 0);
    stop(d);
}

#[test]
fn without_the_binary_supplying_projects_there_is_no_service_and_no_reminder() {
    let home = temp_home("off");
    let p = Arc::new(Scripted::default());
    let d = start(&home, p.clone(), false);
    let mut a = Client::connect(&d.info);
    ask(&mut a, "do some deep research on the newest Kimi models");
    a.until_event(|e| matches!(e, Event::TurnFinished { .. }));
    let reqs = p.conductor_requests();
    assert!(!reqs[0].messages.last().unwrap().joined_text().contains("manage_project"), "no reminder to use a tool that has no service behind it");
    a.send(&Request::ListProjects);
    let err = a.until_event(|e| matches!(e, Event::Error { .. }));
    assert!(matches!(&err.last().unwrap().event, Event::Error { message, .. } if message.contains("projects are not running")));
    stop(d);
}

#[test]
fn a_project_parked_on_its_card_survives_a_restart_and_the_same_answer_resumes_it() {
    let home = temp_home("restart");
    let p = Arc::new(Scripted::default());
    p.say(tool_call("c1", "manage_project", &plan_json()));
    p.say(reply("I made a plan. Shall I go ahead?"));
    let d = start(&home, p.clone(), true);
    let mut a = Client::connect(&d.info);
    ask(&mut a, "research the newest Kimi models for me");
    a.until_event(|e| matches!(e, Event::TurnFinished { .. }));
    a.send(&Request::ListProjects);
    let listed = a.until_event(|e| matches!(e, Event::ProjectList { .. }));
    let Event::ProjectList { projects } = &listed.last().unwrap().event else { unreachable!() };
    let id = projects[0].id.clone();
    drop(a);
    stop(d);

    // A new daemon on the same home: the card is still open and nothing was asked of a model.
    let p2 = Arc::new(Scripted::default());
    p2.say(reply("The project finished."));
    let d = start(&home, p2.clone(), true);
    let mut a = Client::connect(&d.info);
    a.send(&Request::GetProject { project: id.clone() });
    let got = a.until_event(|e| matches!(e, Event::ProjectState { .. }));
    let Event::ProjectState { state, .. } = &got.last().unwrap().event else { unreachable!() };
    assert_eq!(state["status"], "awaiting_plan");
    assert_eq!(*p2.employee_calls.lock().unwrap(), 0);
    a.send(&Request::AnswerProjectGate { project: id.clone(), gate: "g_1".into(), choice: "approve".into(), text: None });
    let turn = loop {
        let seen = a.until_event(|e| matches!(e, Event::ProjectGateRaised { .. } | Event::TurnStarted { .. }));
        match &seen.last().unwrap().event {
            Event::ProjectGateRaised { gate, kind, .. } if kind == "checkin" => {
                a.send(&Request::AnswerProjectGate { project: id.clone(), gate: gate.clone(), choice: "approve".into(), text: None });
            }
            Event::TurnStarted { user, .. } => break user.clone(),
            _ => {}
        }
    };
    assert!(turn.contains(&format!("Project {id} finished")), "{turn}");
    stop(d);
}

#[test]
fn a_finished_project_whose_relay_finds_no_model_waits_for_the_users_next_turn() {
    let home = temp_home("cold");
    let p = Arc::new(Scripted::default());
    let d = start(&home, p.clone(), true);
    let mut a = Client::connect(&d.info);
    ask(&mut a, "hello");
    let got = a.until_event(|e| matches!(e, Event::TurnFinished { .. }));
    let session = got
        .iter()
        .find_map(|e| match &e.event {
            Event::TurnStarted { session, .. } => Some(session.clone()),
            _ => None,
        })
        .unwrap();
    drop(a);
    stop(d);

    // A project that finished while the daemon was down, with its session still to be told.
    let reg = marlowe_project::Registry::builtin();
    let plan = marlowe_project::parse_plan(
        &json!({"objective": "Find the newest Kimi models and say what is confirmed.", "questions": ["Which Kimi models are newest?"], "fields": [{"key": "model_name"}]}),
        &reg,
        1_792_756_800_000,
    )
    .unwrap();
    let mut st = marlowe_project::ProjectState::new("p_0badcafe".into(), plan, &reg, 1_792_756_800_000);
    st.session = Some(session.0.clone());
    st.status = marlowe_project::ProjectStatus::Finished;
    st.tainted = true;
    let store = marlowe_project::ProjectStore::open(&home.join("projects"), &paths::cowork_db(&home)).unwrap();
    store.create(&st).unwrap();
    let marker = store.dir("p_0badcafe").unwrap().join("relayed");

    // The daemon starts with the model not loaded: a relay never loads one, so it waits.
    let up = Arc::new(AtomicBool::new(false));
    let before = p.conductor_requests().len();
    let d = Daemon::start(deps_with(p.clone(), &home, true, up.clone()), &home, DaemonConfig::default()).expect("daemon starts");
    let d = Running { info: d.info(), join: std::thread::spawn(move || d.wait()) };
    std::thread::sleep(Duration::from_millis(1500));
    assert!(!marker.exists(), "held, still due");
    assert_eq!(p.conductor_requests().len(), before, "no model was called to relay it");

    // The user's turn, with the model up: the result goes out after it.
    up.store(true, Ordering::SeqCst);
    p.say(reply("Hi."));
    p.say(reply("The project found nothing."));
    let mut a = Client::connect(&d.info);
    a.send(&Request::Ask { session: Some(session), text: "anything new?".into(), workspace: None, images: Vec::new(), options: None, client_ms: None });
    let deadline = Instant::now() + WAIT;
    while !marker.exists() {
        assert!(Instant::now() < deadline, "the held result was never relayed");
        std::thread::sleep(Duration::from_millis(20));
    }
    let relayed = p.conductor_requests()[before..].iter().any(|r| r.messages.iter().any(|m| m.joined_text().contains("[crew · run project p_0badcafe")));
    assert!(relayed, "the conductor saw the result after the user's turn");
    drop(a);
    stop(d);
}

/// The reminder riding on the last user message of the latest conductor request.
fn tail_of(p: &Scripted) -> String {
    p.conductor_requests().last().unwrap().messages.iter().rev().find(|m| m.role == marlowe_model::Role::User).unwrap().joined_text()
}

#[test]
fn the_reminders_point_at_projects_with_teams_on_and_only_offer_one_with_teams_off() {
    let home = temp_home("remind");
    let p = Arc::new(Scripted::default());
    let d = start(&home, p.clone(), true);
    let mut a = Client::connect(&d.info);
    ask(&mut a, "hello");
    let got = a.until_event(|e| matches!(e, Event::TurnFinished { .. }));
    let session = got
        .iter()
        .find_map(|e| match &e.event {
            Event::TurnStarted { session, .. } => Some(session.clone()),
            _ => None,
        })
        .unwrap();
    let say = |a: &mut Client, text: &str| {
        a.send(&Request::Ask { session: Some(session.clone()), text: text.into(), workspace: None, images: Vec::new(), options: None, client_ms: None });
        a.until_event(|e| matches!(e, Event::TurnFinished { .. }));
    };
    // Teams off: a short chat gets nothing, a long request is offered a project, research starts one.
    say(&mut a, "what time is it in Tokyo?");
    assert!(!tail_of(&p).contains("manage_project"), "{}", tail_of(&p));
    say(&mut a, &format!("I need you to rework the whole build pipeline {}", "and then update every script that depends on it, ".repeat(6)));
    assert!(tail_of(&p).contains("a project could do it and offer"), "{}", tail_of(&p));
    say(&mut a, "can you research how speculative decoding works");
    assert!(tail_of(&p).contains("Start a research project with manage_project"), "{}", tail_of(&p));

    // Teams on: substantive work starts a project; a saved task a kind replaces says so.
    a.send(&Request::SetTeams { session: session.clone(), on: true });
    a.until_event(|e| matches!(e, Event::TeamsChanged { on: true, .. }));
    say(&mut a, "please summarise the notes");
    let on = tail_of(&p);
    assert!(on.contains("Teams is on: for every substantive request start a project with manage_project"), "{on}");
    assert!(!on.contains("form a team task"), "{on}");
    say(&mut a, "run the web-research task on the newest Kimi models");
    let replaced = tail_of(&p);
    assert!(replaced.contains("A project replaces it") && !replaced.contains("Run that one now"), "{replaced}");
    say(&mut a, "run the code-review task on the diff");
    let kept = tail_of(&p);
    assert!(kept.contains("Run that one now: team{name, args}"), "{kept}");
    stop(d);
}
