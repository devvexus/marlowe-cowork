//! Supervisor and sandbox tests: temp homes, temp git repos, a scripted provider. No network, no
//! inference.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use marlowe_agent::{ApprovalReq, Consequence, Exposure, ToolCallReq, ToolCtx, ToolHost, ToolOutcome};
use marlowe_brains::{Brains, Library, MemoryStore, ProviderFactory};
use marlowe_crew::sandbox::{branch_name, no_push_git_env, NO_PUSH_URL};
use marlowe_crew::*;
use marlowe_model::{
    fold_events, CancelToken, ChatRequest, Completion, Effort, ModelInfo, Provider, ProviderError, ProviderKind, Role,
    StopReason, StreamEvent, ToolSpec, Usage,
};
use marlowe_proto::{ApprovalDecision, SessionId, Slot};
use serde_json::{json, Value};

// ---------------------------------------------------------------- scripted provider

type Respond = dyn Fn(&ChatRequest, usize, &CancelToken) -> Result<Vec<StreamEvent>, ProviderError> + Send + Sync;

struct Scripted {
    kind: ProviderKind,
    respond: Box<Respond>,
    calls: AtomicUsize,
    requests: Mutex<Vec<ChatRequest>>,
}

impl Scripted {
    fn new(
        kind: ProviderKind,
        respond: impl Fn(&ChatRequest, usize, &CancelToken) -> Result<Vec<StreamEvent>, ProviderError> + Send + Sync + 'static,
    ) -> Arc<Scripted> {
        Arc::new(Scripted { kind, respond: Box::new(respond), calls: AtomicUsize::new(0), requests: Mutex::new(Vec::new()) })
    }

    fn requests(&self) -> Vec<ChatRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl Provider for Scripted {
    fn kind(&self) -> ProviderKind {
        self.kind
    }

    fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(Vec::new())
    }

    fn stream(
        &self,
        req: &ChatRequest,
        cancel: &CancelToken,
        on: &mut dyn FnMut(StreamEvent),
    ) -> Result<Completion, ProviderError> {
        self.requests.lock().unwrap().push(req.clone());
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let events = (self.respond)(req, n, cancel)?;
        for ev in &events {
            on(ev.clone());
        }
        Ok(fold_events(&events))
    }
}

fn answer(t: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::TextDelta(t.to_string()),
        StreamEvent::Usage(Usage { input: 100, output: 20, ..Default::default() }),
        StreamEvent::Stop(StopReason::EndTurn),
    ]
}

fn call(name: &str, args: Value) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ToolCallStart { index: 0, id: String::new(), name: name.into() },
        StreamEvent::ToolArgsDelta { index: 0, json: args.to_string() },
        StreamEvent::ToolCallEnd { index: 0 },
        StreamEvent::Usage(Usage { input: 100, output: 20, ..Default::default() }),
        StreamEvent::Stop(StopReason::ToolUse),
    ]
}

/// Blocks until the call is cancelled.
fn block(cancel: &CancelToken) -> Result<Vec<StreamEvent>, ProviderError> {
    let end = Instant::now() + Duration::from_secs(30);
    while !cancel.is_cancelled() {
        assert!(Instant::now() < end, "never cancelled");
        std::thread::sleep(Duration::from_millis(5));
    }
    Err(ProviderError::cancelled())
}

fn last_user(req: &ChatRequest) -> String {
    req.messages.iter().rev().find(|m| m.role == Role::User).map(|m| m.joined_text()).unwrap_or_default()
}

fn after_tool(req: &ChatRequest) -> bool {
    req.messages.last().is_some_and(|m| m.role == Role::Tool)
}

/// One tool call, then an answer once the result is in.
fn note_then_answer(text: &'static str) -> impl Fn(&ChatRequest, usize, &CancelToken) -> Result<Vec<StreamEvent>, ProviderError> {
    move |req, _, _| Ok(if after_tool(req) { answer(text) } else { call("note", json!({"text": "looked"})) })
}

// ---------------------------------------------------------------- tools, approvals

/// `note` (free, does nothing), `write` (writes under the sandbox root), `git_diff` (`git diff`
/// in the sandbox root, as a finder would run it) and `read` (a file under the root).
struct TestTools {
    root: PathBuf,
}

impl ToolHost for TestTools {
    fn specs(&self) -> Vec<ToolSpec> {
        ["note", "write", "git_diff", "read"]
            .iter()
            .map(|n| ToolSpec { name: n.to_string(), description: n.to_string(), input_schema: json!({"type": "object"}) })
            .collect()
    }

    fn call(&self, call: &ToolCallReq, _ctx: &ToolCtx) -> ToolOutcome {
        match call.name.as_str() {
            "note" => ToolOutcome::ok("noted", "noted"),
            "write" => {
                let path = self.root.join(call.args["path"].as_str().unwrap_or("out.txt"));
                std::fs::write(&path, call.args["content"].as_str().unwrap_or("")).unwrap();
                ToolOutcome::ok("wrote", "wrote")
            }
            "read" => match std::fs::read_to_string(self.root.join(call.args["path"].as_str().unwrap_or(""))) {
                Ok(text) => ToolOutcome::ok(text, "read"),
                Err(e) => ToolOutcome::error(e.to_string()),
            },
            "git_diff" => {
                let out = Command::new("git").arg("-C").arg(&self.root).arg("diff").output().unwrap();
                ToolOutcome::ok(format!("[diff]{}[/diff]", String::from_utf8_lossy(&out.stdout)), "diffed")
            }
            other => ToolOutcome::error(format!("unknown tool {other}")),
        }
    }

    fn consequence(&self, _call: &ToolCallReq) -> Consequence {
        Consequence::Free
    }
}

fn tool_factory() -> CrewToolFactory {
    // As the daemon's: a `None` sandbox's tools are rooted at the workspace it reads.
    Arc::new(|sb: &Sandbox, ws: Option<&Path>| {
        let root = match sb.kind {
            SandboxKind::None => sb.origin.clone().or_else(|| ws.map(Path::to_path_buf)).unwrap_or_else(|| sb.root.clone()),
            _ => sb.root.clone(),
        };
        Arc::new(TestTools { root }) as Arc<dyn ToolHost>
    })
}

#[derive(Default)]
struct TestApprovals {
    allow: AtomicBool,
    asked: Mutex<Vec<(RunId, ApprovalReq)>>,
    /// When set, each answer is taken from here (the user deciding later).
    hold: Mutex<Option<Receiver<ApprovalDecision>>>,
}

impl RunApprovals for TestApprovals {
    fn ask(&self, run: &RunId, _session: Option<&SessionId>, req: ApprovalReq) -> ApprovalDecision {
        self.asked.lock().unwrap().push((run.clone(), req));
        let held = self.hold.lock().unwrap().clone();
        if let Some(rx) = held {
            return rx.recv_timeout(Duration::from_secs(30)).unwrap_or(ApprovalDecision::Deny);
        }
        if self.allow.load(Ordering::SeqCst) {
            ApprovalDecision::Allow
        } else {
            ApprovalDecision::Deny
        }
    }
}

