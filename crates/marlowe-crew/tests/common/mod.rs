//! Shared fixtures for track I's tests: a scripted provider, file tools rooted in each sandbox,
//! approvals that answer at once, temp homes and git repos. No network, no inference.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use marlowe_agent::{ApprovalReq, Consequence, ToolCallReq, ToolCtx, ToolHost, ToolOutcome};
use marlowe_brains::{Brains, Library, MemoryStore, ProviderFactory};
use marlowe_crew::*;
use marlowe_model::{
    fold_events, CancelToken, ChatRequest, Completion, Message, ModelInfo, Provider, ProviderError, ProviderKind, Role,
    StopReason, StreamEvent, ToolSpec, Usage,
};
use marlowe_proto::{ApprovalDecision, SessionId};
use serde_json::{json, Value};

pub type Respond = dyn Fn(&ChatRequest, usize, &CancelToken) -> Result<Vec<StreamEvent>, ProviderError> + Send + Sync;

pub struct Scripted {
    respond: Box<Respond>,
    calls: AtomicUsize,
    requests: Mutex<Vec<ChatRequest>>,
}

impl Scripted {
    pub fn new(
        respond: impl Fn(&ChatRequest, usize, &CancelToken) -> Result<Vec<StreamEvent>, ProviderError> + Send + Sync + 'static,
    ) -> Arc<Scripted> {
        Arc::new(Scripted { respond: Box::new(respond), calls: AtomicUsize::new(0), requests: Mutex::new(Vec::new()) })
    }

    pub fn requests(&self) -> Vec<ChatRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl Provider for Scripted {
    fn kind(&self) -> ProviderKind {
        ProviderKind::OpenRouter
    }

    fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(Vec::new())
    }

    fn stream(&self, req: &ChatRequest, cancel: &CancelToken, on: &mut dyn FnMut(StreamEvent)) -> Result<Completion, ProviderError> {
        self.requests.lock().unwrap().push(req.clone());
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let events = (self.respond)(req, n, cancel)?;
        for ev in &events {
            on(ev.clone());
        }
        Ok(fold_events(&events))
    }
}

