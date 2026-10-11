//! Fixtures for the git tools' tests: a real repository with a bare remote (git is real), a fake
//! `gh` (gh is not signed in here), and a scripted model driving the real agent loop.
#![allow(dead_code)]

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use marlowe_agent::{
    Agent, ApprovalGate, ApprovalReq, Conversation, Steer, SystemPrompt, ToolCtx, ToolHost, ToolOutcome, TurnOutcome,
};
use marlowe_exec::git::GitState;
use marlowe_exec::BuiltinTools;
use marlowe_model::{
    fold_events, CancelToken, ChatRequest, Completion, ModelInfo, Provider, ProviderError, ProviderKind, StopReason,
    StreamEvent, ToolCall,
};
use marlowe_proto::{ApprovalDecision, SessionId};
use serde_json::{json, Value};

static NEXT: AtomicU32 = AtomicU32::new(0);

const FAKE_GH: &str = include_str!("fake_gh.py");

/// A work tree with a bare `origin`, a profile home and a fake `gh`.
pub struct Fx {
    pub base: PathBuf,
    pub work: PathBuf,
    pub remote: PathBuf,
    pub home: PathBuf,
    pub fake: PathBuf,
}

pub fn run_git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("git runs");
    assert!(out.status.success(), "git {args:?} in {}: {}", dir.display(), String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

impl Fx {
    pub fn new(name: &str) -> Fx {
        let base = std::env::temp_dir().join(format!("marlowe-gitfx-{name}-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let (work, remote, home, fake) = (base.join("work"), base.join("origin.git"), base.join("home"), base.join("fakegh"));
        for d in [&work, &remote, &home, &fake] {
            std::fs::create_dir_all(d).unwrap();
        }
        run_git(&remote, &["init", "--bare", "-q", "-b", "main"]);
        run_git(&work, &["init", "-q", "-b", "main"]);
        for (k, v) in [("user.name", "Test User"), ("user.email", "test@example.com"), ("commit.gpgsign", "false"), ("core.autocrlf", "false")] {
            run_git(&work, &["config", k, v]);
        }
        std::fs::write(work.join("README.md"), "# demo\n").unwrap();
        run_git(&work, &["add", "README.md"]);
        run_git(&work, &["commit", "-q", "-m", "initial commit"]);
        run_git(&work, &["remote", "add", "origin", remote.to_str().unwrap()]);
        run_git(&work, &["push", "-q", "-u", "origin", "main"]);
        run_git(&work, &["remote", "set-head", "origin", "main"]);
        std::fs::write(fake.join("gh.py"), FAKE_GH).unwrap();
        std::fs::write(fake.join("responses.json"), "{}").unwrap();
        Fx { base, work, remote, home, fake }
    }

    /// A host rooted at the work tree, configured like the binary's, with `gh` pointed at the fake.
    pub fn tools(&self) -> BuiltinTools {
        BuiltinTools::new(Some(self.work.clone()))
            .with_state(GitState::new(Some(self.home.clone())).with_gh(["python".to_string(), self.fake.join("gh.py").to_string_lossy().into_owned()]))
    }

    /// Makes `origin` look like a GitHub repository (`test/repo`) while pushes still go to the
    /// local bare repository, so slugs and `--repo` are exercised without a network.
    pub fn github_remote(&self) {
        self.git(&["config", "remote.origin.pushurl", self.remote.to_str().unwrap()]);
        self.git(&["remote", "set-url", "origin", "https://github.com/test/repo.git"]);
    }

    pub fn git(&self, args: &[&str]) -> String {
        run_git(&self.work, args)
    }

    pub fn put(&self, rel: &str, content: &str) {
        let p = self.work.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    /// Commits `rel` with `content` on the current branch (setup, not through the tool).
    pub fn commit_file(&self, rel: &str, content: &str, message: &str) -> String {
        self.put(rel, content);
        self.git(&["add", rel]);
        self.git(&["commit", "-q", "-m", message]);
        self.git(&["rev-parse", "HEAD"])
    }

    /// Sets what the fake `gh` answers: `key` is words that must all appear in the call.
    pub fn respond(&self, key: &str, stdout: Value, code: i32) {
        self.respond_full(key, stdout, "", code)
    }

    pub fn respond_full(&self, key: &str, stdout: Value, stderr: &str, code: i32) {
        let path = self.fake.join("responses.json");
        let mut all: serde_json::Map<String, Value> =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap_or_default();
        all.insert(key.into(), json!({"stdout": stdout, "stderr": stderr, "code": code}));
        std::fs::write(path, serde_json::to_string(&all).unwrap()).unwrap();
    }

    /// Every call the fake `gh` has received.
    pub fn gh_calls(&self) -> Vec<Value> {
        std::fs::read_to_string(self.fake.join("calls.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    /// The argv of each fake `gh` call, joined.
    pub fn gh_lines(&self) -> Vec<String> {
        self.gh_calls()
            .iter()
            .map(|c| c["argv"].as_array().unwrap().iter().filter_map(|a| a.as_str()).collect::<Vec<_>>().join(" "))
            .collect()
    }

    pub fn remote_branches(&self) -> Vec<String> {
        run_git(&self.remote, &["branch", "--format=%(refname:short)"]).lines().map(str::to_string).collect()
    }
}

impl Drop for Fx {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

pub fn ctx(root: &Path) -> ToolCtx {
    ToolCtx { session: SessionId::from("test"), turn: 1, workspace: Some(root.to_path_buf()), cancel: CancelToken::new(), tainted: false }
}

pub fn req(name: &str, args: Value) -> ToolCall {
    ToolCall { id: "c1".into(), name: name.into(), args }
}

/// Calls a tool directly (no approval gate, no middleware).
pub fn call(tools: &BuiltinTools, name: &str, args: Value) -> ToolOutcome {
    tools.call(&req(name, args), &ctx(tools.workspace().unwrap()))
}

// ---------------------------------------------------------------- a scripted model and the real loop

pub enum Turn {
    /// Tool calls in one step.
    Calls(Vec<(&'static str, Value)>),
    Say(&'static str),
}

pub struct Scripted {
    steps: Mutex<VecDeque<Turn>>,
    pub requests: Mutex<Vec<ChatRequest>>,
}

impl Scripted {
    pub fn new(steps: Vec<Turn>) -> Arc<Self> {
        Arc::new(Self { steps: Mutex::new(steps.into()), requests: Mutex::new(Vec::new()) })
    }
}

fn model() -> ModelInfo {
    ModelInfo { context_window: Some(32_000), max_output: Some(2_000), ..ModelInfo::bare(ProviderKind::Ollama, "m") }
}

impl Provider for Scripted {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Ollama
    }

    fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(vec![model()])
    }

    fn stream(&self, req: &ChatRequest, _cancel: &CancelToken, on: &mut dyn FnMut(StreamEvent)) -> Result<Completion, ProviderError> {
        self.requests.lock().unwrap().push(req.clone());
        let turn = self.steps.lock().unwrap().pop_front().unwrap_or(Turn::Say("(script over)"));
        let events: Vec<StreamEvent> = match turn {
            Turn::Say(t) => vec![StreamEvent::TextDelta(t.into()), StreamEvent::Stop(StopReason::EndTurn)],
            Turn::Calls(calls) => {
                let mut evs = Vec::new();
                for (i, (name, args)) in calls.into_iter().enumerate() {
                    evs.push(StreamEvent::ToolCallStart { index: i as u32, id: format!("call-{i}"), name: name.into() });
                    evs.push(StreamEvent::ToolArgsDelta { index: i as u32, json: args.to_string() });
                    evs.push(StreamEvent::ToolCallEnd { index: i as u32 });
                }
                evs.push(StreamEvent::Stop(StopReason::ToolUse));
                evs
            }
        };
        for e in &events {
            on(e.clone());
        }
        Ok(fold_events(&events))
    }
}

/// Counts the approvals asked and answers each with `decision`.
pub struct Gate {
    pub decision: ApprovalDecision,
    pub asked: Mutex<Vec<ApprovalReq>>,
}

impl Gate {
    pub fn new(decision: ApprovalDecision) -> Arc<Self> {
        Arc::new(Self { decision, asked: Mutex::new(Vec::new()) })
    }

    pub fn count(&self) -> usize {
        self.asked.lock().unwrap().len()
    }

    pub fn asks(&self) -> Vec<ApprovalReq> {
        self.asked.lock().unwrap().clone()
    }
}

impl ApprovalGate for Gate {
    fn ask(&self, req: ApprovalReq) -> ApprovalDecision {
        self.asked.lock().unwrap().push(req);
        self.decision
    }
}

pub struct Driven {
    pub outcome: TurnOutcome,
    pub convo: Conversation,
    pub provider: Arc<Scripted>,
}

impl Driven {
    /// The content of tool result number `n` (in call order across the whole run).
    pub fn results(&self) -> Vec<(String, bool)> {
        self.convo
            .messages
            .iter()
            .flat_map(|m| m.tool_results_iter().map(|r| (r.content.clone(), r.is_error)).collect::<Vec<_>>())
            .collect()
    }
}

/// Runs one turn of the real agent loop over `tools`, with `gate` answering approvals.
pub fn drive(tools: BuiltinTools, gate: Arc<Gate>, script: Vec<Turn>) -> Driven {
    let provider = Scripted::new(script);
    let host: Arc<dyn ToolHost> = Arc::new(tools);
    let mut agent = Agent::new(provider.clone(), model(), host, SystemPrompt::with_core("core"));
    agent.approvals = gate;
    let mut convo = Conversation::new(SessionId::from("s1"), None);
    let (_tx, rx) = crossbeam_channel::unbounded::<Steer>();
    let outcome = agent.run_turn(&mut convo, "go", &CancelToken::new(), &rx, &mut |_| {});
    Driven { outcome, convo, provider }
}

/// `drive` with the work tree as the conversation's workspace.
pub fn drive_in(fx: &Fx, gate: Arc<Gate>, script: Vec<Turn>) -> Driven {
    let provider = Scripted::new(script);
    let host: Arc<dyn ToolHost> = Arc::new(fx.tools());
    let mut agent = Agent::new(provider.clone(), model(), host, SystemPrompt::with_core("core"));
    agent.approvals = gate;
    let mut convo = Conversation::new(SessionId::from("s1"), Some(fx.work.clone()));
    let (_tx, rx) = crossbeam_channel::unbounded::<Steer>();
    let outcome = agent.run_turn(&mut convo, "go", &CancelToken::new(), &rx, &mut |_| {});
    Driven { outcome, convo, provider }
}
