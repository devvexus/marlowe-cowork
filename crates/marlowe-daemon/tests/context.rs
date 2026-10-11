//! The context planner over the real wire: per-call usage for the meter, policies saved and
//! broadcast, the agent window following the policy, and the Ollama restart that only ever
//! happens after an Allow (against a server control that records instead of restarting).

use std::io::BufReader;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use marlowe_agent::{Consequence, ToolCallReq, ToolCtx, ToolHost, ToolOutcome, UserInfo};
use marlowe_brains::{Hardware, Library, MemoryStore, ProviderFactory};
use marlowe_daemon::{Daemon, DaemonConfig, DaemonDeps};
use marlowe_hw::{
    GpuBudget, GpuSnapshot, GpuSource, ModelFiles, ModelHeader, ModelSource, RunnerObservation, ServerChange, ServerControl,
    ServerEnv,
};
use marlowe_model::{
    CancelToken, ChatRequest, Completion, CompletionBuilder, ModelInfo, Provider, ProviderError, ProviderKind, StopReason,
    StreamEvent, ToolSpec, Usage,
};
use marlowe_proto::{
    paths, write_line, ApprovalDecision, ContextPolicy, DaemonInfo, Envelope, Event, KvType, LineReader, PolicySource, Request,
    SERVER_RESTART_TOOL,
};

