//! Job requests (track B) over a real daemon: `ListJobs`, `KillJob`, `UserShell`, and the registry's
//! events broadcast as `JobStarted` / `JobOutput` / `JobExited`. The jobs are a fake table in the
//! process-wide registry, as `marlowe-exec`'s tool host would register them.

use std::io::BufReader;
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use marlowe_agent::{Consequence, ToolCallReq, ToolCtx, ToolHost, ToolOutcome, UserInfo};
use marlowe_brains::{MemoryStore, ProviderFactory};
use marlowe_daemon::{Daemon, DaemonConfig, DaemonDeps};
use marlowe_model::ToolSpec;
use marlowe_os::job::{bg_hub, BgEvent, BgInfo, BgNotice, BgOwner, Serves};
use marlowe_proto::{write_line, Envelope, Event, LineReader, Request, SessionId};

struct NoTools;

impl ToolHost for NoTools {
    fn specs(&self) -> Vec<ToolSpec> {
        Vec::new()
    }
    fn call(&self, _call: &ToolCallReq, _ctx: &ToolCtx) -> ToolOutcome {
        ToolOutcome::error("no tools")
    }
    fn consequence(&self, _call: &ToolCallReq) -> Consequence {
        Consequence::Free
    }
}

fn temp_home() -> PathBuf {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let home = std::env::temp_dir().join(format!("marlowe-daemon-seam-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    home
}

struct Fake {
    killed: std::sync::Mutex<Vec<String>>,
}

/// A table that has served nobody and runs commands in no workspace, as a host built for a new
/// session before its first turn.
struct Idle {
    served: std::sync::Mutex<Vec<String>>,
}

impl BgOwner for Idle {
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
        Vec::new()
    }
    fn bg_take(&self, _: &str) -> Vec<BgNotice> {
        Vec::new()
    }
    fn bg_serves(&self, session: &str) -> Serves {
        if self.served.lock().unwrap().iter().any(|s| s == session) {
            Serves::Yes
        } else {
            Serves::No
        }
    }
    fn bg_claimable(&self, workspace: Option<&str>) -> bool {
        workspace.is_none() && self.served.lock().unwrap().is_empty()
    }
    fn bg_claim(&self, session: &str, workspace: Option<&str>) -> bool {
        let mut served = self.served.lock().unwrap();
        if workspace.is_none() && served.is_empty() {
            served.push(session.to_string());
        }
        served.iter().any(|s| s == session)
    }
    fn bg_user_shell(&self, session: &str, command: &str) -> Option<Result<String, String>> {
        self.served.lock().unwrap().iter().any(|s| s == session).then(|| {
            bg_hub().publish(BgEvent::Started(BgInfo {
                id: "jfresh".into(),
                session: session.into(),
                command: command.into(),
                started_ms: 8,
                exit_code: None,
                foreground: false,
                log: None,
            }));
            Ok("jfresh".to_string())
        })
    }
    fn bg_running(&self, _: &str) -> usize {
        0
    }
}

impl BgOwner for Fake {
    fn bg_list(&self, session: Option<&str>) -> Vec<BgInfo> {
        let job = |id: &str, exit: Option<i32>| BgInfo {
            id: id.into(),
            session: "job-s1".into(),
            command: format!("cmd {id}"),
            started_ms: 5,
            exit_code: exit,
            foreground: false,
            log: None,
        };
        let all = vec![job("jx1", None), job("jx2", Some(0))];
        all.into_iter().filter(|j| session.is_none_or(|s| s == j.session)).collect()
    }
    fn bg_kill(&self, id: &str) -> Option<bool> {
        (id == "jx1").then(|| {
            self.killed.lock().unwrap().push(id.into());
            true
        })
    }
    fn bg_detach(&self, _: &str, _: Option<&str>) -> Option<String> {
        None
    }
    fn bg_waking(&self) -> Vec<String> {
        Vec::new()
    }
    fn bg_take(&self, _: &str) -> Vec<BgNotice> {
        Vec::new()
    }
    fn bg_serves(&self, session: &str) -> Serves {
        if session == "job-s1" {
            Serves::Yes
        } else {
            Serves::No
        }
    }
    fn bg_user_shell(&self, session: &str, command: &str) -> Option<Result<String, String>> {
        (session == "job-s1").then(|| {
            bg_hub().publish(BgEvent::Started(BgInfo {
                id: "jx9".into(),
                session: session.into(),
                command: command.into(),
                started_ms: 6,
                exit_code: None,
                foreground: false,
                log: None,
            }));
            Ok("jx9".to_string())
        })
    }
    fn bg_running(&self, _: &str) -> usize {
        0
    }
}

#[test]
fn job_requests_are_answered_and_registry_events_reach_clients() {
    let owner: Arc<dyn BgOwner> = Arc::new(Fake { killed: Default::default() });
    bg_hub().register(Arc::downgrade(&owner));
    let idle: Arc<dyn BgOwner> = Arc::new(Idle { served: Default::default() });
    bg_hub().register(Arc::downgrade(&idle));

    let home = temp_home();
    let deps = DaemonDeps {
        providers: ProviderFactory::new(Arc::new(MemoryStore::new())),
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
    w.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let mut r = LineReader::new(BufReader::new(w.try_clone().unwrap()));
    let mut w = w;
    write_line(&mut w, &Request::Hello { token: info.token.clone(), client: "test".into(), since_seq: None }).unwrap();
    let mut next = || -> Event {
        let env: Envelope = r.next().unwrap().expect("an event");
        env.event
    };

    let s1 = || SessionId("job-s1".into());
    write_line(&mut w, &Request::ListJobs { session: Some(s1()) }).unwrap();
    let list = loop {
        if let Event::Jobs { list } = next() {
            break list;
        }
    };
    assert_eq!(list.iter().map(|j| j.id.as_str()).collect::<Vec<_>>(), ["jx1", "jx2"]);
    assert_eq!(list[0].session, Some(s1()));
    assert_eq!((list[0].exit_code, list[1].exit_code), (None, Some(0)));
    write_line(&mut w, &Request::ListJobs { session: Some(SessionId("other".into())) }).unwrap();
    loop {
        if let Event::Jobs { list } = next() {
            assert!(list.is_empty());
            break;
        }
    }

    // Kill: a running job is stopped; a finished or unknown one is an error that says so.
    write_line(&mut w, &Request::KillJob { id: "jx1".into() }).unwrap();
    write_line(&mut w, &Request::KillJob { id: "jx2".into() }).unwrap();
    write_line(&mut w, &Request::KillJob { id: "nope".into() }).unwrap();
    let mut errors = Vec::new();
    while errors.len() < 2 {
        if let Event::Error { message, .. } = next() {
            errors.push(message);
        }
    }
    assert!(errors[0].contains("jx2") && errors[0].contains("already exited"), "{errors:?}");
    assert!(errors[1].contains("no job `nope`"), "{errors:?}");

    // `!`: the table that serves the session starts it; its events reach the client.
    write_line(&mut w, &Request::UserShell { session: s1(), command: "git status".into() }).unwrap();
    loop {
        if let Event::JobStarted { job } = next() {
            assert_eq!((job.id.as_str(), job.command.as_str()), ("jx9", "git status"));
            break;
        }
    }
    bg_hub().publish(BgEvent::Output { session: "job-s1".into(), id: "jx9".into(), text: "On branch main".into() });
    bg_hub().publish(BgEvent::Exited { session: "job-s1".into(), id: "jx9".into(), code: 0 });
    loop {
        if let Event::JobOutput { session, id, text } = next() {
            assert_eq!((session, id.as_str(), text.as_str()), (Some(s1()), "jx9", "On branch main"));
            break;
        }
    }
    loop {
        if let Event::JobExited { id, code, .. } = next() {
            assert_eq!((id.as_str(), code), ("jx9", 0));
            break;
        }
    }
    // A session the daemon does not know gets an error, and an empty command is refused.
    write_line(&mut w, &Request::UserShell { session: SessionId("elsewhere".into()), command: "ls".into() }).unwrap();
    loop {
        if let Event::Error { message, session } = next() {
            assert!(message.contains("unknown session"), "{message}");
            assert_eq!(session, Some(SessionId("elsewhere".into())));
            break;
        }
    }
    // `!` as the first input of a new session: no turn has run, so no table serves it yet. The
    // daemon loads the session and the idle table for its workspace takes it.
    write_line(&mut w, &Request::NewSession { workspace: None }).unwrap();
    let fresh = loop {
        if let Event::SessionOpened { session, .. } = next() {
            break session;
        }
    };
    write_line(&mut w, &Request::UserShell { session: fresh.clone(), command: "echo first".into() }).unwrap();
    loop {
        match next() {
            Event::JobStarted { job } if job.id == "jfresh" => {
                assert_eq!(job.command, "echo first");
                break;
            }
            Event::Error { message, .. } => panic!("the first `!` of a new session failed: {message}"),
            _ => {}
        }
    }
    write_line(&mut w, &Request::UserShell { session: s1(), command: "  ".into() }).unwrap();
    loop {
        if let Event::Error { message, .. } = next() {
            assert!(message.contains("needs a command"), "{message}");
            break;
        }
    }
    write_line(&mut w, &Request::Shutdown).unwrap();
    join.join().unwrap().unwrap();
    drop(owner);
}
