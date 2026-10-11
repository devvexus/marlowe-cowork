//! Track F3 over the real wire: `/fast`, the plan file round trip, the shared task list across a
//! daemon restart, `ultrathink`, and `/fewer-permission-prompts`. A daemon on an ephemeral port
//! with a temp profile home and a scripted provider.

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
        self.send(&Request::Ask { session, text: text.into(), workspace, images: Vec::new(), options: None });
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

// ---------------------------------------------------------------- U17 /fast

#[test]
fn parity_u17_fast_runs_the_quick_slots_model_keeps_history_and_toggles_back() {
    let home = temp_home("u17");
    seed_models(&home);
    let p = Arc::new(Scripted::default());
    let r = start(&home, p.clone());
    let mut c = Client::connect(&r.info);
    let sid = c.ask(None, "FIRST-QUESTION");
    assert_eq!(p.requests().last().unwrap().model, "scripted");

    c.send(&Request::SetFast { session: sid.clone(), on: true });
    match c.until(|e| matches!(e, Event::FastChanged { .. })).pop().unwrap().event {
        Event::FastChanged { session, state } => {
            assert_eq!(session, sid);
            assert!(state.on);
            assert_eq!(state.model, Some(ModelRef::new(ProviderKind::Ollama, "other")));
        }
        other => panic!("{other:?}"),
    }
    c.ask(Some(sid.clone()), "SECOND-QUESTION");
    let last = p.requests().pop().unwrap();
    assert_eq!(last.model, "other", "the next call uses the Quick model");
    assert!(messages_text(&last).contains("FIRST-QUESTION"), "history kept");

    c.send(&Request::SetFast { session: sid.clone(), on: false });
    match c.until(|e| matches!(e, Event::FastChanged { .. })).pop().unwrap().event {
        Event::FastChanged { state, .. } => {
            assert!(!state.on);
            assert_eq!(state.model, Some(ModelRef::new(ProviderKind::Ollama, "scripted")));
        }
        other => panic!("{other:?}"),
    }
    c.ask(Some(sid.clone()), "THIRD-QUESTION");
    assert_eq!(p.requests().pop().unwrap().model, "scripted", "toggling back restores the conductor slot");

    // An unknown session is refused.
    c.send(&Request::SetFast { session: SessionId("nope".into()), on: true });
    assert!(matches!(c.until(|e| matches!(e, Event::Error { .. })).pop().unwrap().event, Event::Error { .. }));
    drop(c);
    stop(r);
}

// ---------------------------------------------------------------- M13 the plan file over the wire

fn tool_results_of(r: &ChatRequest) -> Vec<String> {
    r.messages.iter().flat_map(|m| m.tool_results_iter().map(|t| t.content.clone())).collect()
}

#[test]
fn parity_m13_the_plan_file_round_trip_pins_the_users_edit_and_applies_the_chosen_mode() {
    use marlowe_proto::{PermissionMode, PlanFileDecision};
    let home = temp_home("m13");
    seed_models(&home);
    let p = Arc::new(Scripted::default());
    let r = start(&home, p.clone());
    let mut c = Client::connect(&r.info);
    let sid = c.ask(None, "let us plan a migration");
    c.send(&Request::SetMode { session: sid.clone(), mode: PermissionMode::Plan });
    c.until(|e| matches!(e, Event::ModeChanged { mode: PermissionMode::Plan, .. }));

    p.push(call("exit_plan", serde_json::json!({"plan": "# Migrate the store\n1. copy the data\n2. drop the old table"})));
    p.push(answer("plan approved, starting"));
    c.send(&Request::Ask { session: Some(sid.clone()), text: "write the plan".into(), workspace: None, images: Vec::new(), options: None });
    let (id, path, text) = match c.until(|e| matches!(e, Event::PlanReady { .. })).pop().unwrap().event {
        Event::PlanReady { session, id, path, text } => {
            assert_eq!(session, sid);
            (id, path, text)
        }
        other => panic!("{other:?}"),
    };
    assert!(text.contains("copy the data"));
    let file = PathBuf::from(&path);
    assert!(file.starts_with(home.join("plans")), "{path}");
    assert!(file.file_name().unwrap().to_string_lossy().ends_with("-migrate-the-store.md"), "{path}");
    assert!(std::fs::read_to_string(&file).unwrap().contains("drop the old table"));

    // The user edits the file in $EDITOR, then approves into accept_edits.
    std::fs::write(&file, "# Migrate the store\n1. copy the data\n2. keep the old table until Friday\n").unwrap();
    c.send(&Request::AnswerPlan { session: sid.clone(), id, decision: PlanFileDecision::Approve { mode: PermissionMode::AcceptEdits } });
    let mode = c.until(|e| matches!(e, Event::ModeChanged { .. })).pop().unwrap().event;
    assert!(matches!(mode, Event::ModeChanged { mode: PermissionMode::AcceptEdits, .. }), "{mode:?}");
    c.until(|e| matches!(e, Event::TurnFinished { .. }));
    let results = tool_results_of(&p.requests().pop().unwrap());
    assert!(results.iter().any(|t| t.contains("own edits") && t.contains("keep the old table until Friday")), "{results:?}");

    // A rejection keeps plan mode and returns the feedback.
    c.send(&Request::SetMode { session: sid.clone(), mode: PermissionMode::Plan });
    c.until(|e| matches!(e, Event::ModeChanged { mode: PermissionMode::Plan, .. }));
    p.push(call("exit_plan", serde_json::json!({"plan": "1. do everything at once"})));
    p.push(answer("revising"));
    c.send(&Request::Ask { session: Some(sid.clone()), text: "again".into(), workspace: None, images: Vec::new(), options: None });
    let id2 = match c.until(|e| matches!(e, Event::PlanReady { .. })).pop().unwrap().event {
        Event::PlanReady { id, .. } => id,
        other => panic!("{other:?}"),
    };
    c.send(&Request::AnswerPlan { session: sid.clone(), id: id2, decision: PlanFileDecision::Reject { feedback: Some("split it up".into()) } });
    c.until(|e| matches!(e, Event::TurnFinished { .. }));
    let results = tool_results_of(&p.requests().pop().unwrap());
    assert!(results.iter().any(|t| t.contains("did not approve") && t.contains("split it up")), "{results:?}");

    // An answer for a plan nobody asked is refused.
    c.send(&Request::AnswerPlan { session: sid, id: "p99".into(), decision: PlanFileDecision::Reject { feedback: None } });
    assert!(matches!(c.until(|e| matches!(e, Event::Error { .. })).pop().unwrap().event, Event::Error { .. }));
    drop(c);
    stop(r);
}

