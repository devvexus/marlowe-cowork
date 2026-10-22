//! Background work is never silent: a voice model loading (the Chatterbox sidecar, tens of seconds
//! of GPU memory) is announced on the bus as a `Work` phase for as long as it loads, and ends with
//! the measured time. A fake engine stands in for the sidecar; nothing is spawned.

use std::io::BufReader;
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use marlowe_agent::{Consequence, ToolCallReq, ToolCtx, ToolHost, ToolOutcome, UserInfo};
use marlowe_brains::{MemoryStore, ProviderFactory};
use marlowe_daemon::{Daemon, DaemonConfig, DaemonDeps, VoiceLink};
use marlowe_model::ToolSpec;
use marlowe_proto::{write_line, Envelope, Event, LineReader, PhaseKind, PhaseState, Request};
use marlowe_voice::{Backend, Voice, VoiceError, VoiceGender, VoiceInfo};

struct NoTools;

impl ToolHost for NoTools {
    fn specs(&self) -> Vec<ToolSpec> {
        Vec::new()
    }
    fn call(&self, _call: &ToolCallReq, _ctx: &ToolCtx) -> ToolOutcome {
        ToolOutcome::ok("x", "x")
    }
    fn consequence(&self, _call: &ToolCallReq) -> Consequence {
        Consequence::Free
    }
}

/// An engine whose model "loads" while the shared marker is set.
struct Loading {
    flag: Arc<AtomicU64>,
}

impl Backend for Loading {
    fn voices(&self) -> Vec<VoiceInfo> {
        vec![VoiceInfo { id: "v".into(), name: "Fake".into(), language: "en-GB".into(), gender: VoiceGender::Unknown }]
    }
    fn synthesize(&mut self, _text: &str, _voice_id: &str) -> Result<Vec<u8>, VoiceError> {
        Err(VoiceError::Unavailable)
    }
    fn loading_flag(&self) -> Option<Arc<AtomicU64>> {
        Some(self.flag.clone())
    }
    fn play(&mut self, _wav: Vec<u8>) -> Result<(), VoiceError> {
        Ok(())
    }
    fn stop(&mut self) {}
    fn is_playing(&self) -> bool {
        false
    }
}

fn temp_home() -> PathBuf {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let home = std::env::temp_dir().join(format!("marlowe-daemon-bgwork-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    home
}

fn until(r: &mut LineReader<BufReader<TcpStream>>, pred: impl Fn(&Event) -> bool) -> Envelope {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        let env = r.next::<Envelope>().expect("read").expect("open");
        if pred(&env.event) {
            return env;
        }
    }
    panic!("timed out");
}

#[test]
fn a_voice_model_loading_is_announced_while_it_loads_and_ends_with_its_time() {
    let home = temp_home();
    let flag = Arc::new(AtomicU64::new(0));
    let (tx, rx) = crossbeam_channel::bounded(16);
    let voice = Voice::with_backend(&home, tx, Some(Box::new(Loading { flag: flag.clone() })));
    let deps = DaemonDeps {
        providers: ProviderFactory::new(Arc::new(MemoryStore::new())),
        tools: Arc::new(|_| Arc::new(NoTools) as Arc<dyn ToolHost>),
        user: UserInfo { name: None, address: None },
        mind: None,
        sentinel: None,
        voice: Some(VoiceLink { voice, events: rx }),
        crew: None,
    };
    let d = Daemon::start(deps, &home, DaemonConfig { legacy_offer: false, ..DaemonConfig::default() }).expect("daemon starts");
    let info = d.info();
    let join = std::thread::spawn(move || d.wait());

    let w = TcpStream::connect(info.addr()).unwrap();
    w.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let mut writer = w.try_clone().unwrap();
    let mut r = LineReader::new(BufReader::new(w));
    write_line(&mut writer, &Request::Hello { token: info.token.clone(), client: "test".into(), since_seq: None }).unwrap();
    until(&mut r, |e| matches!(e, Event::Welcome { .. }));

    // Nothing loads: nothing is announced.
    std::thread::sleep(Duration::from_millis(500));
    flag.store(1_800_000_000_000, Ordering::SeqCst);
    let start = until(&mut r, |e| matches!(e, Event::Phase(p) if p.kind == PhaseKind::Work));
    let Event::Phase(p) = start.event else { unreachable!() };
    assert_eq!((p.state, p.stage.as_deref(), p.session.is_none()), (PhaseState::Start, Some("voice"), true));
    assert_eq!(p.detail.as_deref(), Some("loading the voice model"));

    flag.store(0, Ordering::SeqCst);
    let end = until(&mut r, |e| matches!(e, Event::Phase(p) if p.kind == PhaseKind::Work && p.state == PhaseState::End));
    let Event::Phase(p) = end.event else { unreachable!() };
    assert_eq!(p.stage.as_deref(), Some("voice"));
    assert!(p.ms.is_some(), "the end carries the measured time");

    write_line(&mut writer, &Request::Shutdown).unwrap();
    until(&mut r, |e| matches!(e, Event::ShuttingDown));
    join.join().unwrap().unwrap();
    let _ = std::fs::remove_dir_all(&home);
}
