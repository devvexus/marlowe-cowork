//! T16 end to end: a background job's exit notice that arrives while the session is idle starts a
//! turn that carries it. The jobs are a fake table in the process-wide registry; the provider is
//! scripted, so nothing here needs a model.

use std::collections::VecDeque;
use std::io::BufReader;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use marlowe_agent::{Consequence, ToolCallReq, ToolCtx, ToolHost, ToolOutcome, UserInfo};
use marlowe_brains::{Library, MemoryStore, ProviderFactory};
use marlowe_daemon::{Daemon, DaemonConfig, DaemonDeps};
use marlowe_model::{
    CancelToken, ChatRequest, Completion, CompletionBuilder, ModelInfo, Provider, ProviderError, ProviderKind,
    StopReason, StreamEvent, ToolSpec, Usage,
};
use marlowe_os::job::{bg_hub, BgInfo, BgNotice, BgOwner, NoticeKind, Serves};
use marlowe_proto::{paths, write_line, Envelope, Event, LineReader, Request, SessionId};

#[derive(Default)]
struct Scripted {
    requests: Mutex<Vec<ChatRequest>>,
}

impl Provider for Scripted {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Ollama
    }

    fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        let mut a = ModelInfo::bare(ProviderKind::Ollama, "scripted");
        a.supports_tools = true;
        a.context_window = Some(32_768);
        Ok(vec![a])
    }

    fn stream(&self, req: &ChatRequest, _: &CancelToken, on: &mut dyn FnMut(StreamEvent)) -> Result<Completion, ProviderError> {
        self.requests.lock().unwrap().push(req.clone());
        let mut b = CompletionBuilder::new();
        for e in [
            StreamEvent::TextDelta("noted".into()),
            StreamEvent::Usage(Usage { input: 10, output: 5, ..Default::default() }),
            StreamEvent::Stop(StopReason::EndTurn),
        ] {
            b.push(&e);
            on(e);
        }
        Ok(b.finish())
    }
}

struct NoTools;

impl ToolHost for NoTools {
    fn specs(&self) -> Vec<ToolSpec> {
        Vec::new()
    }
    fn call(&self, _: &ToolCallReq, _: &ToolCtx) -> ToolOutcome {
        ToolOutcome::error("no tools")
    }
    fn consequence(&self, _: &ToolCallReq) -> Consequence {
        Consequence::Free
    }
}

/// A table whose one job has exited: the notice waits until someone takes it.
struct Finished {
    session: String,
    notice: Mutex<VecDeque<BgNotice>>,
}

impl BgOwner for Finished {
    fn bg_list(&self, _: Option<&str>) -> Vec<BgInfo> {
        Vec::new()
    }
    fn bg_kill(&self, _: &str) -> Option<bool> {
        None
    }
    fn bg_detach(&self, _: &str, _: Option<&str>) -> Option<String> {
        None
    }
    fn bg_waking(&self) -> Vec<String> {
        if self.notice.lock().unwrap().is_empty() {
            Vec::new()
        } else {
            vec![self.session.clone()]
        }
    }
    fn bg_take(&self, session: &str) -> Vec<BgNotice> {
        if session == self.session {
            self.notice.lock().unwrap().drain(..).collect()
        } else {
            Vec::new()
        }
    }
    fn bg_serves(&self, _: &str) -> Serves {
        Serves::No
    }
    fn bg_user_shell(&self, _: &str, _: &str) -> Option<Result<String, String>> {
        None
    }
    fn bg_running(&self, _: &str) -> usize {
        0
    }
}

/// The engine's view: `scripted` is loaded while the flag is set. Relay turns (which never load a
/// model) run only then.
struct Engine(Arc<AtomicBool>);

