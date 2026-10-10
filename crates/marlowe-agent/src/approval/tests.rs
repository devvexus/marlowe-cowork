//! Modes, rules and "don't ask again" through the loop's call runner (design 2.3 M1-M5, M11,
//! A16), against a fake tool host. Shared helpers for `hooks` tests live here too.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use marlowe_model::{
    fold_events, CancelToken, ChatRequest, Completion, ModelInfo, Provider, ProviderError, ProviderKind, StopReason, StreamEvent,
    ToolCall, ToolResult, ToolSpec,
};
use marlowe_permission::shell::pwsh_ast::{AstOutcome, PwshAst};
use marlowe_proto::{ApprovalDecision, PermissionMode, SessionId, SettingsScope};
use serde_json::{json, Value};

use super::*;
use crate::middleware::ToolMiddleware;
use crate::tools::{Effects, Profiles, Services, ToolCtx, ToolManifest, ToolOutcome};
use crate::{Agent, AgentEvent, Conversation, SystemPrompt};

// ---------------------------------------------------------------- shared support

/// A scratch directory, removed on drop.
pub(crate) struct Tmp(pub PathBuf);

impl Tmp {
    pub fn new(tag: &str) -> Tmp {
        let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        let p = std::env::temp_dir().join(format!("marlowe-e-{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        Tmp(p)
    }
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Tools with real manifests and the built-ins' consequences: shells always ask, edits ask
/// outside `root`, `web_fetch` asks for an unapproved host, MCP tools ask.
pub(crate) struct FakeHost {
    pub root: PathBuf,
    pub ran: Mutex<Vec<String>>,
}

impl FakeHost {
    pub fn new(root: &Path) -> Arc<Self> {
        Arc::new(FakeHost { root: root.to_path_buf(), ran: Mutex::new(Vec::new()) })
    }

    pub fn ran(&self) -> Vec<String> {
        self.ran.lock().unwrap().clone()
    }
}

fn arg<'a>(c: &'a ToolCall, k: &str) -> &'a str {
    c.args.get(k).and_then(Value::as_str).unwrap_or("")
}

impl ToolHost for FakeHost {
    fn specs(&self) -> Vec<ToolSpec> {
        Vec::new()
    }

    fn call(&self, call: &ToolCall, _ctx: &ToolCtx) -> ToolOutcome {
        let line = match call.name.as_str() {
            "bash" | "powershell" => format!("{} {}", call.name, arg(call, "command")),
            "edit" | "read" => format!("{} {}", call.name, arg(call, "path")),
            "web_fetch" => format!("web_fetch {}", arg(call, "url")),
            other => other.to_string(),
        };
        self.ran.lock().unwrap().push(line);
        if call.name == "fail" {
            return ToolOutcome::error("it failed");
        }
        ToolOutcome::ok("done", "done")
    }

    fn consequence(&self, call: &ToolCall) -> Consequence {
        match call.name.as_str() {
            "bash" | "powershell" | "python" => Consequence::Ask { summary: format!("{}: {}", call.name, arg(call, "command")) },
            "edit" => {
                let p = lexical_join(&self.root, Path::new(arg(call, "path")));
                if shell::within(&self.root, &p) {
                    Consequence::Free
                } else {
                    Consequence::Ask { summary: format!("edit {} (outside the workspace)", arg(call, "path")) }
                }
            }
            "web_fetch" => Consequence::Ask { summary: format!("web_fetch {}", arg(call, "url")) },
            n if n.starts_with("mcp__") => Consequence::Ask { summary: n.to_string() },
            _ => Consequence::Free,
        }
    }

    fn manifest(&self, name: &str) -> Option<ToolManifest> {
        let (group, effects, path_args): (ToolGroup, Effects, &'static [&'static str]) = match name {
            "bash" | "powershell" | "python" => (ToolGroup::Shell, Effects::RUN, &[]),
            "edit" => (ToolGroup::Files, Effects::WRITE, &["path"]),
            "read" => (ToolGroup::Files, Effects::READ, &["path"]),
            "web_fetch" => (ToolGroup::Web, Effects::NETWORK, &[]),
            "fail" => (ToolGroup::Agent, Effects::NONE, &[]),
            n if n.starts_with("mcp__") => (ToolGroup::Mcp, Effects::RUN, &[]),
            _ => return None,
        };
        Some(ToolManifest { group, profiles: Profiles::ALL, effects, path_args })
    }
}

