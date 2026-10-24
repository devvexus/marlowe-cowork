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

// ---------------------------------------------------------------- review fixes

fn user_home() -> PathBuf {
    PathBuf::from(std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).expect("a home"))
}

/// Runs `calls` in one batch with `p`, nobody answering; what each came to, by the host's record.
fn run_with(p: &Permissions, root: &Path, tainted: bool, calls: &[ToolCall]) -> Vec<&'static str> {
    let host = FakeHost::new(root);
    let services = Services::new().with(p.clone());
    let gate = Recording::new(&[]);
    let mut convo = Conversation::new(SessionId("s".into()), Some(root.to_path_buf()));
    convo.exposure.tainted = tainted;
    let r = batch(&*host, &[], &services, &*gate, calls, &mut convo);
    let ran = host.ran();
    calls
        .iter()
        .zip(&r)
        .map(|(c, r)| {
            if r.content.starts_with("Not run:") {
                "refused"
            } else if r.content.starts_with("The user denied") {
                "ask"
            } else if ran.iter().any(|l| {
                let key = [arg(c, "command"), arg(c, "path"), arg(c, "url")].into_iter().find(|s| !s.is_empty()).unwrap_or("");
                l.starts_with(c.name.as_str()) && l.ends_with(key)
            }) {
                "ran"
            } else {
                "?"
            }
        })
        .collect()
}

#[test]
fn parity_m7_home_rules_apply_through_decide() {
    let w = virtual_ws("home");
    std::fs::create_dir_all(&w.home).unwrap();
    std::fs::write(w.home.join("settings.toml"), "[permissions]\ndeny = [\"Read(~/.ssh/**)\", \"Edit(~/.bashrc)\"]\n").unwrap();
    let home = user_home();
    let p = perms(&w, PermissionMode::Bypass);
    let path = |rel: &str| home.join(rel).display().to_string();
    let got = run_with(
        &p,
        &w.root,
        false,
        &[
            call("1", "read", json!({"path": path(".ssh/id_rsa")})),
            call("2", "edit", json!({"path": path(".bashrc")})),
            call("3", "read", json!({"path": path("notes.txt")})),
        ],
    );
    assert_eq!(got, ["refused", "refused", "ran"], "`~/` rules name the user's home");
}

#[test]
fn exit_plan_only_leaves_plan_and_another_mode_change_forgets_it() {
    let w = virtual_ws("plan");
    let p = perms(&w, PermissionMode::Bypass);
    p.set_mode(PermissionMode::Plan);
    p.set_mode(PermissionMode::Default);
    assert_eq!(p.exit_plan(), PermissionMode::Default, "bypass is not restored after the user left plan");
    assert_eq!(p.mode(), PermissionMode::Default);
    let p = perms(&w, PermissionMode::AcceptEdits);
    assert_eq!(p.exit_plan(), PermissionMode::AcceptEdits, "never in plan: nothing changes");
    p.set_mode(PermissionMode::Plan);
    p.set_mode(PermissionMode::Plan);
    assert_eq!(p.exit_plan(), PermissionMode::AcceptEdits);
}

#[test]
fn parity_a16_the_temp_dir_is_outside_and_only_the_session_scratch_is_free() {
    let w = virtual_ws("tmpdir");
    let p = perms(&w, PermissionMode::Bypass);
    let temp = std::env::temp_dir().display().to_string().replace('\\', "/");
    let temp = temp.trim_end_matches('/');
    let scratch = p.scratch_dir().display().to_string().replace('\\', "/");
    assert!(p.scratch_dir().starts_with(std::env::temp_dir()) && p.scratch_dir() != std::env::temp_dir());
    let rows: Vec<(String, &str)> = vec![
        (format!("rm -rf \"{temp}\""), "ask"),
        (format!("rm -rf \"{temp}/claude\""), "ask"),
        (format!("echo x > \"{temp}/x.txt\""), "ask"),
        (format!("rm -rf \"{scratch}/build\""), "ran"),
        (format!("echo x > \"{scratch}/x.txt\""), "ran"),
    ];
    let calls: Vec<ToolCall> = rows.iter().enumerate().map(|(i, (c, _))| sh(&i.to_string(), c)).collect();
    let got = run_with(&p, &w.root, false, &calls);
    for ((cmd, want), got) in rows.iter().zip(got) {
        assert_eq!(got, *want, "{cmd}");
    }
}

