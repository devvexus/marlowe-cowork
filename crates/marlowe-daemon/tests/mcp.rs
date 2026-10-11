//! MCP over the real wire: `McpAdd`/`McpList`/`McpRemove`, and a session whose conductor calls an
//! MCP tool (it asks, because that is the default) from a server on loopback.

use std::collections::VecDeque;
use std::io::BufReader;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use marlowe_agent::{Consequence, ToolCallReq, ToolCtx, ToolHost, ToolOutcome, UserInfo};
use marlowe_brains::{Library, MemoryStore, ProviderFactory};
use marlowe_daemon::{Daemon, DaemonConfig, DaemonDeps};
use marlowe_mcp::fake::{FakeHttp, FakeOpts, HttpOpts};
use marlowe_model::{
    CancelToken, ChatRequest, Completion, CompletionBuilder, ModelInfo, Provider, ProviderError, ProviderKind, StopReason, StreamEvent,
    ToolSpec, Usage,
};
use marlowe_proto::{paths, write_line, ApprovalDecision, DaemonInfo, Envelope, Event, LineReader, Request, SettingsScope, TurnStop};
use serde_json::json;

#[derive(Default)]
struct Scripted {
    scripts: Mutex<VecDeque<Vec<StreamEvent>>>,
    requests: Mutex<Vec<ChatRequest>>,
}

impl Provider for Scripted {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Ollama
    }
    fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        let mut m = ModelInfo::bare(ProviderKind::Ollama, "scripted");
        m.supports_tools = true;
        m.context_window = Some(32_768);
        Ok(vec![m])
    }
    fn stream(&self, req: &ChatRequest, _: &CancelToken, on: &mut dyn FnMut(StreamEvent)) -> Result<Completion, ProviderError> {
        self.requests.lock().unwrap().push(req.clone());
        let script = self.scripts.lock().unwrap().pop_front().unwrap_or_else(|| vec![StreamEvent::TextDelta("ok".into())]);
        let mut b = CompletionBuilder::new();
        for e in script {
            b.push(&e);
            on(e);
        }
        Ok(b.finish())
    }
}

fn call_step(id: &str, name: &str, args: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ToolCallStart { index: 0, id: id.into(), name: name.into() },
        StreamEvent::ToolArgsDelta { index: 0, json: args.into() },
        StreamEvent::ToolCallEnd { index: 0 },
        StreamEvent::Usage(Usage { input: 10, output: 5, ..Default::default() }),
        StreamEvent::Stop(StopReason::ToolUse),
    ]
}

fn say(text: &str) -> Vec<StreamEvent> {
    vec![StreamEvent::TextDelta(text.into()), StreamEvent::Usage(Usage { input: 10, output: 5, ..Default::default() }), StreamEvent::Stop(StopReason::EndTurn)]
}

struct Builtins;
impl ToolHost for Builtins {
    fn specs(&self) -> Vec<ToolSpec> {
        Vec::new()
    }
    fn call(&self, _: &ToolCallReq, _: &ToolCtx) -> ToolOutcome {
        ToolOutcome::error("none")
    }
    fn consequence(&self, _: &ToolCallReq) -> Consequence {
        Consequence::Free
    }
}