/// Answers every approval with a fixed decision and records what was asked.
pub(crate) struct Recording {
    pub answer: Mutex<VecDeque<ApprovalDecision>>,
    pub asked: Mutex<Vec<(String, Option<String>)>>,
}

impl Recording {
    pub fn new(answers: &[ApprovalDecision]) -> Arc<Self> {
        Arc::new(Recording { answer: Mutex::new(answers.iter().copied().collect()), asked: Mutex::new(Vec::new()) })
    }

    pub fn asked(&self) -> Vec<String> {
        self.asked.lock().unwrap().iter().map(|(s, _)| s.clone()).collect()
    }
}

impl ApprovalGate for Recording {
    fn ask(&self, req: ApprovalReq) -> ApprovalDecision {
        self.ask_with(req, None)
    }
    fn ask_with(&self, req: ApprovalReq, proposed: Option<String>) -> ApprovalDecision {
        self.asked.lock().unwrap().push((req.summary, proposed));
        self.answer.lock().unwrap().pop_front().unwrap_or(ApprovalDecision::Deny)
    }
}

pub(crate) fn call(id: &str, name: &str, args: Value) -> ToolCall {
    ToolCall { id: id.into(), name: name.into(), args }
}

pub(crate) fn sh(id: &str, cmd: &str) -> ToolCall {
    call(id, "bash", json!({ "command": cmd }))
}

/// Runs one batch through the loop's call runner.
pub(crate) fn batch(
    host: &dyn ToolHost,
    middleware: &[Arc<dyn ToolMiddleware>],
    services: &Services,
    gate: &dyn ApprovalGate,
    calls: &[ToolCall],
    convo: &mut Conversation,
) -> Vec<ToolResult> {
    crate::run_tools::run_calls(host, middleware, services, gate, calls, convo, 0, &CancelToken::new(), &mut |_e: AgentEvent| {})
}

/// A provider that plays back steps of stream events.
pub(crate) struct Script {
    steps: Mutex<VecDeque<Vec<StreamEvent>>>,
    pub requests: Mutex<Vec<ChatRequest>>,
}

impl Script {
    pub fn new(steps: Vec<Vec<StreamEvent>>) -> Arc<Self> {
        Arc::new(Script { steps: Mutex::new(steps.into()), requests: Mutex::new(Vec::new()) })
    }
}

impl Provider for Script {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Ollama
    }
    fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(vec![model()])
    }
    fn stream(&self, req: &ChatRequest, _cancel: &CancelToken, on: &mut dyn FnMut(StreamEvent)) -> Result<Completion, ProviderError> {
        self.requests.lock().unwrap().push(req.clone());
        let evs = self.steps.lock().unwrap().pop_front().expect("the script ran out");
        for e in &evs {
            on(e.clone());
        }
        Ok(fold_events(&evs))
    }
}

pub(crate) fn model() -> ModelInfo {
    ModelInfo { context_window: Some(32_000), max_output: Some(2_000), ..ModelInfo::bare(ProviderKind::Ollama, "m") }
}

pub(crate) fn tool_step(id: &str, name: &str, args: Value) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ToolCallStart { index: 0, id: id.into(), name: name.into() },
        StreamEvent::ToolArgsDelta { index: 0, json: args.to_string() },
        StreamEvent::ToolCallEnd { index: 0 },
        StreamEvent::Stop(StopReason::ToolUse),
    ]
}

pub(crate) fn text_step(t: &str) -> Vec<StreamEvent> {
    vec![StreamEvent::TextDelta(t.into()), StreamEvent::Stop(StopReason::EndTurn)]
}