impl marlowe_hw::ModelSource for Engine {
    fn files(&self, model_id: &str) -> Result<marlowe_hw::ModelFiles, String> {
        Err(format!("no files for {model_id} in this test"))
    }
    fn runtime(&self, _: &str, _: Option<&str>) -> marlowe_hw::RunnerObservation {
        marlowe_hw::RunnerObservation { loaded: self.0.load(Ordering::SeqCst), ..Default::default() }
    }
    fn loaded_models(&self) -> Vec<String> {
        if self.0.load(Ordering::SeqCst) {
            vec!["scripted".into()]
        } else {
            Vec::new()
        }
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

fn hardware(up: Arc<AtomicBool>) -> marlowe_brains::Hardware {
    marlowe_brains::Hardware { models: Arc::new(Engine(up)), gpu: Arc::new(NoGpu), server: Arc::new(NoServer) }
}

fn temp_home() -> PathBuf {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let home = std::env::temp_dir().join(format!("marlowe-daemon-wake-{}-{nanos}", std::process::id()));
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

#[test]
fn parity_t16_an_idle_session_is_woken_by_a_job_exit_and_the_turn_carries_the_notice() {
    let home = temp_home();
    seed_model(&home);
    let provider = Arc::new(Scripted::default());
    let mut providers = ProviderFactory::new(Arc::new(MemoryStore::new()));
    let p = provider.clone();
    providers.register(ProviderKind::Ollama, move |_| Ok(p.clone() as Arc<dyn Provider>));
    // The wake-up is a relay turn: it runs only on a model that is already up.
    providers.set_hardware(hardware(Arc::new(AtomicBool::new(true))));
    let deps = DaemonDeps {
        providers,
        tools: Arc::new(|_| Arc::new(NoTools) as Arc<dyn ToolHost>),
        user: UserInfo { name: Some("Tester".into()), address: None },
        mind: None,
        sentinel: None,
        voice: None,
        crew: None,
    };
    let cfg = DaemonConfig { legacy_offer: false, ..DaemonConfig::default() };
    let d = Daemon::start(deps, &home, cfg).expect("daemon starts");
    let info = d.info();
    let join = std::thread::spawn(move || d.wait());

    let w = TcpStream::connect(info.addr()).unwrap();
    w.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
    let mut r = LineReader::new(BufReader::new(w.try_clone().unwrap()));
    let mut w = w;
    write_line(&mut w, &Request::Hello { token: info.token.clone(), client: "test".into(), since_seq: None }).unwrap();
    let mut next = || -> Event {
        let env: Envelope = r.next().unwrap().expect("an event");
        env.event
    };

    // A first turn opens the session; the model answers and the session goes idle.
    write_line(
        &mut w,
        &Request::Ask { session: None, text: "start the build in the background".into(), workspace: None, images: Vec::new(), options: None },
    )
    .unwrap();
    let mut session: Option<SessionId> = None;
    loop {
        match next() {
            Event::TurnStarted { session: s, .. } => session = Some(s),
            Event::TurnFinished { .. } => break,
            _ => {}
        }
    }
    let session = session.expect("a TurnStarted");
    assert_eq!(provider.requests.lock().unwrap().len(), 1);

    // The job exits while nobody is asking anything.
    let owner: Arc<dyn BgOwner> = Arc::new(Finished {
        session: session.0.clone(),
        notice: Mutex::new(VecDeque::from([BgNotice {
            session: session.0.clone(),
            id: "j7".into(),
            kind: NoticeKind::Exit,
            text: "[job j7 exited with code 0 after 2.0s: `sleep 2; echo done`]\ndone".into(),
        }])),
    });
    bg_hub().register(Arc::downgrade(&owner));
    // Any job request also starts the daemon's job watcher.
    write_line(&mut w, &Request::ListJobs { session: None }).unwrap();

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut woke = None;
    while Instant::now() < deadline && woke.is_none() {
        if let Event::TurnStarted { session: s, user, .. } = next() {
            if s == session {
                woke = Some(user);
            }
        }
    }
    let user = woke.expect("the idle session was woken");
    assert!(user.contains("job j7 exited with code 0") && user.contains("done"), "{user}");
    loop {
        if matches!(next(), Event::TurnFinished { .. }) {
            break;
        }
    }
    // The model's second request carries the notice as the latest message.
    let reqs = provider.requests.lock().unwrap();
    assert_eq!(reqs.len(), 2);
    let last = reqs[1].messages.last().expect("messages").joined_text();
    assert!(last.contains("job j7 exited with code 0"), "{last}");
    drop(reqs);
    assert!(bg_hub().waking_sessions().iter().all(|s| s != &session.0), "the notice was taken");

    write_line(&mut w, &Request::Shutdown).unwrap();
    join.join().unwrap().unwrap();
    drop(owner);
}

/// A notice whose wake-up cannot run (the conductor's model is not loaded, and relays never load
/// one) is held by the session, not dropped: it goes out right after the user's next turn.
#[test]
fn a_notice_that_cannot_wake_the_session_waits_for_the_users_next_turn() {
    let home = temp_home();
    seed_model(&home);
    let provider = Arc::new(Scripted::default());
    let mut providers = ProviderFactory::new(Arc::new(MemoryStore::new()));
    let p = provider.clone();
    providers.register(ProviderKind::Ollama, move |_| Ok(p.clone() as Arc<dyn Provider>));
    let up = Arc::new(AtomicBool::new(false));
    providers.set_hardware(hardware(up.clone()));
    let deps = DaemonDeps {
        providers,
        tools: Arc::new(|_| Arc::new(NoTools) as Arc<dyn ToolHost>),
        user: UserInfo { name: Some("Tester".into()), address: None },
        mind: None,
        sentinel: None,
        voice: None,
        crew: None,
    };
    let cfg = DaemonConfig { legacy_offer: false, ..DaemonConfig::default() };
    let d = Daemon::start(deps, &home, cfg).expect("daemon starts");
    let info = d.info();
    let join = std::thread::spawn(move || d.wait());

    let w = TcpStream::connect(info.addr()).unwrap();
    w.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
    let mut r = LineReader::new(BufReader::new(w.try_clone().unwrap()));
    let mut w = w;
    write_line(&mut w, &Request::Hello { token: info.token.clone(), client: "test".into(), since_seq: None }).unwrap();
    let mut next = || -> Event {
        let env: Envelope = r.next().unwrap().expect("an event");
        env.event
    };
    let ask = |w: &mut TcpStream, session: Option<SessionId>, text: &str| {
        write_line(w, &Request::Ask { session, text: text.into(), workspace: None, images: Vec::new(), options: None }).unwrap();
    };

    // The user's first turn: the model is up while it runs (a foreground turn may load it).
    up.store(true, Ordering::SeqCst);
    ask(&mut w, None, "start the build in the background");
    let mut session: Option<SessionId> = None;
    loop {
        match next() {
            Event::TurnStarted { session: s, .. } => session = Some(s),
            Event::TurnFinished { .. } => break,
            _ => {}
        }
    }
    let session = session.expect("a TurnStarted");
    // The model is unloaded while the session is idle; then the job exits.
    up.store(false, Ordering::SeqCst);
    let owner: Arc<dyn BgOwner> = Arc::new(Finished {
        session: session.0.clone(),
        notice: Mutex::new(VecDeque::from([BgNotice {
            session: session.0.clone(),
            id: "j9".into(),
            kind: NoticeKind::Exit,
            text: "[job j9 exited with code 0 after 1.0s: `echo held`]\nheld".into(),
        }])),
    });
    bg_hub().register(Arc::downgrade(&owner));
    // Give the watcher time to try (it polls every half second).
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(provider.requests.lock().unwrap().len(), 1, "no model call without a loaded model");
    assert!(bg_hub().waking_sessions().iter().all(|s| s != &session.0), "the notice was taken, and is held by the session");

    // The user's next turn brings the model up; the held notice follows it as its own turn.
    up.store(true, Ordering::SeqCst);
    ask(&mut w, Some(session.clone()), "how is it going?");
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut users = Vec::new();
    while Instant::now() < deadline && users.len() < 2 {
        if let Event::TurnStarted { session: s, user, .. } = next() {
            if s == session {
                users.push(user);
            }
        }
    }
    assert_eq!(users.len(), 2, "{users:?}");
    assert_eq!(users[0], "how is it going?");
    assert!(users[1].contains("job j9 exited with code 0") && users[1].contains("held"), "{}", users[1]);
    loop {
        if matches!(next(), Event::TurnFinished { .. }) {
            break;
        }
    }

    write_line(&mut w, &Request::Shutdown).unwrap();
    join.join().unwrap().unwrap();
    drop(owner);
}
