//! Seam 2 over the real wire: every wave-2 request is routed to its track's stub (which answers
//! with an `Error` naming the request and the track, and leaves the connection usable), and the
//! task registry is a service of every session whose changes reach every client.

use std::collections::VecDeque;
use std::io::BufReader;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use marlowe_agent::{
    CallCtx, Consequence, NewTask, TaskKind, TaskRegistry, TaskStatus, Tasks, ToolCallReq, ToolCtx, ToolHost, ToolOutcome,
    UserInfo,
};
use marlowe_brains::{Library, MemoryStore, ProviderFactory};
use marlowe_daemon::{Daemon, DaemonConfig, DaemonDeps, DaemonError};
use marlowe_model::{
    fold_events, CancelToken, ChatRequest, Completion, ModelInfo, Provider, ProviderError, ProviderKind, StopReason,
    StreamEvent, ToolSpec,
};
use marlowe_proto::background::PlanFileDecision;
use marlowe_proto::{paths, write_line, DaemonInfo, Envelope, Event, LineReader, PermissionMode, Request, RunId, SessionId};

// ---------------------------------------------------------------- scripted provider and tools

#[derive(Default)]
struct Scripted {
    main: Mutex<VecDeque<Vec<StreamEvent>>>,
}

fn answer(t: &str) -> Vec<StreamEvent> {
    vec![StreamEvent::TextDelta(t.into()), StreamEvent::Stop(StopReason::EndTurn)]
}

fn call(name: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ToolCallStart { index: 0, id: "c1".into(), name: name.into() },
        StreamEvent::ToolArgsDelta { index: 0, json: "{}".into() },
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

    fn stream(&self, _req: &ChatRequest, _cancel: &CancelToken, on: &mut dyn FnMut(StreamEvent)) -> Result<Completion, ProviderError> {
        let evs = self.main.lock().unwrap().pop_front().unwrap_or_else(|| answer("ok"));
        for e in &evs {
            on(e.clone());
        }
        Ok(fold_events(&evs))
    }
}

/// One tool that registers a background job through the session's `Tasks` service, as N's
/// tools will.
struct Tools;

impl ToolHost for Tools {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec { name: "probe_tasks".into(), description: "registers a job".into(), input_schema: serde_json::json!({"type": "object"}) }]
    }

    fn call(&self, call: &ToolCallReq, ctx: &ToolCtx) -> ToolOutcome {
        self.call_scoped(call, &CallCtx::detached(ctx.clone()))
    }

    fn call_scoped(&self, call: &ToolCallReq, cx: &CallCtx) -> ToolOutcome {
        match (call.name.as_str(), cx.scope.services.get::<Tasks>()) {
            ("probe_tasks", Some(tasks)) => {
                let id = tasks.0.register(NewTask::new(TaskKind::Job, "cargo test").raw_id("j1").session(Some(cx.session.clone())));
                ToolOutcome::ok(format!("registered {id}"), "registered")
            }
            ("probe_tasks", None) => ToolOutcome::error("no Tasks service in this session"),
            (other, _) => ToolOutcome::error(format!("unknown tool {other}")),
        }
    }

    fn consequence(&self, _call: &ToolCallReq) -> Consequence {
        Consequence::Free
    }
}

// ---------------------------------------------------------------- harness

