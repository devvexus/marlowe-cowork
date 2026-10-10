//! T16 end to end: a background job's exit notice that arrives while the session is idle starts a
//! turn that carries it. The jobs are a fake table in the process-wide registry; the provider is
//! scripted, so nothing here needs a model.

use std::collections::VecDeque;
use std::io::BufReader;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
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