pub(crate) fn agent(provider: Arc<Script>, host: Arc<dyn ToolHost>, gate: Arc<dyn ApprovalGate>, services: Services) -> Agent {
    let mut a = Agent::new(provider, model(), host, SystemPrompt::new());
    a.approvals = gate;
    a.services = services;
    a
}

pub(crate) fn run_turn(a: &Agent, convo: &mut Conversation, text: &str) -> crate::TurnOutcome {
    let (_tx, rx) = crossbeam_channel::unbounded();
    a.run_turn(convo, text, &CancelToken::new(), &rx, &mut |_| {})
}

// ---------------------------------------------------------------- fixtures

struct Ws {
    _t: Tmp,
    home: PathBuf,
    root: PathBuf,
}

fn ws(tag: &str) -> Ws {
    let t = Tmp::new(tag);
    let home = t.0.join("home");
    let root = t.0.join("proj");
    std::fs::create_dir_all(&root).unwrap();
    Ws { home, root, _t: t }
}

/// A workspace outside the temp dir (the temp dir is always writable), with no files.
fn virtual_ws(tag: &str) -> Ws {
    let t = Tmp::new(tag);
    let home = t.0.join("home");
    let root = PathBuf::from(if cfg!(windows) { r"C:\ws\proj" } else { "/ws/proj" });
    Ws { home, root, _t: t }
}

fn perms(w: &Ws, mode: PermissionMode) -> Permissions {
    let p = Permissions::with_parser(&w.home, Some(&w.root), None, None);
    p.set_mode(mode);
    p
}

/// What each call of a batch came to: `ran`, `denied` (by the user), `refused` (by policy).
fn outcomes(host: &FakeHost, results: &[ToolResult], calls: &[ToolCall]) -> Vec<&'static str> {
    let ran = host.ran();
    calls
        .iter()
        .zip(results)
        .map(|(c, r)| {
            if r.content.starts_with("Not run:") {
                "refused"
            } else if r.content.starts_with("The user denied") {
                "denied"
            } else if ran.iter().any(|l| l.ends_with(arg(c, "command")) || l.ends_with(arg(c, "path"))) {
                "ran"
            } else {
                "?"
            }
        })
        .collect()
}

fn one(w: &Ws, mode: PermissionMode, c: ToolCall) -> (&'static str, Vec<String>) {
    let host = FakeHost::new(&w.root);
    let services = Services::new().with(perms(w, mode));
    let gate = Recording::new(&[]);
    let mut convo = Conversation::new(SessionId("s".into()), Some(w.root.clone()));
    let calls = [c];
    let r = batch(&*host, &[], &services, &*gate, &calls, &mut convo);
    let o = outcomes(&host, &r, &calls)[0];
    (if o == "denied" { "ask" } else { o }, gate.asked())
}

#[test]
fn parity_m2_default_shell_asks_edits_free_in_workspace() {
    let w = ws("m2");
    let (o, asked) = one(&w, PermissionMode::Default, sh("1", "cargo test"));
    assert_eq!(o, "ask");
    assert_eq!(asked.len(), 1);
    assert_eq!(one(&w, PermissionMode::Default, call("2", "edit", json!({"path": "src/lib.rs"}))).0, "ran");
    assert_eq!(one(&w, PermissionMode::Default, call("3", "edit", json!({"path": "../other/x.rs"}))).0, "ask");
    assert_eq!(one(&w, PermissionMode::Default, call("4", "read", json!({"path": "src/lib.rs"}))).0, "ran");
}