fn temp_home(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let home = std::env::temp_dir().join(format!("marlowe-daemon-w2-{name}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    home
}

fn seed_models(home: &Path) {
    let lib = Library::open(&paths::cowork_db(home)).unwrap();
    let mut info = ModelInfo::bare(ProviderKind::Ollama, "scripted");
    info.supports_tools = true;
    info.context_window = Some(32_768);
    lib.add(&info).unwrap();
}

struct Running {
    info: DaemonInfo,
    tasks: Arc<dyn TaskRegistry>,
    join: JoinHandle<Result<(), DaemonError>>,
}

fn start(home: &Path, provider: Arc<Scripted>) -> Running {
    let mut providers = ProviderFactory::new(Arc::new(MemoryStore::new()));
    providers.register(ProviderKind::Ollama, move |_| Ok(provider.clone() as Arc<dyn Provider>));
    let deps = DaemonDeps {
        providers,
        tools: Arc::new(|_| Arc::new(Tools) as Arc<dyn ToolHost>),
        user: UserInfo { name: Some("Tester".into()), address: None },
        mind: None,
        sentinel: None,
        voice: None,
        crew: None,
    };
    let cfg = DaemonConfig { legacy_offer: false, ..DaemonConfig::default() };
    let d = Daemon::start(deps, home, cfg).expect("daemon starts");
    let info = d.info();
    let tasks = d.tasks();
    Running { info, tasks, join: std::thread::spawn(move || d.wait()) }
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
}

// ---------------------------------------------------------------- tests

fn sid() -> SessionId {
    SessionId("s-1".into())
}

fn run() -> RunId {
    RunId("r-1".into())
}

#[test]
fn every_wave2_request_reaches_its_tracks_stub() {
    let home = temp_home("routes");
    seed_models(&home);
    let r = start(&home, Arc::new(Scripted::default()));
    let mut c = Client::connect(&r.info);

    let cases: Vec<(Request, &str, &str)> = vec![
        (Request::ListTasks { session: None }, "list_tasks", "N"),
        (Request::StopTask { id: "job_j1".into() }, "stop_task", "N"),
        (Request::TaskOutput { id: "job_j1".into(), since: None, filter: None }, "task_output", "N"),
        (
            Request::CronCreate { session: Some(sid()), cron: Some("0 3 * * *".into()), once_at_ms: None, text: "eval".into(), durable: true },
            "cron_create",
            "N",
        ),
        (Request::NotifySettings { toast: Some(false), voice: None }, "notify_settings", "N"),
        (Request::OpenFile { path: "C:/x".into(), reveal: false }, "open_file", "N"),
        (Request::ListWorkflows, "list_workflows", "W"),
        (Request::WatchWorkflow { run: run() }, "watch_workflow", "W"),
        (
            Request::RunWorkflow { session: Some(sid()), name: Some("code-review".into()), script_path: None, args: serde_json::Value::Null, resume_from: None },
            "run_workflow",
            "W",
        ),
        (Request::SkipWorkflowAgent { run: run(), agent: 2 }, "skip_workflow_agent", "W"),
        (Request::ListFindings { run: Some(run()), session: None }, "list_findings", "W"),
    ];
    for (req, op, track) in cases {
        c.send(&req);
        let env = c.until(|e| matches!(e, Event::Error { message, .. } if message.contains(op))).pop().unwrap();
        let Event::Error { message, session } = env.event else { unreachable!() };
        assert!(message.contains(&format!("track {track})")), "{op}: {message}");
        let named = serde_json::to_value(&req).unwrap().get("session").is_some();
        assert_eq!(session.is_some(), named, "{op}: the error names the session exactly when the request does");
    }

    // F3 filled its three in (`f3_ops.rs`): they answer for real now (`tests/f3.rs` covers the
    // behaviour); here only that they no longer say "not available yet".
    c.send(&Request::SetFast { session: sid(), on: true });
    let e = c.until(|e| matches!(e, Event::Error { .. })).pop().unwrap().event;
    assert!(matches!(&e, Event::Error { message, .. } if message.contains("unknown session")), "{e:?}");
    c.send(&Request::AnswerPlan { session: sid(), id: "p1".into(), decision: PlanFileDecision::Approve { mode: PermissionMode::Default } });
    let e = c.until(|e| matches!(e, Event::Error { .. })).pop().unwrap().event;
    assert!(matches!(&e, Event::Error { message, .. } if message.contains("not waiting")), "{e:?}");
    c.send(&Request::SuggestRules { workspace: None });
    c.until(|e| matches!(e, Event::RuleSuggestions { .. }));

    // The connection is still good.
    c.send(&Request::Status);
    c.until(|e| matches!(e, Event::Status(_)));
    drop(c);
    stop(r);
}

/// BG1 at the seam: a tool reaches the registry through the session's services, and every change
/// is broadcast as `TaskUpdated` to every client.
#[test]
fn parity_bg1_the_registry_is_a_session_service_and_its_changes_reach_every_client() {
    let home = temp_home("registry");
    seed_models(&home);
    let p = Arc::new(Scripted::default());
    p.main.lock().unwrap().push_back(call("probe_tasks"));
    p.main.lock().unwrap().push_back(answer("done"));
    let r = start(&home, p);
    let mut watcher = Client::connect(&r.info);
    let mut asker = Client::connect(&r.info);

    asker.send(&Request::Ask { session: None, text: "start a job".into(), workspace: None, images: Vec::new(), options: None });
    let seen = asker.until(|e| matches!(e, Event::TurnFinished { .. }));
    let result = seen.iter().find_map(|e| match &e.event {
        Event::ToolFinished { ok, summary, .. } => Some((*ok, summary.clone())),
        _ => None,
    });
    assert_eq!(result, Some((true, "registered".into())), "the session has the Tasks service: {seen:#?}");

    // Both clients hear it, attributed to the session: the asker already did (the job registered
    // during its turn), the watcher hears it from its own stream.
    let heard_by_asker = seen.iter().any(|e| matches!(&e.event, Event::TaskUpdated(t) if t.id == "job_j1"));
    assert!(heard_by_asker, "{seen:#?}");
    let env = watcher.until(|e| matches!(e, Event::TaskUpdated(t) if t.id == "job_j1")).pop().unwrap();
    let Event::TaskUpdated(task) = &env.event else { unreachable!() };
    assert_eq!((task.kind, task.status, task.title.as_str()), (TaskKind::Job, TaskStatus::Running, "cargo test"));
    assert!(env.seq > 0, "a broadcast, replayable");
    assert!(env.event.session().is_some());

    // The daemon's handle is the same registry: a finish is a second broadcast.
    assert_eq!(r.tasks.list(None).len(), 1);
    r.tasks.set_status("job_j1", TaskStatus::Done, Some("exit 0".into()));
    let env = watcher.until(|e| matches!(e, Event::TaskUpdated(t) if t.status == TaskStatus::Done)).pop().unwrap();
    let Event::TaskUpdated(task) = env.event else { unreachable!() };
    assert_eq!(task.detail.as_deref(), Some("exit 0"));
    assert!(task.ended_ms.is_some());

    drop((asker, watcher));
    stop(r);
}