fn temp_home(tag: &str) -> PathBuf {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let h = std::env::temp_dir().join(format!("marlowe-daemon-mcp-{tag}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&h).unwrap();
    h
}

fn start(home: &Path, provider: Arc<Scripted>) -> (DaemonInfo, std::thread::JoinHandle<Result<(), marlowe_daemon::DaemonError>>) {
    let lib = Library::open(&paths::cowork_db(home)).unwrap();
    let mut info = ModelInfo::bare(ProviderKind::Ollama, "scripted");
    info.supports_tools = true;
    info.context_window = Some(32_768);
    lib.add(&info).unwrap();
    let mut providers = ProviderFactory::new(Arc::new(MemoryStore::new()));
    providers.register(ProviderKind::Ollama, move |_| Ok(provider.clone() as Arc<dyn Provider>));
    let deps = DaemonDeps {
        providers,
        tools: Arc::new(|_| Arc::new(Builtins) as Arc<dyn ToolHost>),
        user: UserInfo { name: Some("Tester".into()), address: None },
        mind: None,
        sentinel: None,
        voice: None,
        crew: None,
    };
    let d = Daemon::start(deps, home, DaemonConfig { legacy_offer: false, ..DaemonConfig::default() }).expect("daemon starts");
    let info = d.info();
    (info, std::thread::spawn(move || d.wait()))
}

struct Client {
    w: TcpStream,
    r: LineReader<BufReader<TcpStream>>,
}

impl Client {
    fn connect(info: &DaemonInfo) -> Client {
        let w = TcpStream::connect(info.addr()).unwrap();
        w.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        let mut c = Client { r: LineReader::new(BufReader::new(w.try_clone().unwrap())), w };
        c.send(&Request::Hello { token: info.token.clone(), client: "test".into(), since_seq: None });
        c.until(|e| matches!(e, Event::Welcome { .. }));
        c
    }
    fn send(&mut self, r: &Request) {
        write_line(&mut self.w, r).unwrap();
    }
    fn until(&mut self, pred: impl Fn(&Event) -> bool) -> Vec<Envelope> {
        let end = Instant::now() + Duration::from_secs(30);
        let mut out = Vec::new();
        while Instant::now() < end {
            let env: Envelope = self.r.next().unwrap().expect("an event");
            let hit = pred(&env.event);
            out.push(env);
            if hit {
                return out;
            }
        }
        panic!("timed out; saw {out:#?}");
    }
}

fn stop(info: &DaemonInfo, join: std::thread::JoinHandle<Result<(), marlowe_daemon::DaemonError>>) {
    let mut c = Client::connect(info);
    c.send(&Request::Shutdown);
    c.until(|e| matches!(e, Event::ShuttingDown));
    join.join().unwrap().unwrap();
}

#[test]
fn parity_x15_servers_are_added_listed_and_removed_over_the_wire() {
    let home = temp_home("crud");
    let (info, join) = start(&home, Arc::new(Scripted::default()));
    let mut c = Client::connect(&info);
    let entry = json!({"type": "http", "url": "http://127.0.0.1:9/mcp", "consequence": "inert"});

    c.send(&Request::McpAdd { name: "web".into(), scope: SettingsScope::User, config: entry.clone(), workspace: None });
    let list = c.until(|e| matches!(e, Event::McpServers { .. })).pop().unwrap().event;
    match list {
        Event::McpServers { list } => {
            assert_eq!(list.len(), 1);
            assert_eq!((list[0].name.as_str(), list[0].transport.as_str(), list[0].scope), ("web", "http", SettingsScope::User));
            assert!(list[0].status.starts_with("idle"), "nothing is started to answer a list: {}", list[0].status);
        }
        e => panic!("{e:?}"),
    }
    let text = std::fs::read_to_string(home.join("mcp.json")).unwrap();
    assert!(text.contains("\"web\"") && text.contains("127.0.0.1:9"), "{text}");

    // A bad entry is refused with the reason, and the connection is still good.
    c.send(&Request::McpAdd { name: "bad".into(), scope: SettingsScope::User, config: json!({"type": "http"}), workspace: None });
    match c.until(|e| matches!(e, Event::Error { .. })).pop().unwrap().event {
        Event::Error { message, .. } => assert!(message.contains("url"), "{message}"),
        e => panic!("{e:?}"),
    }
    // The project scope needs a workspace.
    c.send(&Request::McpAdd { name: "p".into(), scope: SettingsScope::Project, config: entry.clone(), workspace: None });
    match c.until(|e| matches!(e, Event::Error { .. })).pop().unwrap().event {
        Event::Error { message, .. } => assert!(message.contains("workspace"), "{message}"),
        e => panic!("{e:?}"),
    }

    // Project scope into a workspace: added trusted, listed with its scope.
    let ws = temp_home("ws");
    c.send(&Request::McpAdd { name: "proj".into(), scope: SettingsScope::Project, config: json!({"command": "x"}), workspace: Some(ws.display().to_string()) });
    c.until(|e| matches!(e, Event::McpServers { .. }));
    c.send(&Request::McpList { workspace: Some(ws.display().to_string()) });
    match c.until(|e| matches!(e, Event::McpServers { .. })).pop().unwrap().event {
        Event::McpServers { list } => {
            let proj = list.iter().find(|s| s.name == "proj").unwrap();
            assert_eq!(proj.scope, SettingsScope::Project);
            assert!(proj.status.starts_with("idle"), "added by the user, so trusted: {}", proj.status);
            assert!(list.iter().any(|s| s.name == "web"), "the user scope shows in every workspace");
        }
        e => panic!("{e:?}"),
    }
    // An entry that arrived in the repository's own file is not trusted.
    std::fs::write(ws.join(".mcp.json"), json!({"mcpServers": {"cloned": {"command": "x"}}}).to_string()).unwrap();
    c.send(&Request::McpList { workspace: Some(ws.display().to_string()) });
    match c.until(|e| matches!(e, Event::McpServers { .. })).pop().unwrap().event {
        Event::McpServers { list } => assert_eq!(list.iter().find(|s| s.name == "cloned").unwrap().status, "needs trust"),
        e => panic!("{e:?}"),
    }

    c.send(&Request::McpRemove { name: "web".into(), scope: SettingsScope::User, workspace: None });
    match c.until(|e| matches!(e, Event::McpServers { .. })).pop().unwrap().event {
        Event::McpServers { list } => assert!(list.is_empty()),
        e => panic!("{e:?}"),
    }
    c.send(&Request::McpRemove { name: "web".into(), scope: SettingsScope::User, workspace: None });
    match c.until(|e| matches!(e, Event::Error { .. })).pop().unwrap().event {
        Event::Error { message, .. } => assert!(message.contains("no server named web"), "{message}"),
        e => panic!("{e:?}"),
    }
    stop(&info, join);
}

fn approve_all(c: &mut Client) -> Vec<Envelope> {
    let mut all = Vec::new();
    loop {
        let batch = c.until(|e| matches!(e, Event::ApprovalRequested { .. } | Event::TurnFinished { .. }));
        let last = batch.last().unwrap().event.clone();
        all.extend(batch);
        match last {
            Event::ApprovalRequested { approval, .. } => c.send(&Request::Approve { approval, decision: ApprovalDecision::Allow }),
            _ => return all,
        }
    }
}

#[test]
fn parity_x12_a_session_calls_an_mcp_tool_after_the_user_allows_it() {
    let server = FakeHttp::start(HttpOpts::default());
    let home = temp_home("turn");
    std::fs::write(home.join("mcp.json"), json!({"mcpServers": {"web": {"type": "http", "url": server.url()}}}).to_string()).unwrap();
    let provider = Arc::new(Scripted::default());
    provider.scripts.lock().unwrap().push_back(call_step("c1", "mcp__web__echo", r#"{"text":"from the model"}"#));
    provider.scripts.lock().unwrap().push_back(say("done"));
    let (info, join) = start(&home, provider.clone());
    let mut c = Client::connect(&info);
    c.send(&Request::Ask { session: None, text: "use the web server".into(), workspace: None, images: Vec::new(), options: None });
    let events = approve_all(&mut c);

    let asked = events.iter().find_map(|e| match &e.event {
        Event::ApprovalRequested { tool, summary, .. } => Some((tool.clone(), summary.clone())),
        _ => None,
    });
    let (tool, summary) = asked.expect("an MCP tool asks by default (ADR-052)");
    assert_eq!(tool, "mcp__web__echo");
    assert!(summary.contains("echo") && summary.contains("web"), "{summary}");
    assert!(events.iter().any(|e| matches!(&e.event, Event::ToolFinished { ok: true, .. })));
    assert!(events.iter().any(|e| matches!(&e.event, Event::TurnFinished { stop: TurnStop::Done, .. })));

    let reqs = provider.requests.lock().unwrap();
    assert!(reqs[0].tools.iter().any(|t| t.name == "mcp__web__echo"), "the tool is in the first request");
    // The tool's result went back to the model, and the conversation is now tainted.
    let last = reqs[1].messages.last().unwrap();
    assert!(format!("{last:?}").contains("from the model"), "{last:?}");
    drop(reqs);
    stop(&info, join);
}

#[test]
fn parity_t31_eighty_tools_reach_the_model_as_one_search_tool() {
    let server = FakeHttp::start(HttpOpts { mcp: FakeOpts { extra_tools: 72, ..Default::default() }, ..Default::default() });
    let home = temp_home("search");
    std::fs::write(home.join("mcp.json"), json!({"mcpServers": {"web": {"type": "http", "url": server.url(), "consequence": "inert"}}}).to_string()).unwrap();
    let provider = Arc::new(Scripted::default());
    provider.scripts.lock().unwrap().push_back(call_step("c1", "tool_search", r#"{"query":"select:tool_003"}"#));
    provider.scripts.lock().unwrap().push_back(call_step("c2", "mcp__web__tool_003", r#"{"id":"7"}"#));
    provider.scripts.lock().unwrap().push_back(say("found it"));
    let (info, join) = start(&home, provider.clone());
    let mut c = Client::connect(&info);
    c.send(&Request::Ask { session: None, text: "look something up".into(), workspace: None, images: Vec::new(), options: None });
    let events = approve_all(&mut c);
    assert!(events.iter().any(|e| matches!(&e.event, Event::TurnFinished { stop: TurnStop::Done, .. })), "{events:#?}");

    {
        let reqs = provider.requests.lock().unwrap();
        let names = |i: usize| reqs[i].tools.iter().map(|t| t.name.clone()).collect::<Vec<_>>();
        assert!(names(0).contains(&"tool_search".to_string()), "{:?}", names(0));
        assert!(!names(0).iter().any(|n| n.starts_with("mcp__web__")), "80 tools are not listed up front: {:?}", names(0));
        assert_eq!(reqs.len(), 3);
    }
    // The call by name worked in the same turn (the loop lists tools once per turn, so the schema
    // joins the list from the next turn on).
    let finished: Vec<bool> = events.iter().filter_map(|e| if let Event::ToolFinished { ok, .. } = &e.event { Some(*ok) } else { None }).collect();
    assert_eq!(finished, [true, true], "tool_search and the loaded tool both ran");
    provider.scripts.lock().unwrap().push_back(say("again"));
    let session = events.iter().find_map(|e| if let Event::TurnStarted { session, .. } = &e.event { Some(session.clone()) } else { None }).unwrap();
    c.send(&Request::Ask { session: Some(session), text: "and again".into(), workspace: None, images: Vec::new(), options: None });
    approve_all(&mut c);
    let reqs = provider.requests.lock().unwrap();
    assert!(reqs[3].tools.iter().any(|t| t.name == "mcp__web__tool_003"), "the loaded tool is listed from the next turn");
    drop(reqs);
    stop(&info, join);
}