#[test]
fn parity_m3_accept_edits_rule_table() {
    let w = ws("m3");
    std::fs::create_dir_all(w.root.join(".marlowe")).unwrap();
    std::fs::write(w.root.join(".marlowe/settings.toml"), "[permissions]\nproject_commands = [\"bash(just test:*)\"]\n").unwrap();
    let s = marlowe_settings::Settings::load(&w.home, Some(&w.root), None);
    marlowe_settings::hooks::TrustStore::trust(&w.home, &w.root, &s.digest).unwrap();
    let m = PermissionMode::AcceptEdits;
    let rows: &[(&str, &str)] = &[
        ("ls -la", "ran"),
        ("git status && git log --oneline -5", "ran"),
        ("git diff HEAD~1", "ran"),
        ("mkdir -p src/new && touch src/new/mod.rs", "ran"),
        ("mv a.txt b.txt", "ran"),
        ("just test --verbose", "ran"),
        ("cargo build", "ask"),
        ("git commit -am x", "ask"),
        ("rm -rf target", "ask"),
        ("cat ~/.ssh/id_rsa", "ask"),
        ("git push", "ask"),
    ];
    for (i, (cmd, want)) in rows.iter().enumerate() {
        assert_eq!(one(&w, m, sh(&i.to_string(), cmd)).0, *want, "{cmd}");
    }
    assert_eq!(one(&w, m, call("e", "edit", json!({"path": "src/lib.rs"}))).0, "ran");
}

#[test]
fn parity_m4_bypass_frees_the_sandbox_and_keeps_tripwires() {
    let w = virtual_ws("m4");
    let m = PermissionMode::Bypass;
    let rows: &[(&str, &str)] = &[
        ("cargo test", "ran"),
        ("rm -rf target", "ran"),
        ("npm run build && git commit -am wip", "ran"),
        // A16
        ("rm -rf ../x", "ask"),
        ("curl https://new.host/x", "ask"),
        ("git push", "ask"),
        ("git push --force-with-lease", "refused"),
        ("git push origin +main", "refused"),
        ("git push --mirror", "refused"),
        ("git commit --no-verify -m x", "refused"),
    ];
    for (i, (cmd, want)) in rows.iter().enumerate() {
        assert_eq!(one(&w, m, sh(&i.to_string(), cmd)).0, *want, "{cmd}");
    }
    assert_eq!(one(&w, m, call("e", "edit", json!({"path": "../../etc/x"}))).0, "ask", "a write outside the workspace is a fixed approval");
    assert_eq!(one(&w, m, call("m", "mcp__srv__x", json!({}))).0, "ran");
}

#[test]
fn parity_m1_plan_mode_blocks_every_write_tool_and_exit_plan_restores() {
    let w = ws("m1");
    let p = perms(&w, PermissionMode::AcceptEdits);
    p.set_mode(PermissionMode::Plan);
    let host = FakeHost::new(&w.root);
    let services = Services::new().with(p.clone());
    let gate = Recording::new(&[ApprovalDecision::Allow]);
    let mut convo = Conversation::new(SessionId("s".into()), Some(w.root.clone()));
    let calls = [
        call("1", "edit", json!({"path": "src/lib.rs"})),
        sh("2", "cargo build"),
        call("3", "mcp__srv__write", json!({})),
        call("4", "read", json!({"path": "src/lib.rs"})),
        sh("5", "git status"),
    ];
    let r = batch(&*host, &[], &services, &*gate, &calls, &mut convo);
    let got = outcomes(&host, &r, &calls);
    assert_eq!(got, ["refused", "refused", "refused", "ran", "ran"], "{r:?}");
    assert!(r[0].content.contains("plan mode"));
    assert_eq!(gate.asked().len(), 1, "a read-only shell line still asks as in default");
    assert_eq!(p.exit_plan(), PermissionMode::AcceptEdits, "exit_plan returns to the mode plan was entered from");
}

#[test]
fn parity_m5_dont_ask_denial_reaches_the_model_and_the_run_recovers() {
    let w = ws("m5");
    let p = perms(&w, PermissionMode::DontAsk);
    let host = FakeHost::new(&w.root);
    let provider = Script::new(vec![tool_step("c1", "bash", json!({"command": "cargo build"})), text_step("I could not build; here is the plan instead.")]);
    let gate = Recording::new(&[]);
    let a = agent(provider.clone(), host.clone(), gate.clone(), Services::new().with(p));
    let mut convo = Conversation::new(SessionId("s".into()), Some(w.root.clone()));
    let out = run_turn(&a, &mut convo, "build it");
    assert_eq!(out.stop, crate::TurnStop::Done, "{out:?}");
    assert!(gate.asked().is_empty(), "dont_ask never asks");
    assert!(host.ran().is_empty());
    let second = &provider.requests.lock().unwrap()[1];
    let fed_back = format!("{:?}", second.messages);
    assert!(fed_back.contains("dont_ask") && fed_back.contains("cargo build"), "the reason reaches the model: {fed_back}");
}