// ---------------------------------------------------------------- T34 the list survives a restart

#[test]
fn parity_t34_the_task_list_survives_a_daemon_restart_and_is_the_one_list() {
    let home = temp_home("t34");
    seed_models(&home);
    let p = Arc::new(Scripted::default());
    let r = start(&home, p.clone());
    let mut c = Client::connect(&r.info);
    p.push(call("todo_add", serde_json::json!({"subject": "write the parser"})));
    p.push(call("todo_add", serde_json::json!({"subject": "test the parser", "blocked_by": ["1"]})));
    p.push(answer("listed"));
    let sid = c.ask(None, "plan the work");
    drop(c);
    stop(r);

    let r = start(&home, p.clone());
    let mut c = Client::connect(&r.info);
    p.push(call("todo_update", serde_json::json!({"id": "2", "status": "in_progress"})));
    p.push(call("todo_update", serde_json::json!({"id": "1", "status": "in_progress"})));
    p.push(answer("tried"));
    c.ask(Some(sid.clone()), "start on the tests");
    let results = tool_results_of(&p.requests().pop().unwrap());
    assert!(results.iter().any(|t| t.contains("blocked by #1")), "item 2 waits for 1 after the restart: {results:?}");
    assert!(
        results.iter().any(|t| t.contains("[~] #1 write the parser (owner conductor)") && t.contains("[ ] #2 test the parser (blocked by #1)")),
        "{results:?}"
    );
    drop(c);
    stop(r);
}

// ---------------------------------------------------------------- C14 over the wire

#[test]
fn parity_c14_ultrathink_reaches_the_request_as_max_for_that_turn_only() {
    let home = temp_home("c14");
    seed_models(&home);
    let p = Arc::new(Scripted::default());
    let r = start(&home, p.clone());
    let mut c = Client::connect(&r.info);
    let sid = c.ask(None, "ultrathink: why is the cache cold?");
    assert_eq!(p.requests().last().unwrap().effort, marlowe_model::Effort::Max);
    c.ask(Some(sid), "thanks, now something plain");
    assert_ne!(p.requests().last().unwrap().effort, marlowe_model::Effort::Max);
    drop(c);
    stop(r);
}

// ---------------------------------------------------------------- M14 over the wire

#[test]
fn parity_m14_fewer_permission_prompts_proposes_the_read_only_rules_from_the_projects_transcripts() {
    let home = temp_home("m14");
    seed_models(&home);
    let ws = home.join("project");
    std::fs::create_dir_all(&ws).unwrap();
    let workspace = Some(ws.display().to_string());
    let p = Arc::new(Scripted::default());
    let r = start(&home, p.clone());
    let mut c = Client::connect(&r.info);
    let mut sid = None;
    let lines = [
        "git status", "git status", "git status", "git status", "git status", "rg TODO src", "rg unsafe src", "rg fn src",
        "rg mod src", "cargo test -p x", "cargo test -p y", "cargo test -p z",
    ];
    for line in lines {
        p.push(call("bash", serde_json::json!({"command": line})));
        p.push(answer("ok"));
        sid = Some(c.ask_in(sid, "go", workspace.clone()));
    }
    c.send(&Request::SuggestRules { workspace: workspace.clone() });
    let list = match c.until(|e| matches!(e, Event::RuleSuggestions { .. })).pop().unwrap().event {
        Event::RuleSuggestions { list } => list,
        _ => unreachable!(),
    };
    let patterns: Vec<&str> = list.iter().map(|s| s.pattern.as_str()).collect();
    // `rg` takes `--pre <cmd>`: four different rg lines are not generalised into `rg:*`.
    assert_eq!(patterns, ["bash(git status:*)"], "{list:?}");
    assert_eq!(list[0].count, 5);
    // Another project's transcripts are not read.
    c.send(&Request::SuggestRules { workspace: Some(home.join("elsewhere").display().to_string()) });
    match c.until(|e| matches!(e, Event::RuleSuggestions { .. })).pop().unwrap().event {
        Event::RuleSuggestions { list } => assert!(list.is_empty()),
        _ => unreachable!(),
    }
    drop(c);
    stop(r);
}