/// Ends the whole test process if the test holding it is still running after `secs`: a hung
/// test must never hold the machine-wide build lock. Dropping the guard disarms it.
pub struct Watchdog(#[allow(dead_code)] crossbeam_channel::Sender<()>);

pub fn watchdog(test: &'static str, secs: u64) -> Watchdog {
    let (tx, rx) = crossbeam_channel::bounded::<()>(0);
    std::thread::spawn(move || {
        if rx.recv_timeout(Duration::from_secs(secs)) == Err(crossbeam_channel::RecvTimeoutError::Timeout) {
            eprintln!("watchdog: {test} still running after {secs} s; ending the test process");
            std::process::exit(101);
        }
    });
    Watchdog(tx)
}

pub fn usage() -> StreamEvent {
    StreamEvent::Usage(Usage { input: 100, output: 20, ..Default::default() })
}

pub fn answer(t: &str) -> Vec<StreamEvent> {
    vec![StreamEvent::TextDelta(t.to_string()), usage(), StreamEvent::Stop(StopReason::EndTurn)]
}

/// One message with several tool calls.
pub fn calls(list: &[(&str, Value)]) -> Vec<StreamEvent> {
    let mut v = Vec::new();
    for (i, (name, args)) in (0u32..).zip(list) {
        v.push(StreamEvent::ToolCallStart { index: i, id: String::new(), name: name.to_string() });
        v.push(StreamEvent::ToolArgsDelta { index: i, json: args.to_string() });
        v.push(StreamEvent::ToolCallEnd { index: i });
    }
    v.push(usage());
    v.push(StreamEvent::Stop(StopReason::ToolUse));
    v
}

pub fn write(path: &str, content: &str) -> (&'static str, Value) {
    ("write", json!({"path": path, "content": content}))
}

/// Blocks until the call is cancelled.
pub fn block(cancel: &CancelToken) -> Result<Vec<StreamEvent>, ProviderError> {
    let end = Instant::now() + Duration::from_secs(60);
    while !cancel.is_cancelled() {
        assert!(Instant::now() < end, "never cancelled");
        std::thread::sleep(Duration::from_millis(5));
    }
    Err(ProviderError::cancelled())
}

/// The step's prompt: the first user message.
pub fn prompt_of(req: &ChatRequest) -> String {
    req.messages.iter().find(|m| m.role == Role::User).map(Message::joined_text).unwrap_or_default()
}

pub fn after_tool(req: &ChatRequest) -> bool {
    req.messages.last().is_some_and(|m| m.role == Role::Tool)
}

/// Every tool result the request carries, joined.
pub fn tool_results(req: &ChatRequest) -> String {
    req.messages.iter().flat_map(|m| m.tool_results_iter()).map(|r| r.content.clone()).collect::<Vec<_>>().join("\n")
}

// ---------------------------------------------------------------- tools, approvals

/// `read`, `write` (relative to the sandbox root) and `note`; counts every write. `bash` only
/// says it ran (nothing executes; its consequence is the crew policy's), and `fetch` returns a
/// page and taints the conversation like any untrusted read.
pub struct FileTools {
    pub root: PathBuf,
    pub writes: Arc<AtomicUsize>,
}

impl ToolHost for FileTools {
    fn specs(&self) -> Vec<ToolSpec> {
        ["read", "write", "note", "bash", "fetch"]
            .iter()
            .map(|n| ToolSpec { name: n.to_string(), description: n.to_string(), input_schema: json!({"type": "object"}) })
            .collect()
    }

    fn call(&self, call: &ToolCallReq, _ctx: &ToolCtx) -> ToolOutcome {
        let path = self.root.join(call.args["path"].as_str().unwrap_or("out.txt"));
        match call.name.as_str() {
            "note" => ToolOutcome::ok("noted", "noted"),
            "bash" => ToolOutcome::ok("ran", "ran"),
            "fetch" => ToolOutcome::ok("A page from the web.", "fetched"),
            "read" => match std::fs::read_to_string(&path) {
                Ok(t) => ToolOutcome::ok(t, "read"),
                Err(e) => ToolOutcome::error(e.to_string()),
            },
            "write" => {
                self.writes.fetch_add(1, Ordering::SeqCst);
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir).unwrap();
                }
                std::fs::write(&path, call.args["content"].as_str().unwrap_or("")).unwrap();
                ToolOutcome::ok("wrote", "wrote")
            }
            other => ToolOutcome::error(format!("unknown tool {other}")),
        }
    }

    fn consequence(&self, _call: &ToolCallReq) -> Consequence {
        Consequence::Free
    }

    fn taints(&self, call: &ToolCallReq) -> bool {
        call.name == "fetch"
    }
}

pub fn tool_factory(writes: Arc<AtomicUsize>) -> CrewToolFactory {
    Arc::new(move |sb: &Sandbox, _ws: Option<&Path>| Arc::new(FileTools { root: sb.root.clone(), writes: writes.clone() }) as Arc<dyn ToolHost>)
}

#[derive(Default)]
pub struct Approvals {
    pub allow: AtomicBool,
    pub asked: Mutex<Vec<ApprovalReq>>,
}

impl RunApprovals for Approvals {
    fn ask(&self, _run: &RunId, _session: Option<&SessionId>, req: ApprovalReq) -> ApprovalDecision {
        self.asked.lock().unwrap().push(req);
        if self.allow.load(Ordering::SeqCst) {
            ApprovalDecision::Allow
        } else {
            ApprovalDecision::Deny
        }
    }
}

// ---------------------------------------------------------------- homes, supervisors, repos

