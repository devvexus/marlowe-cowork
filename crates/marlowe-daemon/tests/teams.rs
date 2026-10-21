//! Teams mode over the real wire: the reminders (on every turn while it is on, the off notice
//! once), persistence across a restart, the default setting, and the system prompt untouched.
#![allow(dead_code)]

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
    fold_events, CancelToken, ChatRequest, Completion, ModelInfo, ModelRef, Provider, ProviderError, ProviderKind,
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
        ["echo", "ask_user", "agent", "danger", "bash", "enter_plan", "exit_plan", "todo_add", "todo_update", "todo_get"]
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
            "bash" => ToolOutcome::ok("ran", "exit 0"),
            "enter_plan" => marlowe_agent::planfile::enter_plan_call(&call.args, cx),
            "exit_plan" => marlowe_agent::planfile::exit_plan_call(&call.args, cx),
            "todo_add" => marlowe_agent::tasklist::todo_add_call(&call.args, cx),
            "todo_update" => marlowe_agent::tasklist::todo_update_call(&call.args, cx),
            "todo_get" => marlowe_agent::tasklist::todo_get_call(&call.args, cx),
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
    // `/fast` runs the Quick slot: here the library's second model.
    lib.set_slot(&marlowe_proto::Slot::Quick, Some(&ModelRef::new(ProviderKind::Ollama, "other")), marlowe_model::Effort::Low).unwrap();
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
        self.ask_in(session, text, None)
    }

    fn ask_in(&mut self, session: Option<SessionId>, text: &str, workspace: Option<String>) -> SessionId {
        self.send(&Request::Ask { session, text: text.into(), workspace, images: Vec::new(), options: None, client_ms: None });
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

// ---------------------------------------------------------------- Teams

const ON_MARK: &str = "Teams is on: form a team task for every substantive request";
const OFF_MARK: &str = "Teams is off: the standard opt-in rule applies again.";

fn set(c: &mut Client, session: &SessionId, on: bool) -> Event {
    c.send(&Request::SetTeams { session: session.clone(), on });
    c.until(|e| matches!(e, Event::TeamsChanged { .. })).pop().unwrap().event
}

fn get(c: &mut Client, session: &SessionId) -> (bool, u32) {
    c.send(&Request::GetTeams { session: session.clone() });
    match c.until(|e| matches!(e, Event::TeamsChanged { .. })).pop().unwrap().event {
        Event::TeamsChanged { on, size, .. } => (on, size),
        other => panic!("{other:?}"),
    }
}

/// The last request's user messages that carry `mark`, counted.
fn carrying(r: &ChatRequest, mark: &str) -> usize {
    r.messages.iter().filter(|m| m.joined_text().contains(mark)).count()
}

#[test]
fn the_reminders_ride_on_the_user_turn_while_teams_is_on_and_the_off_notice_comes_once() {
    let home = temp_home("teams-reminders");
    seed_models(&home);
    let p = Arc::new(Scripted::default());
    let r = start(&home, p.clone());
    let mut c = Client::connect(&r.info);

    let sid = c.ask(None, "FIRST: say hello");
    let first = p.requests().pop().unwrap();
    assert!(!messages_text(&first).contains("Teams is"), "off by default: nothing is said");
    assert_eq!(get(&mut c, &sid), (false, 10));

    assert_eq!(set(&mut c, &sid, true), Event::TeamsChanged { session: sid.clone(), on: true, size: 10 });
    c.ask(Some(sid.clone()), "SECOND: refactor the gate");
    let second = p.requests().pop().unwrap();
    assert_eq!(carrying(&second, ON_MARK), 1, "{}", messages_text(&second));
    assert!(messages_text(&second).contains("under 10 specialists"));
    assert!(messages_text(&second).contains("team-task-authoring"));
    // It leads the turn's own user message, not the system prompt or the tools.
    let user = second.messages.iter().rev().find(|m| m.joined_text().contains("SECOND")).unwrap().joined_text();
    assert!(user.starts_with("<system-reminder>Teams is on"), "{user}");
    assert!(!second.system_text().contains("Teams is"), "the prefix is untouched");
    assert_eq!(second.system_text(), first.system_text(), "the system prompt is byte-identical");
    let names = |r: &ChatRequest| r.tools.iter().map(|t| t.name.clone()).collect::<Vec<_>>();
    assert_eq!(names(&second), names(&first), "no tool appears or goes");

    // Every user turn carries it while it is on.
    c.ask(Some(sid.clone()), "THIRD: and the tests");
    let third = p.requests().pop().unwrap();
    assert_eq!(carrying(&third, ON_MARK), 2, "each turn's own message carries its reminder");

    // Off: the next turn is told once; the one after is quiet.
    assert_eq!(set(&mut c, &sid, false), Event::TeamsChanged { session: sid.clone(), on: false, size: 10 });
    c.ask(Some(sid.clone()), "FOURTH: back to normal");
    let fourth = p.requests().pop().unwrap();
    assert_eq!(carrying(&fourth, OFF_MARK), 1, "{}", messages_text(&fourth));
    assert_eq!(carrying(&fourth, ON_MARK), 2, "earlier turns keep what they carried");
    assert_eq!(fourth.system_text(), first.system_text());
    c.ask(Some(sid.clone()), "FIFTH: still normal");
    let fifth = p.requests().pop().unwrap();
    assert_eq!(carrying(&fifth, OFF_MARK), 1, "the notice is not repeated");
    assert_eq!(carrying(&fifth, ON_MARK), 2);

    // Off when it was already off says nothing more.
    set(&mut c, &sid, false);
    c.ask(Some(sid.clone()), "SIXTH");
    assert_eq!(carrying(&p.requests().pop().unwrap(), OFF_MARK), 1);

    // An unknown session is refused.
    c.send(&Request::SetTeams { session: SessionId("nope".into()), on: true });
    assert!(matches!(c.until(|e| matches!(e, Event::Error { .. })).pop().unwrap().event, Event::Error { .. }));
    drop(c);
    stop(r);
}

#[test]
fn a_sessions_choice_survives_a_restart_and_a_pending_off_notice_is_still_owed() {
    let home = temp_home("teams-restart");
    seed_models(&home);
    let p = Arc::new(Scripted::default());
    let r = start(&home, p.clone());
    let mut c = Client::connect(&r.info);
    let on = c.ask(None, "ONE");
    let off = c.ask(None, "TWO");
    set(&mut c, &on, true);
    set(&mut c, &off, true);
    set(&mut c, &off, false);
    drop(c);
    stop(r);

    let p = Arc::new(Scripted::default());
    let r = start(&home, p.clone());
    let mut c = Client::connect(&r.info);
    assert_eq!(get(&mut c, &on), (true, 10));
    assert_eq!(get(&mut c, &off), (false, 10));
    c.ask(Some(on.clone()), "ON-AGAIN");
    assert_eq!(carrying(&p.requests().pop().unwrap(), ON_MARK), 1, "the reminder returns with the session");
    c.ask(Some(off.clone()), "OFF-AGAIN");
    let req = p.requests().pop().unwrap();
    assert_eq!(carrying(&req, OFF_MARK), 1, "switched off before the restart, told after it");
    c.ask(Some(off.clone()), "OFF-ONCE-MORE");
    assert_eq!(carrying(&p.requests().pop().unwrap(), OFF_MARK), 1, "once");
    drop(c);
    stop(r);
}

#[test]
fn the_default_setting_starts_new_sessions_on_and_sets_the_team_size() {
    let home = temp_home("teams-default");
    seed_models(&home);
    std::fs::write(home.join("settings.toml"), "[teams]\ndefault = true\nsize = 4\n").unwrap();
    let p = Arc::new(Scripted::default());
    let r = start(&home, p.clone());
    let mut c = Client::connect(&r.info);
    let sid = c.ask(None, "FIRST under the default");
    let req = p.requests().pop().unwrap();
    assert_eq!(carrying(&req, ON_MARK), 1);
    assert!(messages_text(&req).contains("under 4 specialists"));
    assert_eq!(get(&mut c, &sid), (true, 4));
    // The session keeps what it had when the default changes; a later one takes the new default.
    std::fs::write(home.join("settings.toml"), "[teams]\ndefault = false\nsize = 6\n").unwrap();
    assert_eq!(get(&mut c, &sid), (true, 6));
    let other = c.ask(None, "SECOND session");
    assert_eq!(get(&mut c, &other), (false, 6));
    assert!(!messages_text(&p.requests().pop().unwrap()).contains("Teams is"));
    drop(c);
    stop(r);
}