#[test]
fn parity_m11_session_allow_is_keyed_by_pattern_and_always_survives_restart() {
    let w = ws("m11");
    let host = FakeHost::new(&w.root);
    let p = perms(&w, PermissionMode::Default);
    let services = Services::new().with(p.clone());
    let mut convo = Conversation::new(SessionId("s".into()), Some(w.root.clone()));

    // "Allow for this session" on `cargo test --release` proposes `bash(cargo test:*)`.
    let gate = Recording::new(&[ApprovalDecision::AllowPattern { scope: SettingsScope::Session }]);
    let calls = [sh("1", "cargo test --release"), sh("2", "cargo test -p core")];
    let r = batch(&*host, &[], &services, &*gate, &calls, &mut convo);
    assert_eq!(outcomes(&host, &r, &calls), ["ran", "ran"], "the grant covers the next call in the batch");
    let asked = gate.asked.lock().unwrap().clone();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].1.as_deref(), Some("bash(cargo test:*)"), "the proposal is the rule, never the bare tool");
    assert!(convo.session_allows.is_empty(), "nothing is allowed by tool name");

    let gate = Recording::new(&[]);
    for (i, cmd) in ["cargo publish", "git push", "cargo testx"].iter().enumerate() {
        let calls = [sh(&format!("n{i}"), cmd)];
        let r = batch(&*host, &[], &services, &*gate, &calls, &mut convo);
        assert_eq!(outcomes(&host, &r, &calls), ["denied"], "{cmd} still asks");
    }
    assert_eq!(gate.asked().len(), 3);

    // The TUI's "allow for session" key is the same grant.
    let gate = Recording::new(&[ApprovalDecision::AllowForSession]);
    let calls = [sh("3", "npm run lint -- --fix")];
    batch(&*host, &[], &services, &*gate, &calls, &mut convo);
    assert!(p.session_rules().contains(&"bash(npm run lint:*)".to_string()), "{:?}", p.session_rules());

    // "Always for this project" writes settings.local.toml; a new session (a restart) honours it.
    let gate = Recording::new(&[ApprovalDecision::AllowPattern { scope: SettingsScope::Local }]);
    let calls = [sh("4", "make check")];
    batch(&*host, &[], &services, &*gate, &calls, &mut convo);
    let local = std::fs::read_to_string(w.root.join(".marlowe/settings.local.toml")).unwrap();
    assert!(local.contains("bash(make check:*)"), "{local}");
    let restarted = perms(&w, PermissionMode::Default);
    let services = Services::new().with(restarted);
    let gate = Recording::new(&[]);
    let calls = [sh("5", "make check -j4"), sh("6", "cargo publish"), sh("7", "git push")];
    let r = batch(&*host, &[], &services, &*gate, &calls, &mut convo);
    let got = outcomes(&host, &r, &calls);
    assert_eq!(got[0], "ran", "{r:?}");
    assert_eq!(&got[1..], ["denied", "denied"]);
}

#[test]
fn tripwires_and_destructive_commands_are_never_widened() {
    let w = ws("wide");
    let host = FakeHost::new(&w.root);
    let services = Services::new().with(perms(&w, PermissionMode::Default));
    let mut convo = Conversation::new(SessionId("s".into()), Some(w.root.clone()));
    let gate = Recording::new(&[ApprovalDecision::AllowForSession, ApprovalDecision::AllowForSession]);
    let calls = [sh("1", "git push origin main"), sh("2", "rm -rf build")];
    batch(&*host, &[], &services, &*gate, &calls, &mut convo);
    let asked = gate.asked.lock().unwrap().clone();
    assert_eq!(asked[0].1, None, "a push is approved once, never as a rule");
    assert_eq!(asked[1].1.as_deref(), Some("bash(rm -rf build)"), "a destructive command only exactly");
    let gate = Recording::new(&[]);
    let calls = [sh("3", "git push origin main"), sh("4", "rm -rf build"), sh("5", "rm -rf build src")];
    let r = batch(&*host, &[], &services, &*gate, &calls, &mut convo);
    assert_eq!(outcomes(&host, &r, &calls), ["denied", "ran", "denied"]);
}

