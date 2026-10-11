//! Every M4.6 request is routed: until its track lands, the stub answers this client with an
//! `Error` naming the request and the track, and the connection stays usable.

use std::io::BufReader;
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use marlowe_agent::{Consequence, ToolCallReq, ToolCtx, ToolHost, ToolOutcome, UserInfo};
use marlowe_brains::{MemoryStore, ProviderFactory};
use marlowe_daemon::{Daemon, DaemonConfig, DaemonDeps};
use marlowe_model::ToolSpec;
use marlowe_proto::{
    write_line, Envelope, Event, ExportFormat, LineReader, PermissionMode, Request, RewindScope, RunId, SessionId, SettingsScope,
};

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

#[test]
fn every_m46_request_reaches_its_tracks_stub() {
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

    let s = || SessionId("s-1".into());
    let cases: Vec<(Request, &str, &str)> = vec![
        (Request::SetMode { session: s(), mode: PermissionMode::Plan }, "set_mode", "E"),
        (Request::ListRules { workspace: None }, "list_rules", "E"),
        (Request::GetSettings { workspace: None }, "get_settings", "E"),
        (Request::Compact { session: s(), focus: None }, "compact", "F"),
        (Request::DetachCall { session: s(), call_id: "c".into() }, "detach_call", "F"),
        (Request::ListSchedules, "list_schedules", "F"),
        (Request::ListExperiments { limit: 5 }, "list_experiments", "J2"),
        (Request::PutBlob { mime: "image/png".into(), base64: "iVBORw0KGgo=".into() }, "put_blob", "G"),
    ];
    for (req, op, track) in cases {
        write_line(&mut w, &req).unwrap();
        let message = loop {
            let env: Envelope = r.next().unwrap().expect("an event");
            if let Event::Error { message, session } = env.event {
                if message.contains(op) {
                    if req_has_session(&req) {
                        assert_eq!(session, Some(s()), "{op}: the error names its session");
                    }
                    break message;
                }
            }
        };
        assert!(message.contains(&format!("track {track}")), "{op}: {message}");
    }

    // The connection is still good.
    write_line(&mut w, &Request::Status).unwrap();
    loop {
        let env: Envelope = r.next().unwrap().expect("an event");
        if matches!(env.event, Event::Status(_)) {
            break;
        }
    }
    write_line(&mut w, &Request::Shutdown).unwrap();
    join.join().unwrap().unwrap();
}

fn req_has_session(r: &Request) -> bool {
    serde_json::to_value(r).unwrap().get("session").is_some()
}