struct Echo;
impl Provider for Echo {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Ollama
    }
    fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(vec![info()])
    }
    fn stream(&self, _: &ChatRequest, _: &CancelToken, on: &mut dyn FnMut(StreamEvent)) -> Result<Completion, ProviderError> {
        let mut b = CompletionBuilder::new();
        for e in [
            StreamEvent::TextDelta("ok".into()),
            StreamEvent::Usage(Usage { input: 1200, output: 34, ..Default::default() }),
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

fn info() -> ModelInfo {
    let mut i = ModelInfo::bare(ProviderKind::Ollama, "llama3:8b");
    i.context_window = Some(8_192);
    i
}

struct Models;
impl ModelSource for Models {
    fn files(&self, _: &str) -> Result<ModelFiles, String> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../marlowe-hw/tests/fixtures/llama3_8b_q4_0.json");
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        Ok(ModelFiles { main: ModelHeader::from_show(&v)?, source: "GGUF header".into(), ..Default::default() })
    }
    fn runtime(&self, _: &str, _: Option<&str>) -> RunnerObservation {
        RunnerObservation {
            saved: ServerEnv { kv_cache_type: Some("q8_0".into()), num_parallel: Some("1".into()), flash_attention: Some("1".into()) },
            ..Default::default()
        }
    }
}

struct Gpu;
impl GpuSource for Gpu {
    fn snapshot(&self) -> Option<GpuSnapshot> {
        Some(GpuSnapshot {
            gpu: GpuBudget {
                name: "test GPU".into(),
                total_mib: 16_376.0,
                used_mib: 1_300.0,
                free_mib: 15_076.0,
                runners_mib: 0.0,
                runners_shared_mib: 0.0,
                headroom_mib: 1_024.0,
                bandwidth_gbs: Some(736.0),
            },
            runners: vec![],
            ram_total_mib: None,
            ram_free_mib: None,
        })
    }
}

#[derive(Default)]
struct Recorder(Mutex<Vec<ServerChange>>);
impl ServerControl for Recorder {
    fn apply(&self, c: &ServerChange) -> Result<String, String> {
        self.0.lock().unwrap().push(*c);
        Ok("set OLLAMA_KV_CACHE_TYPE=q4_0 and restarted Ollama".into())
    }
}

fn home(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let h = std::env::temp_dir().join(format!("marlowe-ctx-it-{name}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&h).unwrap();
    Library::open(&paths::cowork_db(&h)).unwrap().add(&info()).unwrap();
    h
}

fn start(home: &Path, server: Arc<Recorder>) -> (DaemonInfo, std::thread::JoinHandle<()>) {
    start_on(home, server, Arc::new(Gpu))
}

fn start_on(home: &Path, server: Arc<Recorder>, gpu: Arc<dyn GpuSource>) -> (DaemonInfo, std::thread::JoinHandle<()>) {
    let mut providers = ProviderFactory::new(Arc::new(MemoryStore::new()));
    providers.register(ProviderKind::Ollama, |_| Ok(Arc::new(Echo) as Arc<dyn Provider>));
    providers.set_hardware(Hardware { models: Arc::new(Models), gpu, server });
    let deps = DaemonDeps {
        providers,
        tools: Arc::new(|_| Arc::new(NoTools) as Arc<dyn ToolHost>),
        user: UserInfo { name: None, address: None },
        mind: None,
        sentinel: None,
        voice: None,
        crew: None,
    };
    let d = Daemon::start(deps, home, DaemonConfig { legacy_offer: false, ..DaemonConfig::default() }).unwrap();
    let info = d.info();
    (info, std::thread::spawn(move || d.wait().unwrap()))
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
    fn until(&mut self, pred: impl Fn(&Event) -> bool) -> Vec<Event> {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut out = Vec::new();
        while Instant::now() < deadline {
            let env: Envelope = self.r.next().unwrap().expect("stream open");
            let hit = pred(&env.event);
            out.push(env.event);
            if hit {
                return out;
            }
        }
        panic!("timed out: {out:#?}");
    }
}

#[test]
fn usage_policies_and_the_approval_gated_restart() {
    let h = home("all");
    let server = Arc::new(Recorder::default());
    let (info, join) = start(&h, server.clone());
    let mut c = Client::connect(&info);
    let model: marlowe_model::ModelRef = "ollama/llama3:8b".parse().unwrap();

    // Each model call reports the context it holds, out of the agent's window.
    c.send(&Request::Ask { session: None, text: "hi".into(), workspace: None, images: Vec::new(), options: None });
    let evs = c.until(|e| matches!(e, Event::TurnFinished { .. }));
    assert!(
        evs.iter().any(|e| matches!(e, Event::ContextUsage { tokens: 1234, window: 8_192, .. })),
        "{evs:#?}"
    );

    // The plan, and the policies list.
    c.send(&Request::ContextPlan { model: model.clone() });
    let plan = match c.until(|e| matches!(e, Event::ContextPlan(_))).pop().unwrap() {
        Event::ContextPlan(p) => p,
        _ => unreachable!(),
    };
    assert_eq!(plan.arch.summary, "32 layers, 32 keep a KV cache");
    assert_eq!(plan.ctx_limit, 8_192);
    assert!(plan.plan.as_ref().unwrap().agents(8_192, KvType::Q8_0).unwrap() >= 1);

    // A window above the model's maximum is refused; a good one is saved and broadcast.
    let too_big = ContextPolicy { max_ctx: 16_384, kv_type: KvType::Q8_0, parallel: 1, source: PolicySource::User, updated_ms: 0 };
    c.send(&Request::SetContextPolicy { model: model.clone(), policy: too_big });
    let err = c.until(|e| matches!(e, Event::Error { .. }));
    assert!(matches!(err.last(), Some(Event::Error { message, .. }) if message.contains("at most 8192")), "{err:?}");
    let good = ContextPolicy { max_ctx: 4_096, kv_type: KvType::Q4_0, ..too_big };
    c.send(&Request::SetContextPolicy { model: model.clone(), policy: good });
    let saved = c.until(|e| matches!(e, Event::ContextPolicies { .. }));
    let pending = saved.iter().find_map(|e| match e {
        Event::ContextPolicySaved { pending_restart, changes, .. } => Some((*pending_restart, changes.len())),
        _ => None,
    });
    assert_eq!(pending, Some((true, 3)), "{saved:#?}");

    // The next turn's agent compacts against the policy's window.
    c.send(&Request::Ask { session: None, text: "again".into(), workspace: None, images: Vec::new(), options: None });
    let evs = c.until(|e| matches!(e, Event::TurnFinished { .. }));
    assert!(evs.iter().any(|e| matches!(e, Event::ContextUsage { window: 4_096, .. })), "{evs:#?}");

    // A restart asks first. Denied: nothing runs, and the manual steps come back.
    c.send(&Request::ApplyOllamaServer { kv_type: Some(KvType::Q4_0), parallel: None, flash_attn: None });
    let asked = c.until(|e| matches!(e, Event::ApprovalRequested { .. }));
    let Some(Event::ApprovalRequested { approval, tool, detail, .. }) = asked.last() else { unreachable!() };
    assert_eq!(tool, SERVER_RESTART_TOOL);
    assert!(detail.contains("every Ollama model") && detail.contains("setx OLLAMA_KV_CACHE_TYPE q4_0"), "{detail}");
    c.send(&Request::Approve { approval: *approval, decision: ApprovalDecision::Deny });
    let done = c.until(|e| matches!(e, Event::ServerApplied { .. }));
    assert!(matches!(done.last(), Some(Event::ServerApplied { ok: false, message }) if message.contains("setx")));
    assert!(server.0.lock().unwrap().is_empty(), "denied means not run");

    // Allowed: the control is called with exactly that change.
    c.send(&Request::ApplyOllamaServer { kv_type: Some(KvType::Q4_0), parallel: None, flash_attn: None });
    let asked = c.until(|e| matches!(e, Event::ApprovalRequested { .. }));
    let Some(Event::ApprovalRequested { approval, .. }) = asked.last() else { unreachable!() };
    c.send(&Request::Approve { approval: *approval, decision: ApprovalDecision::Allow });
    let done = c.until(|e| matches!(e, Event::ServerApplied { .. }));
    assert!(matches!(done.last(), Some(Event::ServerApplied { ok: true, .. })));
    assert_eq!(server.0.lock().unwrap().as_slice(), &[ServerChange { kv_type: Some(KvType::Q4_0), parallel: None, flash_attn: None }]);

    c.send(&Request::Shutdown);
    c.until(|e| matches!(e, Event::ShuttingDown));
    join.join().unwrap();
    let _ = std::fs::remove_dir_all(&h);
}

/// The same card with something else holding all but 3 GiB.
struct TightGpu;
impl GpuSource for TightGpu {
    fn snapshot(&self) -> Option<GpuSnapshot> {
        let mut s = Gpu.snapshot()?;
        s.gpu.used_mib = 13_376.0;
        s.gpu.free_mib = 3_000.0;
        Some(s)
    }
}

/// A conductor model that is not loaded and does not fit beside what is: the turn asks first,
/// with the planner's cost, and a denial calls no model. Allowed once, the same load is not
/// asked again.
#[test]
fn a_cold_load_that_does_not_fit_asks_with_its_cost() {
    let h = home("tight");
    let (info, join) = start_on(&h, Arc::default(), Arc::new(TightGpu));
    let mut c = Client::connect(&info);

    c.send(&Request::Ask { session: None, text: "hi".into(), workspace: None, images: Vec::new(), options: None });
    let asked = c.until(|e| matches!(e, Event::ApprovalRequested { .. }));
    let Some(Event::ApprovalRequested { approval, tool, summary, detail, .. }) = asked.last() else { unreachable!() };
    assert_eq!(tool, "load model");
    assert!(summary.contains("llama3:8b"), "{summary}");
    assert!(detail.contains("GiB of GPU memory") && detail.contains("1.9 GiB is free"), "{detail}");
    c.send(&Request::Approve { approval: *approval, decision: ApprovalDecision::Deny });
    let refused = c.until(|e| matches!(e, Event::Error { .. }));
    assert!(matches!(refused.last(), Some(Event::Error { message, .. }) if message.contains("not loading")), "{refused:#?}");
    assert!(!refused.iter().any(|e| matches!(e, Event::TurnStarted { .. })), "denied: no model call");

    c.send(&Request::Ask { session: None, text: "hi again".into(), workspace: None, images: Vec::new(), options: None });
    let asked = c.until(|e| matches!(e, Event::ApprovalRequested { .. }));
    let Some(Event::ApprovalRequested { approval, .. }) = asked.last() else { unreachable!() };
    c.send(&Request::Approve { approval: *approval, decision: ApprovalDecision::Allow });
    c.until(|e| matches!(e, Event::TurnFinished { .. }));
    c.send(&Request::Ask { session: None, text: "and again".into(), workspace: None, images: Vec::new(), options: None });
    let evs = c.until(|e| matches!(e, Event::TurnFinished { .. }));
    assert!(!evs.iter().any(|e| matches!(e, Event::ApprovalRequested { .. })), "asked once: {evs:#?}");

    c.send(&Request::Shutdown);
    c.until(|e| matches!(e, Event::ShuttingDown));
    join.join().unwrap();
    let _ = std::fs::remove_dir_all(&h);
}