#[test]
fn hosts_approved_once_are_remembered_for_the_session() {
    let w = ws("hosts");
    let host = FakeHost::new(&w.root);
    let services = Services::new().with(perms(&w, PermissionMode::Bypass));
    let mut convo = Conversation::new(SessionId("s".into()), Some(w.root.clone()));
    let gate = Recording::new(&[ApprovalDecision::Allow]);
    let calls = [sh("1", "curl https://docs.rs/serde")];
    batch(&*host, &[], &services, &*gate, &calls, &mut convo);
    assert!(convo.exposure.approved_hosts.contains("docs.rs"));
    let gate = Recording::new(&[]);
    let calls = [sh("2", "curl https://docs.rs/tokio")];
    let r = batch(&*host, &[], &services, &*gate, &calls, &mut convo);
    assert_eq!(outcomes(&host, &r, &calls), ["ran"]);
}

struct FakeParser(AstOutcome);

impl PwshAst for FakeParser {
    fn parse(&self, _script: &str) -> AstOutcome {
        self.0.clone()
    }
}

#[test]
fn powershell_lines_freed_by_a_mode_must_read_the_same_to_powershell() {
    let w = ws("pwsh");
    let host = FakeHost::new(&w.root);
    let ps = |outcome: AstOutcome| {
        let p = Permissions::with_parser(&w.home, Some(&w.root), None, Some(Arc::new(FakeParser(outcome))));
        p.set_mode(PermissionMode::Bypass);
        p
    };
    let c = call("1", "powershell", json!({"command": "Get-ChildItem src"}));
    let lit = vec![vec!["Get-ChildItem".to_string(), "src".to_string()]];
    assert_eq!(ps(AstOutcome::Commands(lit)).decide(&*host, &c, &Exposure::default(), None), Gate::Free);
    for disagree in [AstOutcome::ParseErrors, AstOutcome::Unsupported, AstOutcome::Commands(vec![vec!["Remove-Item".into()]]), AstOutcome::Failed] {
        assert!(matches!(ps(disagree.clone()).decide(&*host, &c, &Exposure::default(), None), Gate::Ask { .. }), "{disagree:?}");
    }
}

/// Codex's `powershell_parser.rs` tests, against the real Windows PowerShell child.
#[cfg(windows)]
#[test]
fn codex_powershell_parser_child_protocol() {
    let p = PwshAstChild::new("powershell.exe");
    assert_eq!(p.parse("Get-Content 'foo bar'"), AstOutcome::Commands(vec![vec!["Get-Content".into(), "foo bar".into()]]));
    assert_eq!(
        p.parse("Write-Output foo | Measure-Object"),
        AstOutcome::Commands(vec![vec!["Write-Output".into(), "foo".into()], vec!["Measure-Object".into()]]),
        "the same child answers a second request"
    );
    for unsupported in [
        "git log --% HEAD --output=codex_poc.txt",
        "param([string]$path = (Get-Location)) Write-Output test",
        "begin { Set-Content codex_poc.txt pwned } end { Get-Content Cargo.toml }",
        "using module ./codex_poc.psm1\nGet-Content Cargo.toml",
        "trap { Set-Content codex_poc.txt pwned; continue } Get-Content missing -ErrorAction Stop",
    ] {
        assert_eq!(p.parse(unsupported), AstOutcome::Unsupported, "{unsupported}");
    }
    assert_eq!(p.parse("Get-Content 'unclosed"), AstOutcome::ParseErrors);
}
