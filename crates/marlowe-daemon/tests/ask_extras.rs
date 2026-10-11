//! What an `Ask` carries beside its words reaches the turn: attached images join the user's
//! message (U3, T2), and `marlowe -p`'s options apply to it (H4): an appended system prompt, a
//! step cap and per-run deny rules. The provider is scripted, so nothing here needs a model.

use std::io::BufReader;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use marlowe_agent::{Consequence, ToolCallReq, ToolCtx, ToolHost, ToolOutcome, UserInfo};
use marlowe_brains::{Library, MemoryStore, ProviderFactory};
use marlowe_daemon::{Daemon, DaemonConfig, DaemonDeps};
use marlowe_model::{
    CancelToken, ChatRequest, Completion, CompletionBuilder, ImageRef, ModelInfo, Part, Provider, ProviderError,
    ProviderKind, Role, StopReason, StreamEvent, ToolSpec, Usage,
};
use marlowe_proto::{paths, write_line, Envelope, Event, HeadlessOptions, LineReader, Request};

/// Answers with one `echo` call on the first request of a turn that offers it, then text.
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
        let last_is_user = req.messages.last().is_some_and(|m| m.role == Role::User);
        let mut events = Vec::new();
        if last_is_user && req.tools.iter().any(|t| t.name == "echo") {
            events.push(StreamEvent::ToolCallStart { index: 0, id: "e1".into(), name: "echo".into() });
            events.push(StreamEvent::ToolArgsDelta { index: 0, json: "{\"text\":\"hi\"}".into() });
            events.push(StreamEvent::ToolCallEnd { index: 0 });
            events.push(StreamEvent::Stop(StopReason::ToolUse));
        } else {
            events.push(StreamEvent::TextDelta("seen".into()));
            events.push(StreamEvent::Stop(StopReason::EndTurn));
        }
        events.insert(0, StreamEvent::Usage(Usage { input: 10, output: 5, ..Default::default() }));
        let mut b = CompletionBuilder::new();
        for e in events {
            b.push(&e);
            on(e);
        }
        Ok(b.finish())
    }
}

/// One free tool that records its runs.
#[derive(Default)]
struct Echo {
    ran: Mutex<u32>,
}

impl ToolHost for Echo {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec { name: "echo".into(), description: "echo".into(), input_schema: serde_json::json!({"type": "object"}) }]
    }
    fn call(&self, _: &ToolCallReq, _: &ToolCtx) -> ToolOutcome {
        *self.ran.lock().unwrap() += 1;
        ToolOutcome::ok("echoed", "echoed")
    }
    fn consequence(&self, _: &ToolCallReq) -> Consequence {
        Consequence::Free
    }
}

fn temp_home(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let home = std::env::temp_dir().join(format!("marlowe-daemon-extras-{tag}-{}-{nanos}", std::process::id()));
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

/// Starts a daemon, sends one Ask, waits for its TurnFinished; returns the provider's requests
/// and how often `echo` ran.
fn one_ask(tag: &str, images: Vec<ImageRef>, options: Option<HeadlessOptions>) -> (Vec<ChatRequest>, u32) {
    let home = temp_home(tag);
    seed_model(&home);
    let provider = Arc::new(Scripted::default());
    let echo = Arc::new(Echo::default());
    let mut providers = ProviderFactory::new(Arc::new(MemoryStore::new()));
    let p = provider.clone();
    providers.register(ProviderKind::Ollama, move |_| Ok(p.clone() as Arc<dyn Provider>));
    let host = echo.clone();
    let deps = DaemonDeps {
        providers,
        tools: Arc::new(move |_| host.clone() as Arc<dyn ToolHost>),
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
    write_line(&mut w, &Request::Ask { session: None, text: "look at this".into(), workspace: None, images, options }).unwrap();
    loop {
        let env: Envelope = r.next().unwrap().expect("an event");
        if matches!(env.event, Event::TurnFinished { .. }) {
            break;
        }
    }
    write_line(&mut w, &Request::Shutdown).unwrap();
    join.join().unwrap().unwrap();
    let reqs = provider.requests.lock().unwrap().clone();
    let ran = *echo.ran.lock().unwrap();
    (reqs, ran)
}

#[test]
fn parity_u3_an_attached_image_joins_the_users_message() {
    let img = ImageRef { mime: "image/png".into(), blob: "ab".repeat(32), w: 64, h: 48 };
    let (reqs, _) = one_ask("img", vec![img.clone()], None);
    let first = &reqs[0];
    let user = first.messages.iter().rev().find(|m| m.role == Role::User).expect("the user's message");
    assert!(user.content.iter().any(|p| matches!(p, Part::Text { text } if text.contains("look at this"))), "{user:?}");
    assert!(user.content.iter().any(|p| *p == Part::Image(img.clone())), "the image rides with the words: {user:?}");
}

#[test]
fn parity_h4_options_reach_the_turn_append_a_prompt_and_deny_a_tool() {
    let options = HeadlessOptions {
        append_system_prompt: Some("ANSWER-IN-HAIKU".into()),
        disallowed_tools: vec!["echo".into()],
        ..HeadlessOptions::default()
    };
    let (reqs, ran) = one_ask("opts", Vec::new(), Some(options));
    let system: String = reqs[0].system.iter().map(|b| b.text.clone()).collect::<Vec<_>>().join("\n");
    assert!(system.contains("ANSWER-IN-HAIKU"), "{system}");
    assert_eq!(ran, 0, "a denied tool does not run, even one that needs no approval");
    let fed_back = format!("{:?}", reqs.last().unwrap().messages);
    assert!(fed_back.contains("denied by the rule"), "the reason reaches the model: {fed_back}");
}