#[test]
fn settings_lift_a_tools_own_ask_for_allowed_hosts_and_added_roots() {
    let w = virtual_ws("lift");
    std::fs::create_dir_all(&w.home).unwrap();
    std::fs::write(w.home.join("settings.toml"), "[network]\nallow_hosts = [\"docs.rs\"]\n\n[permissions]\nallow = [\"WebFetch(domain:crates.io)\"]\n").unwrap();
    let p = perms(&w, PermissionMode::Default);
    let extra = w._t.0.join("extra");
    p.add_root(extra.clone());
    let calls = [
        call("1", "web_fetch", json!({"url": "https://docs.rs/serde"})),
        call("2", "web_fetch", json!({"url": "https://static.crates.io/x"})),
        call("3", "web_fetch", json!({"url": "https://evil.example/x"})),
        call("4", "edit", json!({"path": extra.join("src").join("x.rs").display().to_string()})),
        call("5", "edit", json!({"path": w._t.0.join("elsewhere").join("x.rs").display().to_string()})),
    ];
    let got = run_with(&p, &w.root, true, &calls);
    assert_eq!(got, ["ran", "ran", "ask", "ran", "ask"], "tainted session; allow_hosts, a domain rule and /add-dir answer the tools' asks");
}

#[test]
fn parity_m11_always_for_project_survives_restart_in_an_untrusted_project() {
    let w = ws("m11u");
    std::fs::create_dir_all(w.root.join(".claude")).unwrap();
    std::fs::write(
        w.root.join(".claude").join("settings.json"),
        r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"echo hi"}]}]}}"#,
    )
    .unwrap();
    let p = perms(&w, PermissionMode::Default);
    assert!(!p.settings().trusted, "a repository's hook is untrusted until the user trusts it");
    let host = FakeHost::new(&w.root);
    let mut convo = Conversation::new(SessionId("s".into()), Some(w.root.clone()));
    let gate = Recording::new(&[ApprovalDecision::AllowPattern { scope: SettingsScope::Local }]);
    batch(&*host, &[], &Services::new().with(p.clone()), &*gate, &[sh("1", "make check")], &mut convo);
    let local = std::fs::read_to_string(w.root.join(".marlowe/settings.local.toml")).unwrap();
    assert!(local.contains("bash(make check:*)"), "{local}");

    let restarted = perms(&w, PermissionMode::Default);
    let s = restarted.settings();
    assert!(!s.trusted, "granting a rule trusts nothing else");
    assert!(s.hooks().is_empty(), "the untrusted hook still does not run");
    assert!(s.rules().iter().any(|r| r.pattern == "bash(make check:*)"), "/permissions shows the rule the user added");
    let got = run_with(&restarted, &w.root, false, &[sh("2", "make check -j4"), sh("3", "cargo publish")]);
    assert_eq!(got, ["ran", "ask"]);

    // A rule the repository wrote itself is not the user's grant.
    std::fs::write(w.root.join(".marlowe/settings.local.toml"), "[permissions]\nallow = [\"bash(make check:*)\", \"bash(cargo:*)\"]\n").unwrap();
    let again = perms(&w, PermissionMode::Default);
    assert_eq!(run_with(&again, &w.root, false, &[sh("4", "cargo build")]), ["ask"]);
}

/// Makes `link` a directory link to `target` (a junction on Windows, which needs no rights).
fn dir_link(link: &Path, target: &Path) -> bool {
    #[cfg(windows)]
    {
        let mut cmd = std::process::Command::new("cmd");
        cmd.arg("/c").arg("mklink").arg("/J").arg(link).arg(target);
        let limits = marlowe_os::child::RunLimits { timeout: std::time::Duration::from_secs(20), max_output_bytes: 1 << 16 };
        marlowe_os::child::run(cmd, &marlowe_os::child::ChildSpec::new(), limits, &|| false).is_ok_and(|o| o.code() == 0)
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link).is_ok()
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = (link, target);
        false
    }
}

#[test]
fn a_link_out_of_the_workspace_is_seen_through() {
    let w = ws("link");
    let outside = w._t.0.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("notes.txt"), "keep me").unwrap();
    assert!(dir_link(&w.root.join("esc"), &outside), "could not make a directory link");
    let p = perms(&w, PermissionMode::Bypass);
    let rows: &[(&str, &str)] = &[
        ("rm esc/notes.txt", "ask"),
        ("echo x > esc/new.txt", "ask"),
        ("rm -rf esc", "ask"),
        ("touch inside.txt", "ran"),
        ("ln -s ../outside esc2", "ask"),
    ];
    let calls: Vec<ToolCall> = rows.iter().enumerate().map(|(i, (c, _))| sh(&i.to_string(), c)).collect();
    let got = run_with(&p, &w.root, false, &calls);
    for ((cmd, want), got) in rows.iter().zip(got) {
        assert_eq!(got, *want, "{cmd}");
    }
    assert_eq!(std::fs::read_to_string(outside.join("notes.txt")).unwrap(), "keep me");
}