pub fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("marlowe-crew-i-{name}-{}", RunId::new_random().short()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A home with a `tester` role (scratch, `note`, `write`, `bash` and `fetch`).
pub fn home(name: &str) -> PathBuf {
    let home = temp_dir(name);
    let roles = home.join("roles");
    std::fs::create_dir_all(&roles).unwrap();
    std::fs::write(
        roles.join("tester.md"),
        "---\nname: tester\ndescription: Test role.\ntools: [note, write, bash, fetch]\nbudget: {tokens: 100000, steps: 20, usd: 1.0}\n\
         sandbox: scratch\n---\nDo the task you are given and answer in a few lines.\n",
    )
    .unwrap();
    home
}

pub fn brains(p: &Arc<Scripted>) -> Arc<Brains> {
    let mut factory = ProviderFactory::new(Arc::new(MemoryStore::new()));
    let lib = Library::open_in_memory().unwrap();
    let p2 = p.clone();
    factory.register(ProviderKind::OpenRouter, move |_| Ok(p2.clone() as Arc<dyn Provider>));
    lib.add(&ModelInfo::bare(ProviderKind::OpenRouter, "m")).unwrap();
    Arc::new(Brains::new(lib, factory))
}

pub struct Fixture {
    pub sup: Supervisor,
    pub events: Receiver<CrewEvent>,
    pub approvals: Arc<Approvals>,
    pub writes: Arc<AtomicUsize>,
}

pub fn start(home: &Path, p: &Arc<Scripted>, cfg: CrewConfig, allow: bool) -> Fixture {
    let approvals = Arc::new(Approvals::default());
    approvals.allow.store(allow, Ordering::SeqCst);
    let writes = Arc::new(AtomicUsize::new(0));
    let (tx, events) = crossbeam_channel::unbounded();
    let deps = CrewDeps {
        brains: brains(p),
        tools: tool_factory(writes.clone()),
        approvals: approvals.clone(),
        mind: None,
        gpu_busy: Arc::new(|| false),
        research: None,
        session_model: None,
    };
    let sup = Supervisor::start(home, deps, cfg, tx).unwrap();
    Fixture { sup, events, approvals, writes }
}

pub fn args(role: &str, goal: &str, playbook: Option<Playbook>, workspace: Option<&Path>) -> DispatchArgs {
    DispatchArgs {
        role: role.into(),
        goal: goal.into(),
        playbook,
        workspace: workspace.map(|w| w.display().to_string()),
        budget: None,
        tainted: false,
    }
}

pub fn wait(what: &str, secs: u64, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(secs);
    while !f() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

pub fn info(sup: &Supervisor, id: &RunId) -> CrewRunInfo {
    sup.get(id).unwrap().unwrap()
}

pub fn wait_status(sup: &Supervisor, id: &RunId, status: CrewRunStatus, secs: u64) -> CrewRunInfo {
    wait(&format!("{status:?}"), secs, || {
        let s = info(sup, id);
        assert!(!(s.status.is_terminal() && s.status != status), "run ended {:?}, note {:?}", s.status, s.note);
        s.status == status
    });
    info(sup, id)
}

pub fn finish(fx: Fixture, dirs: &[&Path]) {
    fx.sup.shutdown();
    drop(fx);
    remove(dirs);
}

pub fn remove(dirs: &[&Path]) {
    for dir in dirs {
        for _ in 0..50 {
            if std::fs::remove_dir_all(dir).is_ok() || !dir.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A repo on `main` with `files` committed.
pub fn repo(base: &Path, files: &[(&str, &str)]) -> PathBuf {
    let work = base.join("work");
    git(base, &["init", "-q", "-b", "main", work.to_str().unwrap()]);
    git(&work, &["config", "user.name", "Test"]);
    git(&work, &["config", "user.email", "test@example.invalid"]);
    git(&work, &["config", "core.autocrlf", "false"]);
    for (path, content) in files {
        let p = work.join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-q", "-m", "first"]);
    work
}