// ---------------------------------------------------------------- fixture

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("marlowe-crew-it-{name}-{}", RunId::new_random().short()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

const CARD_BODY: &str = "Do the task you are given and answer in a few lines.\n";

/// A home with two test roles: `tester` (scratch) and `builder` (worktree).
fn home(name: &str) -> PathBuf {
    let home = temp_dir(name);
    let roles = home.join("roles");
    std::fs::create_dir_all(&roles).unwrap();
    for (role, sandbox) in [("tester", "scratch"), ("builder", "worktree")] {
        std::fs::write(
            roles.join(format!("{role}.md")),
            format!(
                "---\nname: {role}\ndescription: Test role.\ntools: [note, write]\n\
                 budget: {{tokens: 100000, steps: 20, usd: 1.0}}\nsandbox: {sandbox}\n---\n{CARD_BODY}"
            ),
        )
        .unwrap();
    }
    home
}

/// Brains whose first model (the default) is `models[0]`.
fn brains(models: &[(&str, Arc<Scripted>)]) -> Arc<Brains> {
    let mut factory = ProviderFactory::new(Arc::new(MemoryStore::new()));
    let lib = Library::open_in_memory().unwrap();
    for (id, p) in models {
        let p = p.clone();
        let kind = p.kind;
        factory.register(kind, move |_| Ok(p.clone() as Arc<dyn Provider>));
        lib.add(&ModelInfo::bare(kind, *id)).unwrap();
    }
    Arc::new(Brains::new(lib, factory))
}

struct Fixture {
    sup: Supervisor,
    events: Receiver<CrewEvent>,
    approvals: Arc<TestApprovals>,
}

fn start_with(home: &Path, brains: Arc<Brains>, cfg: CrewConfig, gpu_busy: bool) -> Fixture {
    start_probe(home, brains, cfg, Arc::new(move || gpu_busy))
}

fn start_probe(home: &Path, brains: Arc<Brains>, cfg: CrewConfig, gpu_busy: GpuBusy) -> Fixture {
    let approvals = Arc::new(TestApprovals::default());
    let (tx, events) = crossbeam_channel::unbounded();
    let deps = CrewDeps { brains, tools: tool_factory(), approvals: approvals.clone(), mind: None, gpu_busy, research: None, session_model: None };
    let sup = Supervisor::start(home, deps, cfg, tx).unwrap();
    Fixture { sup, events, approvals }
}

fn start(home: &Path, provider: &Arc<Scripted>) -> Fixture {
    start_with(home, brains(&[("m", provider.clone())]), CrewConfig::default(), false)
}

fn cloud(respond: impl Fn(&ChatRequest, usize, &CancelToken) -> Result<Vec<StreamEvent>, ProviderError> + Send + Sync + 'static) -> Arc<Scripted> {
    Scripted::new(ProviderKind::OpenRouter, respond)
}

fn args(role: &str, goal: &str, playbook: Option<Playbook>, workspace: Option<&Path>) -> DispatchArgs {
    DispatchArgs {
        role: role.into(),
        goal: goal.into(),
        playbook,
        workspace: workspace.map(|w| w.display().to_string()),
        budget: None,
        tainted: false,
    }
}

fn wait(what: &str, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(30);
    while !f() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn info(sup: &Supervisor, id: &RunId) -> CrewRunInfo {
    sup.get(id).unwrap().unwrap()
}

fn wait_status(sup: &Supervisor, id: &RunId, status: CrewRunStatus) -> CrewRunInfo {
    wait(&format!("{status:?}"), || {
        let s = info(sup, id).status;
        assert!(!(s.is_terminal() && s != status), "run ended {s:?}, note {:?}", info(sup, id).note);
        s == status
    });
    info(sup, id)
}

fn drain(rx: &Receiver<CrewEvent>) -> Vec<CrewEvent> {
    rx.try_iter().collect()
}

/// Shuts the supervisor down, closes `cowork.db`, and removes `dirs` (retrying while Windows
/// still holds a handle).
fn finish(fx: Fixture, dirs: &[&Path]) {
    fx.sup.shutdown();
    drop(fx);
    remove(dirs);
}

fn remove(dirs: &[&Path]) {
    for dir in dirs {
        for _ in 0..50 {
            if std::fs::remove_dir_all(dir).is_ok() || !dir.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A repo with one commit on `main` and an `origin` remote pointing at a bare repo.
fn repo(name: &str) -> (PathBuf, PathBuf) {
    let base = temp_dir(name);
    let (work, bare) = (base.join("work"), base.join("origin.git"));
    git(&base, &["init", "-q", "--bare", bare.to_str().unwrap()]);
    git(&base, &["init", "-q", "-b", "main", work.to_str().unwrap()]);
    git(&work, &["config", "user.name", "Test"]);
    git(&work, &["config", "user.email", "test@example.invalid"]);
    std::fs::write(work.join("README.md"), "hello\n").unwrap();
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-q", "-m", "first"]);
    git(&work, &["remote", "add", "origin", bare.to_str().unwrap()]);
    (work, bare)
}

// ---------------------------------------------------------------- tests

#[test]
fn a_run_completes_and_its_steps_are_stored() {
    let home = home("complete");
    let provider = cloud(note_then_answer("All three configs agree.\nConfidence: high"));
    let fx = start(&home, &provider);
    let receipt = fx.sup.dispatch(Some(SessionId("s1".into())), args("tester", "Compare the configs", None, None)).unwrap();
    assert_eq!(receipt.role, "tester");

    let run = wait_status(&fx.sup, &receipt.run_id, CrewRunStatus::Done);
    let result = run.result.expect("a result");
    assert_eq!(result.summary, "All three configs agree.\nConfidence: high");
    assert_eq!(result.confidence, Confidence::High);
    assert_eq!((run.spent.steps, run.spent.tokens, run.step), (2, 240, 2));
    assert_eq!(run.sandbox.as_ref().map(|s| s.kind), Some(SandboxKind::Scratch));

    let (_, steps) = fx.sup.transcript(&receipt.run_id).unwrap();
    assert_eq!(steps.len(), 1);
    assert!(steps[0].action.contains("note"), "{}", steps[0].action);
    assert!(steps[0].text.contains("› Goal: Compare the configs") && steps[0].text.contains("All three configs agree"));

    let events = drain(&fx.events);
    assert!(events.iter().any(|e| matches!(e, CrewEvent::Finished { session: Some(s), .. } if s.0 == "s1")));
    assert!(events.iter().any(|e| matches!(e, CrewEvent::Step { text_delta: Some(_), .. })));
    assert!(events.iter().any(|e| matches!(e, CrewEvent::Updated(i) if i.status == CrewRunStatus::Running)));
    assert_eq!(fx.sup.list(10).unwrap()[0].id, receipt.run_id);
    assert!(matches!(
        fx.sup.dispatch(None, args("nobody", "x", None, None)),
        Err(CrewError::UnknownRole(r)) if r == "nobody"
    ));
    finish(fx, &[&home]);
}

#[test]
fn a_run_reports_its_spin_up_as_each_step_really_ends() {
    use marlowe_proto::{PhaseKind, PhaseState};
    let home = home("spinup");
    let provider = cloud(note_then_answer("done.
Confidence: high"));
    let fx = start(&home, &provider);
    let receipt = fx.sup.dispatch(None, args("tester", "Compare the configs", None, None)).unwrap();
    wait_status(&fx.sup, &receipt.run_id, CrewRunStatus::Done);
    let phases: Vec<_> = drain(&fx.events)
        .into_iter()
        .filter_map(|e| match e {
            CrewEvent::Phase(p) if p.kind == PhaseKind::Spinup => Some(p),
            _ => None,
        })
        .collect();
    assert!(phases.iter().all(|p| p.run.as_ref() == Some(&receipt.run_id)));
    let names: Vec<(PhaseState, Option<&str>)> = phases.iter().map(|p| (p.state, p.stage.as_deref())).collect();
    assert_eq!(
        names,
        [
            (PhaseState::Start, None),
            (PhaseState::End, Some("sandbox")),
            (PhaseState::End, Some("tools")),
            (PhaseState::End, Some("budget")),
            (PhaseState::End, Some("model")),
        ]
    );
    assert_eq!(phases[0].detail.as_deref(), Some("tester"), "the opening names the role");
    assert!(phases[2].done.is_some_and(|n| n > 0), "the tool count is the role's, measured");
    assert!(phases[3].detail.as_deref().is_some_and(|d| !d.is_empty()), "the budget echo");
    assert!(phases[4].detail.is_some(), "the model that was resolved");
    finish(fx, &[&home]);
}

#[test]
fn a_run_killed_mid_way_resumes_at_its_next_step_after_a_restart() {
    let home = home("resume");
    let first = cloud(|req, _, cancel| {
        let u = last_user(req);
        if u.contains("Plan the investigation") {
            Ok(answer("1. Which ranks were tested?\n2. Which tasks?"))
        } else {
            block(cancel) // the scouts never come back before the "crash"
        }
    });
    let fx = start(&home, &first);
    let id = fx.sup.dispatch(None, args("researcher", "LoRA rank sensitivity", Some(Playbook::DeepResearch), None)).unwrap().run_id;
    wait("both scouts in flight", || {
        first.requests().iter().filter(|r| last_user(r).contains("Your sub-question")).count() == 2
    });
    fx.sup.shutdown();
    let stored = info(&fx.sup, &id);
    assert_eq!(stored.status, CrewRunStatus::Running, "a stopped run stays running in the store");
    drop(fx);

    let second = cloud(|req, _, _| {
        let u = last_user(req);
        assert!(!u.contains("Plan the investigation"), "the completed plan step ran again");
        Ok(if u.contains("Your sub-question") {
            answer("1. Rank 8 matches full fine-tuning on GLUE [Hu 2021](https://arxiv.org/abs/2106.09685) (load-bearing)")
        } else if u.contains("Try to refute") {
            answer("1. supported\n2. supported")
        } else {
            answer("Rank barely matters above 8 [Hu 2021](https://arxiv.org/abs/2106.09685).\n\nConfidence: medium")
        })
    });
    let fx = start(&home, &second);
    let run = wait_status(&fx.sup, &id, CrewRunStatus::Done);
    let keys: Vec<String> = fx.sup.transcript(&id).unwrap().1.iter().map(|s| s.action.clone()).collect();
    assert_eq!(keys.len(), 5, "{keys:?}");
    assert!(keys[0].starts_with("plan"), "{keys:?}");
    assert_eq!(second.requests().len(), 4, "two scouts, verify, synthesize");
    let sandbox = PathBuf::from(&run.sandbox.unwrap().root);
    assert!(std::fs::read_to_string(sandbox.join(playbook::REPORT_FILE)).unwrap().contains("Rank barely matters"));
    assert_eq!(run.result.unwrap().citations.len(), 1);
    finish(fx, &[&home]);
}

#[test]
fn a_run_the_daemon_died_under_fails_with_the_reason_instead_of_staying_running() {
    // 2026-10-21: the daemon aborted with "memory allocation of 5505024 bytes failed" when
    // llama-server took the last of the commit limit, and its workflow run stayed `running`.
    let home = home("crashed");
    let first = cloud(|req, _, cancel| {
        if last_user(req).contains("Plan the investigation") {
            Ok(answer("1. Which ranks were tested?\n2. Which tasks?"))
        } else {
            block(cancel)
        }
    });
    let fx = start(&home, &first);
    assert!(home.join(ALIVE_FILE).exists(), "a running supervisor leaves its marker");
    let id = fx.sup.dispatch(None, args("researcher", "LoRA rank sensitivity", Some(Playbook::DeepResearch), None)).unwrap().run_id;
    wait("both scouts in flight", || {
        first.requests().iter().filter(|r| last_user(r).contains("Your sub-question")).count() == 2
    });
    fx.sup.shutdown();
    drop(fx);
    assert!(!home.join(ALIVE_FILE).exists(), "a clean stop removes it");

    // What the dead daemon left: its marker, and its last words after the offset it recorded.
    std::fs::write(home.join("daemon.log"), "an older daemon: memory allocation of 1 bytes failed\nmarlowe: loading the conductor\nmemory allocation of 5505024 bytes failed\n").unwrap();
    std::fs::write(home.join(ALIVE_FILE), "pid=1\nlog=53\n").unwrap();
    let second = cloud(|_, _, _| Ok(answer("resumed")));
    let fx = start(&home, &second);
    let run = info(&fx.sup, &id);
    assert_eq!(run.status, CrewRunStatus::Failed, "{:?}", run.note);
    let note = run.note.unwrap_or_default();
    assert!(note.contains("commit memory") && note.contains("5505024") && !note.contains("of 1 bytes"), "{note}");
    std::thread::sleep(Duration::from_millis(200));
    assert!(second.requests().is_empty(), "it does not start again by itself");
    finish(fx, &[&home]);
}

#[test]
fn a_local_call_waits_for_commit_memory_and_fails_with_the_reason_when_it_stays_short() {
    let home = home("commit-short");
    let local = Scripted::new(ProviderKind::Ollama, |_, _, _| Ok(answer("never")));
    let cfg = CrewConfig {
        commit: || Some(marlowe_os::memory::Commit { available: 1 << 30, limit: 127 << 30 }),
        memory_wait: Duration::from_millis(300),
        ..CrewConfig::default()
    };
    let fx = start_with(&home, brains(&[("qwen", local.clone())]), cfg, false);
    let id = fx.sup.dispatch(None, args("tester", "Anything", None, None)).unwrap().run_id;
    let run = wait_status(&fx.sup, &id, CrewRunStatus::Failed);
    let note = run.note.unwrap_or_default();
    assert!(note.contains("Windows is out of memory: 1.0 GB of its 127 GB commit limit is left"), "{note}");
    assert_eq!(local.calls.load(Ordering::SeqCst), 0, "the runner never got the prompt");
    finish(fx, &[&home]);
}

#[test]
fn live_step_numbers_match_the_transcript_rows_with_parallel_scouts() {
    let home = home("rows");
    // A scout that may call `note`, so one scout makes two model calls while the other makes one.
    std::fs::write(
        home.join("roles").join("scout.md"),
        format!("---\nname: scout\ndescription: Test scout.\ntools: [note]\nbudget: {{tokens: 100000, steps: 20, usd: 1.0}}\nsandbox: scratch\n---\n{CARD_BODY}"),
    )
    .unwrap();
    let provider = cloud(|req, _, _| {
        let u = last_user(req);
        Ok(if u.contains("Plan the investigation") {
            answer("1. First question?\n2. Second question?")
        } else if u.contains("Your sub-question: First question?") && !after_tool(req) {
            call("note", json!({"text": "looked"}))
        } else if u.contains("Your sub-question") {
            answer(&format!("1. claim for {} [src](https://example.com/{})", &u[u.len() - 16..], u.len()))
        } else if u.contains("Try to refute") {
            answer("1. supported\n2. supported")
        } else {
            answer("The report [src](https://example.com/a).\n\nConfidence: medium")
        })
    });
    let fx = start(&home, &provider);
    let id = fx.sup.dispatch(None, args("researcher", "Row numbers", Some(Playbook::DeepResearch), None)).unwrap().run_id;
    wait_status(&fx.sup, &id, CrewRunStatus::Done);
    let (_, rows) = fx.sup.transcript(&id).unwrap();
    let mut numbers: Vec<u32> = rows.iter().map(|s| s.step).collect();
    numbers.sort_unstable();
    numbers.dedup();
    assert_eq!(numbers.len(), rows.len(), "row numbers are unique: {rows:?}");

    let mut live = 0;
    for e in drain(&fx.events) {
        let CrewEvent::Step { step, action, text_delta, .. } = e else { continue };
        let row = rows.iter().find(|r| r.step == step).unwrap_or_else(|| panic!("no row {step} for {action:?}"));
        assert!(row.action.starts_with(&action), "event {step} {action:?} vs row {:?}", row.action);
        // Tool lines are the live form of the row's `▸`/`↳` lines, worded by the tool's summary.
        if let Some(t) = text_delta.filter(|t| !t.starts_with(['▸', ' '])) {
            assert!(row.text.contains(t.trim()), "delta of {action:?} is in row {step}");
            live += 1;
        }
    }
    assert_eq!(live, 5, "plan, two scouts, verify, synthesize");
    finish(fx, &[&home]);
}

#[test]
fn an_open_step_shows_its_checkpointed_calls_and_streams_its_tool_lines() {
    let home = home("open-step");
    // One tool call, then a model call held until the pause is in: the "main" step stays open.
    let release = Arc::new(AtomicBool::new(false));
    let gate = release.clone();
    let provider = cloud(move |_req, n, cancel| {
        if n == 1 {
            wait("the pause", || gate.load(Ordering::SeqCst) || cancel.is_cancelled());
        }
        Ok(call("note", json!({"text": "looked"})))
    });
    let fx = start(&home, &provider);
    let id = fx.sup.dispatch(None, args("tester", "Look around", None, None)).unwrap().run_id;
    wait("the second model call", || provider.calls.load(Ordering::SeqCst) == 2);

    let (_, rows) = fx.sup.transcript(&id).unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!((rows[0].step, rows[0].role.as_str()), (1, "tester"));
    assert!(rows[0].text.contains("› Goal: Look around"), "{}", rows[0].text);
    assert!(rows[0].text.contains("▸ note") && rows[0].text.contains("↳ noted"), "{}", rows[0].text);

    // Live: the tool's start and finish stream as lines of their own, on the same row.
    let deltas: Vec<(u32, String)> = drain(&fx.events)
        .into_iter()
        .filter_map(|e| match e {
            CrewEvent::Step { step, text_delta: Some(t), .. } => Some((step, t)),
            _ => None,
        })
        .collect();
    assert!(deltas.iter().any(|(n, t)| *n == 1 && t.starts_with("▸ note") && t.ends_with('\n')), "{deltas:?}");
    assert!(deltas.iter().any(|(n, t)| *n == 1 && t.starts_with("  ↳ noted")), "{deltas:?}");

    // Paused mid-step: the row stays, and says where it stopped.
    fx.sup.pause(&id).unwrap();
    release.store(true, Ordering::SeqCst);
    let run = wait_status(&fx.sup, &id, CrewRunStatus::Paused);
    let (_, rows) = fx.sup.transcript(&id).unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].step, 1);
    assert!(rows[0].action.starts_with("stopped here"), "{}", rows[0].action);
    assert!(rows[0].text.contains("▸ note"), "{}", rows[0].text);
    assert_eq!(run.step, 2);
    finish(fx, &[&home]);
}

#[test]
fn cancel_stops_a_running_run_and_discard_removes_its_scratch() {
    let home = home("cancel");
    let provider = cloud(|_, _, cancel| block(cancel));
    let fx = start(&home, &provider);
    let id = fx.sup.dispatch(None, args("tester", "Wait forever", None, None)).unwrap().run_id;
    wait("the model call", || provider.calls.load(Ordering::SeqCst) == 1);
    fx.sup.cancel(&id).unwrap();
    let run = wait_status(&fx.sup, &id, CrewRunStatus::Cancelled);
    let scratch = PathBuf::from(run.sandbox.unwrap().root);
    assert!(scratch.is_dir());
    assert!(matches!(fx.sup.cancel(&id), Err(CrewError::BadState { .. })));
    assert!(matches!(fx.sup.steer(&id, "too late"), Err(CrewError::BadState { .. })));
    fx.sup.discard(&id).unwrap();
    assert!(!scratch.exists());
    assert_eq!(info(&fx.sup, &id).note.as_deref(), Some("discarded"));
    finish(fx, &[&home]);
}

#[test]
fn steer_text_reaches_the_next_step() {
    let home = home("steer");
    let (release, gate) = crossbeam_channel::bounded::<()>(1);
    let provider = cloud(move |_, n, cancel| {
        if n == 0 {
            while gate.recv_timeout(Duration::from_millis(10)).is_err() {
                if cancel.is_cancelled() {
                    return Err(ProviderError::cancelled());
                }
            }
            Ok(call("note", json!({"text": "first look"})))
        } else {
            Ok(answer("Used the 2019 split."))
        }
    });
    let fx = start(&home, &provider);
    let id = fx.sup.dispatch(None, args("tester", "Evaluate the model", None, None)).unwrap().run_id;
    wait("the first model call", || provider.calls.load(Ordering::SeqCst) == 1);
    fx.sup.steer(&id, "use the 2019 split, not 2020").unwrap();
    assert!(matches!(fx.sup.steer(&id, "   "), Err(CrewError::BadState { .. })));
    release.send(()).unwrap();
    wait_status(&fx.sup, &id, CrewRunStatus::Done);

    let reqs = provider.requests();
    assert_eq!(reqs.len(), 2);
    assert!(!reqs[0].messages.iter().any(|m| m.joined_text().contains("2019")));
    assert_eq!(last_user(&reqs[1]), "use the 2019 split, not 2020");
    assert!(drain(&fx.events).iter().any(|e| matches!(e, CrewEvent::Step { action, .. } if action.starts_with("steer:"))));
    finish(fx, &[&home]);
}

#[test]
fn a_budget_overrun_needs_you_and_an_approved_extension_continues() {
    let home = home("budget");
    let provider = cloud(note_then_answer("Finished on the extension."));
    let fx = start(&home, &provider);
    let mut a = args("tester", "Count the rows", None, None);
    a.budget = Some(CrewBudget { steps: Some(1), ..Default::default() });

    // Refused: the run pauses at its budget.
    let id = fx.sup.dispatch(None, a).unwrap().run_id;
    let paused = wait_status(&fx.sup, &id, CrewRunStatus::Paused);
    assert!(paused.note.unwrap().contains("steps budget"));
    let asked = fx.approvals.asked.lock().unwrap().clone();
    assert_eq!(asked.len(), 1);
    assert_eq!((asked[0].0.clone(), asked[0].1.tool.as_str()), (id.clone(), "budget"));
    assert!(asked[0].1.summary.contains("steps budget (1 of 1)"), "{}", asked[0].1.summary);
    assert!(drain(&fx.events)
        .iter()
        .any(|e| matches!(e, CrewEvent::Updated(i) if i.id == id && i.status == CrewRunStatus::NeedsYou)));

    // Approved on resume: the budget grows and the run finishes.
    fx.approvals.allow.store(true, Ordering::SeqCst);
    fx.sup.resume(&id).unwrap();
    let done = wait_status(&fx.sup, &id, CrewRunStatus::Done);
    assert_eq!(done.result.unwrap().summary, "Finished on the extension.");
    assert!(done.budget.steps.unwrap() >= 2, "{:?}", done.budget);
    assert!(fx.approvals.asked.lock().unwrap().len() >= 2);
    finish(fx, &[&home]);
}

#[test]
fn pause_holds_a_queued_run_and_resume_requeues_it() {
    let home = home("pause");
    let provider = cloud(|_, _, cancel| block(cancel));
    let cfg = CrewConfig { per_provider: 1, ..CrewConfig::default() };
    let fx = start_with(&home, brains(&[("m", provider.clone())]), cfg, false);
    let a = fx.sup.dispatch(None, args("tester", "first", None, None)).unwrap().run_id;
    let b = fx.sup.dispatch(None, args("tester", "second", None, None)).unwrap().run_id;
    wait_status(&fx.sup, &a, CrewRunStatus::Running);
    assert_eq!(info(&fx.sup, &b).status, CrewRunStatus::Queued, "one run per provider here");
    fx.sup.pause(&b).unwrap();
    assert_eq!(info(&fx.sup, &b).status, CrewRunStatus::Paused);
    fx.sup.cancel(&a).unwrap();
    wait_status(&fx.sup, &a, CrewRunStatus::Cancelled);
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(info(&fx.sup, &b).status, CrewRunStatus::Paused, "a paused run is not scheduled");
    fx.sup.resume(&b).unwrap();
    wait_status(&fx.sup, &b, CrewRunStatus::Running);
    fx.sup.pause(&b).unwrap(); // a running run pauses at its next boundary; cancel the call to get there
    fx.sup.cancel(&b).unwrap();
    wait_status(&fx.sup, &b, CrewRunStatus::Cancelled);
    assert!(matches!(fx.sup.resume(&b), Err(CrewError::BadState { .. })), "the user's cancel outranks the pause");
    finish(fx, &[&home]);
}

#[test]
fn cancel_during_a_budget_approval_ends_cancelled_not_paused() {
    let home = home("budget-cancel");
    let provider = cloud(note_then_answer("never reached"));
    let fx = start(&home, &provider);
    let (decide, decisions) = crossbeam_channel::bounded(1);
    *fx.approvals.hold.lock().unwrap() = Some(decisions);
    let mut a = args("tester", "Count the rows", None, None);
    a.budget = Some(CrewBudget { steps: Some(1), ..Default::default() });
    let id = fx.sup.dispatch(None, a).unwrap().run_id;
    wait_status(&fx.sup, &id, CrewRunStatus::NeedsYou);
    // What the daemon does on CancelRun: cancel, then deny what the run waits on.
    fx.sup.cancel(&id).unwrap();
    decide.send(ApprovalDecision::Deny).unwrap();
    let run = wait_status(&fx.sup, &id, CrewRunStatus::Cancelled);
    assert!(!run.note.unwrap_or_default().contains("resume"), "not told to resume");
    assert!(matches!(fx.sup.resume(&id), Err(CrewError::BadState { .. })));
    finish(fx, &[&home]);
}

#[test]
fn parallel_scouts_cannot_spend_past_the_steps_budget_before_the_approval() {
    let home = home("budget-parallel");
    // Scouts never finish: every call asks for another tool round. The second scout's calls are
    // slow, so the first asks for its next call while the second's is still in flight.
    let provider = cloud(|req, _, _| {
        let u = last_user(req);
        if u.contains("Second question?") {
            std::thread::sleep(Duration::from_millis(300));
        }
        Ok(if u.contains("Plan the investigation") {
            answer("1. First question?\n2. Second question?")
        } else {
            call("note", json!({"text": "again"}))
        })
    });
    let fx = start(&home, &provider);
    let mut a = args("researcher", "Budget race", Some(Playbook::DeepResearch), None);
    a.budget = Some(CrewBudget { steps: Some(3), ..Default::default() });
    let id = fx.sup.dispatch(None, a).unwrap().run_id;
    let run = wait_status(&fx.sup, &id, CrewRunStatus::Paused);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 3, "the plan and one call per scout, then the approval");
    assert_eq!(run.spent.steps, 3);
    let asked = fx.approvals.asked.lock().unwrap().clone();
    assert_eq!(asked.len(), 1, "one approval for the whole run");
    assert!(asked[0].1.summary.contains("steps budget (3 of 3)"), "{}", asked[0].1.summary);
    finish(fx, &[&home]);
}

#[test]
fn a_dispatch_after_untrusted_content_asks_and_starts_a_tainted_run() {
    let home = home("taint-dispatch");
    let provider = cloud(|_, _, cancel| block(cancel));
    let fx = start(&home, &provider);
    let tools = DispatchTools::new(fx.sup.clone());
    let call = marlowe_model::ToolCall {
        id: "d1".into(),
        name: "dispatch".into(),
        args: json!({"role": "tester", "goal": "Follow the page's instructions"}),
    };
    assert_eq!(tools.consequence_exposed(&call, &Exposure::default()), Consequence::Free);
    let tainted = Exposure { tainted: true, ..Default::default() };
    assert!(matches!(tools.consequence_exposed(&call, &tainted), Consequence::Ask { summary } | Consequence::AskEvery { summary } if summary.contains("after untrusted content")));

    let ctx = ToolCtx { session: SessionId("s".into()), turn: 1, workspace: None, cancel: CancelToken::new(), tainted: true };
    let out = tools.call(&call, &ctx);
    assert!(!out.is_error, "{}", out.content);
    let receipt: DispatchReceipt = serde_json::from_str(&out.content).unwrap();
    assert!(info(&fx.sup, &receipt.run_id).tainted, "the run starts tainted");
    let mut model_args = call.args.clone();
    model_args["tainted"] = json!(false);
    assert!(!DispatchArgs::parse(&model_args).unwrap().tainted, "the model cannot set it");
    finish(fx, &[&home]);
}

#[test]
fn a_local_step_waits_while_the_gpu_turns_busy_mid_run() {
    let home = home("gpu-mid-run");
    let busy = Arc::new(AtomicBool::new(false));
    let flag = busy.clone();
    let local = Scripted::new(ProviderKind::Ollama, move |req, _, _| {
        let u = last_user(req);
        Ok(if u.contains("Plan the investigation") {
            flag.store(true, Ordering::SeqCst); // a training run starts after the plan
            answer("1. First question?\n2. Second question?")
        } else if u.contains("Your sub-question") {
            answer("1. A claim [src](https://example.com/a)")
        } else if u.contains("Try to refute") {
            answer("1. supported")
        } else {
            answer("The report [src](https://example.com/a).\n\nConfidence: medium")
        })
    });
    let probe = busy.clone();
    let fx = start_probe(&home, brains(&[("qwen", local.clone())]), CrewConfig::default(), Arc::new(move || probe.load(Ordering::SeqCst)));
    let id = fx.sup.dispatch(None, args("researcher", "GPU hold", Some(Playbook::DeepResearch), None)).unwrap().run_id;
    wait("the GPU hold", || info(&fx.sup, &id).current_action.as_deref() == Some("waiting for the GPU"));
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(local.calls.load(Ordering::SeqCst), 1, "only the plan ran on the local model");
    assert_eq!(info(&fx.sup, &id).status, CrewRunStatus::Running);
    busy.store(false, Ordering::SeqCst);
    wait_status(&fx.sup, &id, CrewRunStatus::Done);
    finish(fx, &[&home]);
}

#[test]
fn local_streams_are_capped_across_runs() {
    let home = home("local-cap");
    let (now, peak) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let (n, p) = (now.clone(), peak.clone());
    let local = Scripted::new(ProviderKind::Ollama, move |req, _, _| {
        let k = n.fetch_add(1, Ordering::SeqCst) + 1;
        p.fetch_max(k, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(60));
        n.fetch_sub(1, Ordering::SeqCst);
        Ok(if last_user(req).contains("Try to refute") { answer("1. supported\n2. supported") } else { answer("1. A claim [src](https://example.com/a)") })
    });
    let remote = cloud(|req, _, _| {
        Ok(if last_user(req).contains("Plan the investigation") {
            answer("1. First question?\n2. Second question?")
        } else {
            answer("The report [src](https://example.com/a).\n\nConfidence: medium")
        })
    });
    // The researcher runs on a cloud slot; scouts and the verifier fall back to the local default.
    let b = brains(&[("qwen", local.clone()), ("gpt", remote.clone())]);
    let gpt = b.with_library(|l| l.models().unwrap()).into_iter().find(|m| m.info.id == "gpt").unwrap().model;
    b.set_slot(&Slot::Crew("researcher".into()), Some(&gpt), Effort::Low).unwrap();
    let fx = start_with(&home, b, CrewConfig::default(), false);
    let ids: Vec<RunId> = (0..2)
        .map(|i| fx.sup.dispatch(None, args("researcher", &format!("Cap {i}"), Some(Playbook::DeepResearch), None)).unwrap().run_id)
        .collect();
    for id in &ids {
        wait_status(&fx.sup, id, CrewRunStatus::Done);
    }
    assert!(local.calls.load(Ordering::SeqCst) >= 4, "the scouts ran locally");
    assert_eq!(peak.load(Ordering::SeqCst), 1, "one local stream at a time across both runs");
    finish(fx, &[&home]);
}

// ---------------------------------------------------------------- crew capacity

/// A local provider whose calls hold until `release`, counting the streams in flight.
fn holder(release: &Arc<AtomicBool>) -> (Arc<Scripted>, Arc<AtomicUsize>) {
    let (now, peak) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let (n, p, r) = (now.clone(), peak.clone(), release.clone());
    let provider = Scripted::new(ProviderKind::Ollama, move |_, _, cancel| {
        p.fetch_max(n.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
        let end = Instant::now() + Duration::from_secs(30);
        while !r.load(Ordering::SeqCst) && !cancel.is_cancelled() {
            assert!(Instant::now() < end, "never released");
            std::thread::sleep(Duration::from_millis(5));
        }
        n.fetch_sub(1, Ordering::SeqCst);
        Ok(answer("held, then done"))
    });
    (provider, peak)
}

fn with_slots(b: &Brains, model: &str, slots: u16) -> marlowe_model::ModelRef {
    let m = b.with_library(|l| l.models().unwrap()).into_iter().find(|m| m.info.id == model).unwrap().model;
    let policy = marlowe_proto::ContextPolicy {
        max_ctx: 16_384,
        kv_type: marlowe_proto::KvType::Q8_0,
        parallel: slots,
        source: marlowe_proto::PolicySource::User,
        updated_ms: 1,
    };
    b.with_library(|l| l.set_policy(&m, &policy)).unwrap();
    m
}

fn count(sup: &Supervisor, ids: &[RunId], status: CrewRunStatus) -> usize {
    ids.iter().filter(|id| info(sup, id).status == status).count()
}

fn dispatch_n(sup: &Supervisor, role: &str, n: usize) -> Vec<RunId> {
    (0..n).map(|i| sup.dispatch(None, args(role, &format!("Hold {i}"), None, None)).unwrap().run_id).collect()
}

#[test]
fn three_local_dispatches_on_six_slots_all_run() {
    let home = home("slots-three");
    let release = Arc::new(AtomicBool::new(false));
    let (local, peak) = holder(&release);
    let b = brains(&[("qwen", local)]);
    with_slots(&b, "qwen", 6);
    let fx = start_with(&home, b, CrewConfig::default(), false);
    let ids = dispatch_n(&fx.sup, "tester", 3);
    wait("three running", || count(&fx.sup, &ids, CrewRunStatus::Running) == 3);
    wait("three streams", || peak.load(Ordering::SeqCst) == 3);
    assert_eq!(count(&fx.sup, &ids, CrewRunStatus::Queued), 0, "none queued");
    release.store(true, Ordering::SeqCst);
    for id in &ids {
        wait_status(&fx.sup, id, CrewRunStatus::Done);
    }
    finish(fx, &[&home]);
}

#[test]
fn seven_local_dispatches_run_five_and_queue_two() {
    let home = home("slots-seven");
    let release = Arc::new(AtomicBool::new(false));
    let (local, peak) = holder(&release);
    let b = brains(&[("qwen", local)]);
    with_slots(&b, "qwen", 6);
    let fx = start_with(&home, b, CrewConfig::default(), false);
    let ids = dispatch_n(&fx.sup, "tester", 7);
    wait("five running", || count(&fx.sup, &ids, CrewRunStatus::Running) == 5);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!((count(&fx.sup, &ids, CrewRunStatus::Running), count(&fx.sup, &ids, CrewRunStatus::Queued)), (5, 2));
    assert_eq!(peak.load(Ordering::SeqCst), 5, "the conductor keeps the sixth slot: streams never pass the crew's five");
    release.store(true, Ordering::SeqCst);
    for id in &ids {
        wait_status(&fx.sup, id, CrewRunStatus::Done);
    }
    assert_eq!(peak.load(Ordering::SeqCst), 5, "the queued two ran after, not beside");
    finish(fx, &[&home]);
}

#[test]
fn local_streams_are_capped_per_model_not_across_models() {
    let home = home("slots-per-model");
    std::fs::write(
        home.join("roles").join("tester2.md"),
        format!("---
name: tester2
description: Test role.
tools: [note]
budget: {{tokens: 100000, steps: 20, usd: 1.0}}
sandbox: scratch
---
{CARD_BODY}"),
    )
    .unwrap();
    // One Ollama provider serves every model (the factory is by kind); it counts streams by model.
    let release = Arc::new(AtomicBool::new(false));
    let counts: Arc<Mutex<std::collections::HashMap<String, (usize, usize)>>> = Arc::default();
    let (c, r) = (counts.clone(), release.clone());
    let provider = Scripted::new(ProviderKind::Ollama, move |req, _, cancel| {
        {
            let mut m = c.lock().unwrap();
            let e = m.entry(req.model.clone()).or_default();
            e.0 += 1;
            e.1 = e.1.max(e.0);
        }
        let end = Instant::now() + Duration::from_secs(30);
        while !r.load(Ordering::SeqCst) && !cancel.is_cancelled() {
            assert!(Instant::now() < end, "never released");
            std::thread::sleep(Duration::from_millis(5));
        }
        c.lock().unwrap().get_mut(&req.model).unwrap().0 -= 1;
        Ok(answer("done"))
    });
    // The conductor's model is the first one; the crew roles run on the other two.
    let b = brains(&[("cond", provider.clone())]);
    for id in ["alpha", "gamma"] {
        b.with_library(|l| l.add(&ModelInfo::bare(ProviderKind::Ollama, id))).unwrap();
    }
    with_slots(&b, "cond", 4);
    let alpha = with_slots(&b, "alpha", 2);
    let gamma = with_slots(&b, "gamma", 3);
    b.set_slot(&Slot::Crew("tester".into()), Some(&alpha), Effort::Low).unwrap();
    b.set_slot(&Slot::Crew("tester2".into()), Some(&gamma), Effort::Low).unwrap();
    assert_eq!((b.context().crew_slots(&alpha), b.context().crew_slots(&gamma)), (2, 3), "the conductor is on neither");
    let fx = start_with(&home, b, CrewConfig::default(), false);
    let mut ids = dispatch_n(&fx.sup, "tester", 4);
    ids.extend(dispatch_n(&fx.sup, "tester2", 5));
    wait("five running", || count(&fx.sup, &ids, CrewRunStatus::Running) == 5);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(count(&fx.sup, &ids, CrewRunStatus::Running), 5, "2 on alpha and 3 on gamma at once");
    release.store(true, Ordering::SeqCst);
    for id in &ids {
        wait_status(&fx.sup, id, CrewRunStatus::Done);
    }
    let peaks = counts.lock().unwrap().iter().map(|(m, v)| (m.clone(), v.1)).collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(peaks.get("alpha"), Some(&2), "{peaks:?}");
    assert_eq!(peaks.get("gamma"), Some(&3), "{peaks:?}");
    finish(fx, &[&home]);
}

#[test]
fn cloud_runs_still_obey_per_provider() {
    let home = home("slots-cloud");
    let (now, peak) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let (n, p) = (now.clone(), peak.clone());
    let remote = cloud(move |_, _, _| {
        p.fetch_max(n.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(80));
        n.fetch_sub(1, Ordering::SeqCst);
        Ok(answer("cloud done"))
    });
    let fx = start_with(&home, brains(&[("gpt", remote)]), CrewConfig { per_provider: 2, ..CrewConfig::default() }, false);
    let ids = dispatch_n(&fx.sup, "tester", 4);
    for id in &ids {
        wait_status(&fx.sup, id, CrewRunStatus::Done);
    }
    assert_eq!(peak.load(Ordering::SeqCst), 2, "two per cloud provider, whatever the slots say");
    finish(fx, &[&home]);
}

#[test]
fn a_busy_gpu_moves_local_runs_to_a_cloud_slot_or_holds_them() {
    // Local default, no cloud model: the run waits for the GPU.
    let home1 = home("gpu-wait");
    let local = Scripted::new(ProviderKind::Ollama, note_then_answer("local answer"));
    let fx = start_with(&home1, brains(&[("qwen", local.clone())]), CrewConfig::default(), true);
    let id = fx.sup.dispatch(None, args("tester", "x", None, None)).unwrap().run_id;
    wait("the GPU wait", || info(&fx.sup, &id).current_action.as_deref() == Some("waiting for the GPU"));
    assert_eq!(info(&fx.sup, &id).status, CrewRunStatus::Queued);
    assert_eq!(local.calls.load(Ordering::SeqCst), 0);
    finish(fx, &[]);

    // A cloud model assigned to a slot: the run moves there.
    let home2 = home("gpu-cloud");
    let local = Scripted::new(ProviderKind::Ollama, note_then_answer("local answer"));
    let remote = cloud(note_then_answer("cloud answer"));
    let b = brains(&[("qwen", local.clone()), ("gpt", remote.clone())]);
    let gpt = b.with_library(|l| l.models().unwrap()).into_iter().find(|m| m.info.id == "gpt").unwrap().model;
    b.set_slot(&Slot::Quick, Some(&gpt), Effort::Low).unwrap();
    let fx = start_with(&home2, b, CrewConfig::default(), true);
    let id = fx.sup.dispatch(None, args("tester", "x", None, None)).unwrap().run_id;
    let run = wait_status(&fx.sup, &id, CrewRunStatus::Done);
    assert_eq!(run.result.unwrap().summary, "cloud answer");
    assert_eq!(local.calls.load(Ordering::SeqCst), 0);
    finish(fx, &[&home1, &home2]);
}

#[test]
fn a_worktree_run_waits_for_merge_and_merge_fast_forwards_the_checkout() {
    let home = home("merge");
    let (work, _) = repo("merge");
    let provider = cloud(|req, _, _| {
        Ok(if after_tool(req) {
            answer("Added hello.txt.")
        } else {
            call("write", json!({"path": "hello.txt", "content": "hi\n"}))
        })
    });
    let fx = start(&home, &provider);
    let id = fx.sup.dispatch(None, args("builder", "Add hello.txt", None, Some(&work))).unwrap().run_id;
    let run = wait_status(&fx.sup, &id, CrewRunStatus::NeedsYou);
    let diff = run.result.as_ref().and_then(|r| r.diff.clone()).expect("a diff");
    assert_eq!((diff.branch.as_str(), diff.files.as_slice(), diff.insertions), (branch_name(&id).as_str(), &["hello.txt".to_string()][..], 1));
    let sandbox = run.sandbox.unwrap();
    assert_eq!(sandbox.kind, SandboxKind::Worktree);
    assert!(!work.join("hello.txt").exists(), "the original checkout is untouched before merge");

    let before = git(&work, &["rev-parse", "HEAD"]);
    let outcome = fx.sup.merge(&id).unwrap();
    let head = git(&work, &["rev-parse", "HEAD"]);
    assert_eq!(outcome, MergeOutcome::FastForward { head: head.clone() });
    assert_ne!(head, before);
    assert_eq!(git(&work, &["rev-parse", "--abbrev-ref", "HEAD"]), "main");
    assert_eq!(std::fs::read_to_string(work.join("hello.txt")).unwrap().replace("\r\n", "\n"), "hi\n");
    assert!(!PathBuf::from(&sandbox.root).exists(), "the worktree is removed after merge");
    assert!(git(&work, &["branch", "--list", &branch_name(&id)]).is_empty());
    let done = info(&fx.sup, &id);
    assert_eq!(done.status, CrewRunStatus::Done);
    assert!(done.note.unwrap().starts_with("merged: fast-forward"));
    finish(fx, &[&home, work.parent().unwrap()]);
}

#[test]
fn discard_removes_the_worktree_and_its_branch() {
    let home = temp_dir("discard");
    let (work, _) = repo("discard");
    let run = RunId::new_random();
    let sb = Sandbox::create(&home, &run, Some(&work), SandboxKind::Worktree).unwrap();
    assert_eq!(sb.kind, SandboxKind::Worktree);
    assert_eq!(sb.root, home.join("worktrees").join(&run.as_str()[..10]), "a short directory: the repository's deepest paths go under it");
    std::fs::write(sb.root.join("scratch.txt"), "x").unwrap();
    assert_eq!(sb.diff_summary().unwrap().files, ["scratch.txt"]);
    assert_eq!(Sandbox::create(&home, &run, Some(&work), SandboxKind::Worktree).unwrap(), sb, "idempotent");
    sb.discard().unwrap();
    assert!(!sb.root.exists());
    assert!(git(&work, &["branch", "--list", &branch_name(&run)]).is_empty());
    assert!(git(&work, &["worktree", "list"]).lines().count() == 1);
    assert!(!work.join("scratch.txt").exists());

    // Not a git workspace: a scratch dir instead.
    let plain = temp_dir("plain");
    let sb = Sandbox::create(&home, &run, Some(&plain), SandboxKind::Worktree).unwrap();
    assert_eq!((sb.kind, sb.root.clone()), (SandboxKind::Scratch, home.join("scratch").join(run.as_str())));
    sb.discard().unwrap();
    assert!(!sb.root.exists());
    remove(&[&home, &plain, work.parent().unwrap()]);
}

#[test]
fn a_crew_shell_cannot_push() {
    let home = temp_dir("push");
    let (work, bare) = repo("push");
    // Even an explicit pushurl in the repo's own config is overridden.
    git(&work, &["config", "remote.origin.pushurl", bare.to_str().unwrap()]);
    let run = RunId::new_random();
    let sb = Sandbox::create(&home, &run, Some(&work), SandboxKind::Worktree).unwrap();
    let env = sb.child_env();
    let mut want = no_push_git_env(&["origin".into()]);
    // The worktree's own build output dir, under its repository's.
    let own = marlowe_crew::sandbox::target_dir(&home, sb.origin.as_ref().unwrap(), run.as_str());
    want.push(("CARGO_TARGET_DIR".into(), own.display().to_string()));
    assert_eq!(env, want);

    let crew_git = |args: &[&str]| {
        let mut cmd = Command::new("git");
        cmd.current_dir(&sb.root).args(args);
        for (k, v) in &env {
            cmd.env(k, v);
        }
        cmd.output().unwrap()
    };
    let urls = crew_git(&["remote", "get-url", "--push", "--all", "origin"]);
    assert_eq!(String::from_utf8_lossy(&urls.stdout).trim(), NO_PUSH_URL);
    let push = crew_git(&["push", "origin", "HEAD:refs/heads/stolen"]);
    assert!(!push.status.success(), "push must fail");
    let direct = crew_git(&["push", bare.to_str().unwrap(), "HEAD:refs/heads/stolen"]);
    assert!(!direct.status.success(), "a push to a bare path must fail too");
    assert!(git(&bare, &["branch", "--list"]).is_empty(), "nothing reached the remote");

    // The playbook shell carries the same environment.
    let r = playbook::sandbox_shell(&sb, "git remote get-url --push origin", &CancelToken::new()).unwrap();
    assert!(r.ok && r.output.trim() == NO_PUSH_URL, "{r:?}");
    sb.discard().unwrap();
    remove(&[&home, work.parent().unwrap()]);
}

#[test]
fn dangerous_shell_commands_ask_and_writes_stay_in_the_sandbox() {
    let root = PathBuf::from(if cfg!(windows) { r"C:\h\worktrees\r1" } else { "/h/worktrees/r1" });
    let sb = Sandbox {
        kind: SandboxKind::Worktree,
        root: root.clone(),
        origin: Some(PathBuf::from(if cfg!(windows) { r"C:\code\proj" } else { "/code/proj" })),
        branch: Some("crew/r1".into()),
        base: Some("abc".into()),
        repo_digest: None,
        frozen: Vec::new(),
        target_dir: None,
    };
    let asks = |c: &str| matches!(sb.classify_shell(c, false), ShellVerdict::Ask(_));
    for free in [
        "cargo test",
        "git status && git diff",
        "git commit -am wip",
        "git reset --hard HEAD~1",
        "rm -rf target",
        "rm -rf ./build/../dist",
        "Remove-Item -Recurse -Force .\\out",
        "git branch feature",
        "git -c core.pager=cat log",
    ] {
        assert!(!asks(free), "{free} should run free");
    }
    for ask in [
        "git push",
        "git push origin main",
        "cargo fmt; git -C . push --force",
        "git branch -f main HEAD",
        "git branch --force main",
        "git -C C:/code/proj reset --hard",
        "cd .. && git reset --hard",
        "rm -rf /",
        "rm -rf ../other",
        "rm -rf ~/projects",
        "rmdir /s /q C:\\Users",
        "Remove-Item -Recurse $env:USERPROFILE",
        "cmd /c \"git push\"",
    ] {
        assert!(asks(ask), "{ask} should ask");
    }
    assert!(matches!(sb.classify_shell("ls", true), ShellVerdict::Ask(s) if s.contains("untrusted")));

    assert!(sb.allows_write(Path::new("src/main.rs")));
    assert!(sb.allows_write(&root.join("a").join("..").join("b.txt")));
    assert!(!sb.allows_write(Path::new("../escape.txt")));
    assert!(!sb.allows_write(sb.origin.as_ref().unwrap()));
    if cfg!(windows) {
        assert!(sb.allows_write(Path::new(r"c:\H\WORKTREES\r1\x")), "case-insensitive on Windows");
    }
    let none = Sandbox { kind: SandboxKind::None, ..sb.clone() };
    assert!(!none.allows_write(Path::new("x")));
    assert!(matches!(none.classify_shell("rm -rf x", false), ShellVerdict::Ask(_)));
}

#[test]
fn the_store_round_trips_runs_and_steps() {
    let dir = temp_dir("store");
    let store = RunStore::open(&dir.join("cowork.db")).unwrap();
    let roles = Roles::builtin();
    let mut a = Run::new(None, roles.get("scout").unwrap(), "first", None, None, CrewBudget::default());
    a.created_ms -= 10;
    let mut b = Run::new(Some(SessionId("s".into())), roles.get("engineer").unwrap(), "second", None, None, CrewBudget::default());
    b.status = CrewRunStatus::Running;
    b.updated_ms += 10;
    let mut c = Run::new(None, roles.get("scout").unwrap(), "third", None, None, CrewBudget::default());
    c.status = CrewRunStatus::Done;
    for r in [&a, &b, &c] {
        store.save(r).unwrap();
    }
    assert_eq!(store.get(&b.id).unwrap().as_ref(), Some(&b));
    assert_eq!(store.get(&RunId("nope".into())).unwrap(), None);
    assert_eq!(store.list(1).unwrap()[0].id, b.id);
    let ids: Vec<RunId> = store.resumable().unwrap().into_iter().map(|r| r.id).collect();
    assert_eq!(ids, [a.id.clone(), b.id.clone()]);

    let rec = StepRecord {
        run: b.id.clone(),
        step: 1,
        key: "plan".into(),
        role: "engineer".into(),
        action: "plan".into(),
        messages: vec![marlowe_model::Message::user("Goal: x"), marlowe_model::Message::assistant("1. do it")],
        output: Some("1. do it".into()),
        usage: Usage { input: 5, output: 3, ..Default::default() },
        tainted: false,
        truncated: false,
        at_ms: 1,
    };
    store.append_step(&rec).unwrap();
    let steps = store.steps(&b.id).unwrap();
    assert_eq!(steps, std::slice::from_ref(&rec));
    let view = steps[0].view();
    assert_eq!(view.text, "› Goal: x\n1. do it");
    drop(store);
    let reopened = RunStore::open(&dir.join("cowork.db")).unwrap();
    assert_eq!(reopened.steps(&b.id).unwrap().len(), 1);
    drop(reopened);
    remove(&[&dir]);
}

#[test]
fn the_result_memory_carries_the_run_as_provenance() {
    let roles = Roles::builtin();
    let mut run = Run::new(None, roles.get("researcher").unwrap(), "LoRA rank", Some(Playbook::DeepResearch), Some(PathBuf::from("C:/w")), CrewBudget::default());
    run.tainted = true;
    let d = Dispatch {
        summary: "Rank barely matters above 8.".into(),
        artifacts: vec![],
        confidence: Confidence::Medium,
        citations: vec![Citation { title: "Hu 2021".into(), url_or_path: "https://arxiv.org/abs/2106.09685".into() }],
        diff: None,
    };
    let m = result_memory(&run, &d);
    assert!(m.text.contains("Rank barely matters") && m.text.contains("[Hu 2021](https://arxiv.org/abs/2106.09685)"));
    assert!(m.text.ends_with("Confidence: medium."));
    assert_eq!(m.kind, marlowe_mind::Kind::Result);
    assert_eq!(m.scope, marlowe_proto::MemoryScope::Project("C:/w".into()));
    assert_eq!(m.source, marlowe_mind::Provenance::Agent { session: format!("crew:{}", run.id), turn: 0, tainted: true });
}

#[test]
fn notify_run_checks_the_run_and_frames_the_message_as_another_agents() {
    let home = home("notify");
    let (release, gate) = crossbeam_channel::bounded::<()>(1);
    let provider = cloud(move |_, n, cancel| {
        if n == 0 {
            while gate.recv_timeout(Duration::from_millis(10)).is_err() {
                if cancel.is_cancelled() {
                    return Err(ProviderError::cancelled());
                }
            }
            Ok(call("note", json!({"text": "first look"})))
        } else {
            Ok(answer("Done."))
        }
    });
    let fx = start(&home, &provider);
    let notice = RunNotice { from: "session:other".into(), text: "the hub file moved".into() };
    assert!(matches!(fx.sup.notify_run(&RunId::new_random(), notice.clone()), Err(CrewError::UnknownRun(_))));
    let id = fx.sup.dispatch(None, args("tester", "Evaluate the model", None, None)).unwrap().run_id;
    wait("the first model call", || provider.calls.load(Ordering::SeqCst) == 1);
    fx.sup.notify_run(&id, notice).unwrap();
    release.send(()).unwrap();
    wait_status(&fx.sup, &id, CrewRunStatus::Done);
    let reqs = provider.requests();
    let texts: Vec<String> = reqs.iter().flat_map(|r| r.messages.iter().map(|m| m.joined_text())).collect();
    // Never the bare text, as if the user had said it: always framed with its sender.
    assert!(!texts.iter().any(|t| t.trim() == "the hub file moved"));
    assert!(texts.iter().any(|t| t.starts_with("[Message from session:other") && t.ends_with("the hub file moved")));
    finish(fx, &[&home]);
}

/// G11/G12: a workflow run (`/code-review`, `/simplify`) starts from the checkout as it is. Its
/// agents' `git diff` sees the uncommitted changes, and its own edits merge back into the working
/// tree, uncommitted, without touching the user's pending work or HEAD.
#[test]
fn a_workflow_run_sees_the_pending_changes_and_merges_into_the_working_tree() {
    let home = home("wf-pending");
    std::fs::write(
        home.join("roles").join("workflow.md"),
        format!(
            "---
name: workflow
description: Test workflow agent.
tools: [git_diff, write]
             budget: {{tokens: 100000, steps: 20, usd: 1.0}}
sandbox: worktree
---
{CARD_BODY}"
        ),
    )
    .unwrap();
    let (work, _bare) = repo("wf-pending");
    let head = git(&work, &["rev-parse", "HEAD"]);
    // Uncommitted: a planted bug in a tracked file, a staged new file, an untracked one.
    std::fs::write(work.join("README.md"), "hello
let last = items[n + 1]; // planted
").unwrap();
    std::fs::write(work.join("staged.txt"), "staged
").unwrap();
    git(&work, &["add", "staged.txt"]);
    std::fs::write(work.join("notes.txt"), "untracked
").unwrap();

    let provider = cloud(|req, _, _| {
        let prompt = req.messages.iter().find(|m| m.role == Role::User).map(|m| m.joined_text()).unwrap_or_default();
        Ok(match (prompt.contains("Review"), after_tool(req)) {
            (true, false) => call("git_diff", json!({})),
            (true, true) => answer(&serde_json::to_string(req.messages.last().unwrap()).unwrap()),
            (false, false) => call("write", json!({"path": "README.md", "content": "hello
let last = items[n - 1]; // fixed
"})),
            (false, true) => answer("Fixed README.md."),
        })
    });
    let fx = start(&home, &provider);
    let source = "META = {\"description\": \"review then fix\"}
def main(args):
    seen = agent(\"Review the pending changes\")
    fixed = agent(\"Fix the finding\")
    return {\"seen\": seen, \"fixed\": fixed}
";
    let script = home.join("wf.star");
    std::fs::write(&script, source).unwrap();
    let receipt = fx
        .sup
        .dispatch_workflow(None, args("workflow", "workflow t: review then fix", None, Some(&work)), |_run| {
            Ok(marlowe_proto::WorkflowRef {
                name: Some("t".into()),
                script_path: script.display().to_string(),
                digest: marlowe_workflow::digest(source),
                args: Value::Null,
                foreground: false,
                resumed_from: None,
            })
        })
        .unwrap();
    let run = wait_status(&fx.sup, &receipt.run_id, CrewRunStatus::NeedsYou);

    // The finder's `git diff` in its sandbox had the uncommitted planted bug.
    let journal = std::fs::read_to_string(home.join("workflows").join("runs").join(receipt.run_id.as_str()).join("journal.jsonl")).unwrap();
    assert!(journal.contains("items[n + 1]"), "the finder saw the pending change: {journal}");
    let root = PathBuf::from(run.sandbox.clone().unwrap().root);
    assert!(root.join("notes.txt").is_file() && root.join("staged.txt").is_file(), "untracked and staged files came along");
    assert!(git(&work, &["for-each-ref", "refs/marlowe-seed"]).contains(receipt.run_id.as_str()), "the snapshot is kept by its ref");
    // The run's diff is its own change only, not the user's pending work.
    let diff = run.result.as_ref().and_then(|d| d.diff.clone()).expect("the fix is a diff");
    assert_eq!(diff.files, ["README.md"]);
    assert_eq!(diff.insertions, 1);

    let outcome = fx.sup.merge(&receipt.run_id).unwrap();
    assert_eq!(outcome, MergeOutcome::Applied { files: vec!["README.md".into()] });
    assert_eq!(std::fs::read_to_string(work.join("README.md")).unwrap(), "hello
let last = items[n - 1]; // fixed
");
    assert_eq!(git(&work, &["rev-parse", "HEAD"]), head, "nothing committed to the user's branch");
    assert!(work.join("notes.txt").is_file() && git(&work, &["diff", "--cached", "--name-only"]) == "staged.txt", "the user's pending work is as it was");
    assert!(git(&work, &["for-each-ref", "refs/marlowe-seed"]).is_empty(), "the snapshot ref goes with the worktree");
    finish(fx, &[&home, work.parent().unwrap()]);
}

/// A workflow whose agents only read never makes a worktree (one in a git clone hit Windows' path
/// limit on test fixtures): its agents read the workspace itself, relative paths included.
#[test]
fn a_read_only_workflow_in_a_git_repo_makes_no_worktree_and_reads_the_workspace() {
    let home = home("wf-read-only");
    std::fs::write(
        home.join("roles").join("workflow.md"),
        format!("---
name: workflow
description: Test workflow agent.
tools: [read, write]
budget: {{tokens: 100000, steps: 20, usd: 1.0}}
sandbox: worktree
---
{CARD_BODY}"),
    )
    .unwrap();
    let (work, _bare) = repo("wf-read-only");
    let provider = cloud(|req, _, _| {
        assert!(!req.tools.iter().any(|t| t.name == "write"), "a read-only agent has no write tool");
        Ok(if after_tool(req) {
            answer(&format!("read: {}", serde_json::to_string(req.messages.last().unwrap()).unwrap()))
        } else {
            call("read", json!({"path": "README.md"}))
        })
    });
    let fx = start(&home, &provider);
    let source = "META = {\"description\": \"read twice\"}
def main(args):
    return parallel([lambda: agent(\"Summarize\", agent_type = \"explore\"), lambda: agent(\"Again\", agent_type = \"explore\")])
";
    let script = home.join("wf.star");
    std::fs::write(&script, source).unwrap();
    let receipt = fx
        .sup
        .dispatch_workflow(None, args("workflow", "workflow t: read", None, Some(&work)), |_run| {
            Ok(marlowe_proto::WorkflowRef {
                name: Some("t".into()),
                script_path: script.display().to_string(),
                digest: marlowe_workflow::digest(source),
                args: Value::Null,
                foreground: false,
                resumed_from: None,
            })
        })
        .unwrap();
    let run = wait_status(&fx.sup, &receipt.run_id, CrewRunStatus::Done);
    assert_eq!(run.sandbox.as_ref().map(|s| s.kind), Some(SandboxKind::Scratch));
    let journal = std::fs::read_to_string(home.join("workflows").join("runs").join(receipt.run_id.as_str()).join("journal.jsonl")).unwrap();
    let agents: Vec<&str> = journal.lines().filter(|l| l.contains("\"type\":\"agent\"")).collect();
    assert_eq!(agents.len(), 2, "{journal}");
    assert!(agents.iter().all(|l| l.contains("hello")), "both read the workspace's README by its relative path: {journal}");
    assert!(!home.join("worktrees").exists(), "no worktree");
    assert!(git(&work, &["branch", "--list", "crew/*"]).is_empty() && git(&work, &["worktree", "list"]).lines().count() == 1);
    assert!(run.result.and_then(|d| d.diff).is_none());
    finish(fx, &[&home, work.parent().unwrap()]);
}