#[test]
fn parity_m9_the_projects_permission_files_ask_in_every_mode() {
    let w = ws("m9g");
    let rel = |p: &str| w.root.join(p).display().to_string();
    for mode in [PermissionMode::Default, PermissionMode::AcceptEdits, PermissionMode::Bypass] {
        let p = perms(&w, mode);
        let calls = [
            call("1", "edit", json!({"path": rel(".claude/settings.json")})),
            call("2", "edit", json!({"path": rel(".marlowe/settings.local.toml")})),
            call("3", "edit", json!({"path": rel(".claude/hooks/guard.sh")})),
            sh("4", "echo '{}' > .claude/settings.local.json"),
            sh("5", "rm -rf .marlowe"),
            call("6", "edit", json!({"path": rel("src/lib.rs")})),
            call("7", "edit", json!({"path": rel(".claude/commands/review.md")})),
        ];
        let got = run_with(&p, &w.root, false, &calls);
        assert_eq!(got[..5], ["ask"; 5], "{mode:?}: changes the permission settings");
        assert_eq!(got[5..], ["ran", "ran"], "{mode:?}: other edits in the workspace stay free");
    }
    // dont_ask refuses it with the reason; the user may still approve it when asked.
    let p = perms(&w, PermissionMode::DontAsk);
    assert_eq!(run_with(&p, &w.root, false, &[call("1", "edit", json!({"path": rel(".claude/settings.json")}))]), ["refused"]);
}

/// The mixed tools as plan mode sees them: `git` (reads, writes, network) answers Free for a
/// read and asks for a write; `agent` runs.
struct MixedHost {
    ran: Mutex<Vec<String>>,
}

impl ToolHost for MixedHost {
    fn specs(&self) -> Vec<ToolSpec> {
        Vec::new()
    }
    fn call(&self, call: &ToolCall, _ctx: &ToolCtx) -> ToolOutcome {
        self.ran.lock().unwrap().push(format!("{} {}", call.name, call.args));
        ToolOutcome::ok("done", "done")
    }
    fn consequence(&self, call: &ToolCall) -> Consequence {
        match (call.name.as_str(), arg(call, "op")) {
            ("git", "status" | "diff" | "log") => Consequence::Free,
            ("git", op) => Consequence::Ask { summary: format!("git {op}") },
            _ => Consequence::Free,
        }
    }
    fn manifest(&self, name: &str) -> Option<ToolManifest> {
        let (group, effects) = match name {
            "git" => (ToolGroup::Git, Effects { reads: true, writes: true, runs: false, network: true }),
            "agent" => (ToolGroup::Agent, Effects::RUN),
            _ => return None,
        };
        Some(ToolManifest { group, profiles: Profiles::ALL, effects, path_args: &[] })
    }
}

/// Plan mode allows what only reads, decided per call (tracks J and F): `git status` runs and
/// `git commit` is refused; a built-in `explore` agent runs and a `general` one is refused.
#[test]
fn parity_m1_plan_mode_allows_read_only_git_and_read_only_agents() {
    let w = ws("m1-mixed");
    let p = perms(&w, PermissionMode::Plan);
    let host = MixedHost { ran: Mutex::new(Vec::new()) };
    let services = Services::new().with(p);
    let gate = Recording::new(&[]);
    let mut convo = Conversation::new(SessionId("s".into()), Some(w.root.clone()));
    let calls = [
        call("1", "git", json!({"op": "status"})),
        call("2", "git", json!({"op": "commit", "message": "x"})),
        call("3", "agent", json!({"type": "explore", "prompt": "find the parser"})),
        call("4", "agent", json!({"type": "general", "prompt": "fix the parser"})),
    ];
    let r = batch(&host, &[], &services, &*gate, &calls, &mut convo);
    let refused: Vec<bool> = r.iter().map(|x| x.is_error && x.content.contains("plan mode")).collect();
    assert_eq!(refused, [false, true, false, true], "{r:?}");
    assert!(gate.asked().is_empty(), "nothing asked in plan mode: {:?}", gate.asked());
}

/// A project's plan waits for the user's approval of its card, so making one is planning: plan
/// mode lets `manage_project` create, revise and look, and refuses what sends a project on.
/// Found live: dawn entered plan mode for a research request and was refused its project.
#[test]
fn plan_mode_lets_a_project_plan_be_made_but_not_sent_on() {
    let p = perms(&ws("plan-project"), PermissionMode::Plan);
    let reads = |args: Value| p.only_reads(&call("1", "manage_project", args));
    assert!(reads(json!({"action": "create", "plan": {}})));
    assert!(reads(json!({"action": "status", "id": "p_1"})));
    assert!(!reads(json!({"action": "answer", "id": "p_1", "answer": "approve"})), "sending a project on is not planning");
    assert!(!reads(json!({"action": "cancel", "id": "p_1"})));
}
