//! The approval port and the session's permission state (design 2.3 M1-M11, 3.5).
//!
//! - [`ApprovalGate`]: asks the user. The daemon implements it as an ApprovalRequested/Approve
//!   round trip; [`ApprovalGate::ask_with`] carries the rule an "always" answer would add.
//! - [`Permissions`]: one per conductor session, installed by the daemon as a [`Services`]
//!   value. It holds the mode, the layered settings (`marlowe-settings`), this session's
//!   granted rules and PowerShell's parser child, and decides every call
//!   ([`Permissions::decide`]) with `marlowe_permission::rules::policy`. `run_tools.rs` consults
//!   it; without one (the crew, tests) the tool's own consequence decides, as before M4.6.
//! - [`HookVerdicts`]: PreToolUse hooks' `allow`/`ask` per call id, left in the conversation's
//!   extensions by the hook middleware for the decision.
//!
//! [`Services`]: crate::Services

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, RwLock};

use marlowe_model::ToolCall;
use marlowe_permission::rules::policy::{self, Facts, HookPermission, Mode, ToolSays, Verdict};
use marlowe_permission::rules::{self, command_matches, domain_matches, Kind, RuleSet, Subject};
use marlowe_permission::shell::pwsh_ast::{self, AstOutcome, PwshAst};
use marlowe_permission::shell::{self, lexical_join, Analysis, Dialect, ShellCtx};
use marlowe_proto::{ApprovalDecision, PermissionMode, SessionId, SettingsScope};
use marlowe_settings::{Settings, SettingsFile};
use serde_json::Value;

use crate::tools::{Consequence, Exposure, ToolGroup, ToolHost};

/// One approval request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalReq {
    pub session: SessionId,
    pub call_id: String,
    pub tool: String,
    /// One line, from `Consequence::Ask`.
    pub summary: String,
    /// The full arguments, pretty-printed.
    pub detail: String,
}

/// Asks the user. Blocks the calling tool thread; must return `Deny` on timeout or cancel.
pub trait ApprovalGate: Send + Sync {
    fn ask(&self, req: ApprovalReq) -> ApprovalDecision;

    /// [`ApprovalGate::ask`] with the rule(s) `ApprovalDecision::AllowPattern` would add (one per
    /// line; `Event::ApprovalRequested::proposed_rule`). Gates that show it override this.
    fn ask_with(&self, req: ApprovalReq, _proposed_rule: Option<String>) -> ApprovalDecision {
        self.ask(req)
    }
}

/// Allows everything. Tests and headless trusted runs only.
#[derive(Debug, Default, Clone, Copy)]
pub struct AutoApprove;

impl ApprovalGate for AutoApprove {
    fn ask(&self, _req: ApprovalReq) -> ApprovalDecision {
        ApprovalDecision::Allow
    }
}

/// Denies everything that asks.
#[derive(Debug, Default, Clone, Copy)]
pub struct DenyAll;

impl ApprovalGate for DenyAll {
    fn ask(&self, _req: ApprovalReq) -> ApprovalDecision {
        ApprovalDecision::Deny
    }
}

/// PreToolUse hooks' `allow` / `ask` by call id (their `deny` answers the call directly).
#[derive(Debug, Clone, Default)]
pub struct HookVerdicts(pub HashMap<String, HookPermission>);

/// What the permission layer decided about one call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    Free,
    Ask {
        summary: String,
        /// Rules an "always" answer adds (empty: the approval covers this call only).
        proposal: Vec<String>,
        /// Hosts recorded as approved when the user allows it.
        hosts: Vec<String>,
        /// A tripwire or fixed approval (a push, a delete outside the workspace, a new host,
        /// a side effect after untrusted content): only the user answers it, never a hook.
        tripwire: bool,
    },
    Deny(String),
}

/// `marlowe_proto::PermissionMode` as the policy's mode.
pub fn policy_mode(m: PermissionMode) -> Mode {
    match m {
        PermissionMode::Plan => Mode::Plan,
        PermissionMode::Default => Mode::Default,
        PermissionMode::AcceptEdits => Mode::AcceptEdits,
        PermissionMode::Bypass => Mode::Bypass,
        PermissionMode::DontAsk => Mode::DontAsk,
    }
}

/// The settings a session decides with, refreshed by [`Permissions::reload`].
struct Loaded {
    settings: Settings,
    rules: RuleSet,
    project_commands: Vec<String>,
    allow_hosts: Vec<String>,
    roots: Vec<PathBuf>,
    protected: Vec<PathBuf>,
    protected_branches: Vec<String>,
}

struct Inner {
    home: PathBuf,
    /// The user's own home (`USERPROFILE`/`HOME`), for `~/` rules.
    user_home: Option<PathBuf>,
    workspace: Option<PathBuf>,
    /// This session's scratch directory: the one place outside the roots shells write freely
    /// (created on first use, removed at the end when empty).
    scratch: PathBuf,
    scratch_made: std::sync::Once,
    cli: Option<SettingsFile>,
    mode: Mutex<PermissionMode>,
    /// The mode before `plan`, restored by `exit_plan`.
    before_plan: Mutex<Option<PermissionMode>>,
    loaded: RwLock<Loaded>,
    /// Rules granted for this session ("allow for this session").
    session: Mutex<RuleSet>,
    /// Extra roots for this session (`/add-dir`, `--add-dir`).
    extra_roots: Mutex<Vec<PathBuf>>,
    pwsh: Option<Arc<dyn PwshAst>>,
}

/// One session's permission state. Cheap to clone; clones share the state.
#[derive(Clone)]
pub struct Permissions {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Permissions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Permissions").field("mode", &self.mode()).finish_non_exhaustive()
    }
}

impl Permissions {
    /// Loads the settings for `workspace` from `home` (plus `cli`, the highest layer), starts in
    /// the settings' default mode, and asks Windows PowerShell's parser on Windows.
    pub fn new(home: &Path, workspace: Option<&Path>, cli: Option<SettingsFile>) -> Self {
        Self::with_parser(home, workspace, cli, default_pwsh())
    }

    /// [`Permissions::new`] with PowerShell's parser supplied (a fake in tests; `None` skips the
    /// cross-check).
    pub fn with_parser(home: &Path, workspace: Option<&Path>, cli: Option<SettingsFile>, pwsh: Option<Arc<dyn PwshAst>>) -> Self {
        let loaded = load(home, workspace, cli.clone());
        let mode = loaded.settings.default_mode().unwrap_or_default();
        Permissions {
            inner: Arc::new(Inner {
                home: home.to_path_buf(),
                user_home: std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from),
                workspace: workspace.map(Path::to_path_buf),
                scratch: default_scratch(),
                scratch_made: std::sync::Once::new(),
                cli,
                mode: Mutex::new(mode),
                before_plan: Mutex::new(None),
                loaded: RwLock::new(loaded),
                session: Mutex::new(RuleSet::new()),
                extra_roots: Mutex::new(Vec::new()),
                pwsh,
            }),
        }
    }

    /// This session's scratch directory instead of the default one (the daemon passes
    /// `<temp>\marlowe\<session>`). Call before the value is shared.
    pub fn with_scratch(mut self, dir: PathBuf) -> Self {
        if let Some(inner) = Arc::get_mut(&mut self.inner) {
            inner.scratch = dir;
        }
        self
    }

    /// Where shells may write outside the roots without asking (not the whole temp dir).
    pub fn scratch_dir(&self) -> &Path {
        &self.inner.scratch
    }

    pub fn mode(&self) -> PermissionMode {
        *lock(&self.inner.mode)
    }

    /// Sets the mode. Entering `plan` remembers the mode to return to; any other change forgets
    /// it.
    pub fn set_mode(&self, mode: PermissionMode) {
        let mut m = lock(&self.inner.mode);
        let mut before = lock(&self.inner.before_plan);
        if mode == PermissionMode::Plan {
            if *m != PermissionMode::Plan {
                *before = Some(*m);
            }
        } else {
            *before = None;
        }
        *m = mode;
    }

    /// Leaves plan mode for the mode it was entered from (the `exit_plan` tool, once approved).
    /// Outside plan mode it changes nothing.
    pub fn exit_plan(&self) -> PermissionMode {
        let mut m = lock(&self.inner.mode);
        if *m != PermissionMode::Plan {
            return *m;
        }
        let back = lock(&self.inner.before_plan).take().unwrap_or_default();
        *m = back;
        back
    }

    /// Re-reads the settings files (after an edit, a trust, `/permissions`).
    pub fn reload(&self) {
        let fresh = load(&self.inner.home, self.inner.workspace.as_deref(), self.inner.cli.clone());
        *self.inner.loaded.write().unwrap_or_else(|p| p.into_inner()) = fresh;
    }

    /// The settings as loaded.
    pub fn settings(&self) -> Settings {
        self.inner.loaded.read().unwrap_or_else(|p| p.into_inner()).settings.clone()
    }

    pub fn workspace(&self) -> Option<&Path> {
        self.inner.workspace.as_deref()
    }

    /// Rules granted for this session, as written.
    pub fn session_rules(&self) -> Vec<String> {
        lock(&self.inner.session).iter().map(|(_, r)| r.text.clone()).collect()
    }

    /// Another root for this session (writes inside it are free).
    pub fn add_root(&self, dir: PathBuf) {
        let mut roots = lock(&self.inner.extra_roots);
        if !roots.contains(&dir) {
            roots.push(dir);
        }
    }

    /// Every root: the workspace, the settings' additional directories, this session's extras.
    pub fn roots(&self) -> Vec<PathBuf> {
        let mut out = self.inner.loaded.read().unwrap_or_else(|p| p.into_inner()).roots.clone();
        out.extend(lock(&self.inner.extra_roots).iter().cloned());
        out
    }

    /// Grants `rules` ("don't ask again") at `scope`: for this session, or written to the
    /// settings file of that scope (`Local` is "always for this project") and recorded as the
    /// user's own grant, so it applies after a restart even in a project whose other settings
    /// are untrusted ([`Settings::grant`]).
    pub fn grant(&self, rules: &[String], scope: SettingsScope) -> Result<(), String> {
        {
            let mut s = lock(&self.inner.session);
            for r in rules {
                s.add_text(rules::Action::Allow, r).map_err(|e| e.to_string())?;
            }
        }
        if scope == SettingsScope::Session {
            return Ok(());
        }
        Settings::grant(&self.inner.home, self.inner.workspace.as_deref(), scope, marlowe_proto::RuleAction::Allow, rules)
            .map_err(|e| e.to_string())?;
        self.reload();
        Ok(())
    }

    /// Whether every rule of `proposal` was granted this session (so a later call in the same
    /// batch need not ask again).
    pub fn granted(&self, proposal: &[String]) -> bool {
        let s = lock(&self.inner.session);
        !proposal.is_empty() && proposal.iter().all(|p| s.contains(rules::Action::Allow, p))
    }

    /// Decides `call` (after middleware rewrote it); see `marlowe_permission::rules::policy`.
    pub fn decide(&self, host: &dyn ToolHost, call: &ToolCall, exposure: &Exposure, hook: Option<HookPermission>) -> Gate {
        let loaded = self.inner.loaded.read().unwrap_or_else(|p| p.into_inner());
        let mut roots = loaded.roots.clone();
        roots.extend(lock(&self.inner.extra_roots).iter().cloned());
        let root = self.inner.workspace.clone().or_else(|| roots.first().cloned());
        let manifest = host.manifest(&call.name);
        let is_mcp = call.name.starts_with("mcp__") || manifest.as_ref().is_some_and(|m| m.group == ToolGroup::Mcp);
        // A fixed ask (`AskEvery`) is a tripwire: no rule, hook, root or mode lifts it.
        let mut fixed: Option<String> = None;
        let says = match host.consequence_exposed(call, exposure) {
            Consequence::Free => ToolSays::Free,
            Consequence::Ask { summary } => ToolSays::Ask(summary),
            Consequence::AskEvery { summary } => {
                fixed = Some(summary.clone());
                ToolSays::Ask(summary)
            }
        };
        // A tool with no manifest is judged by its own answer: one that asks is treated as
        // running something (plan mode refuses it), one that does not as a read.
        let effects = manifest.as_ref().map(|m| m.effects).unwrap_or(if matches!(says, ToolSays::Ask(_)) {
            crate::tools::Effects::RUN
        } else {
            crate::tools::Effects::READ
        });

        // Shell lines are analysed in their own dialect from their working directory.
        let dialect = Dialect::of_tool(&call.name);
        let command = call.args.get("command").and_then(Value::as_str).unwrap_or("");
        let scratch = [self.inner.scratch.clone()];
        let guarded = self.inner.workspace.as_deref().map(guarded_paths).unwrap_or_default();
        if dialect.is_some() {
            let dir = self.inner.scratch.clone();
            self.inner.scratch_made.call_once(move || {
                let _ = std::fs::create_dir_all(dir);
            });
        }
        let analysis: Option<Analysis> = dialect.map(|d| {
            let cwd = match (call.args.get("cwd").and_then(Value::as_str), root.as_deref()) {
                (Some(c), Some(r)) => lexical_join(r, Path::new(c)),
                (Some(c), None) => PathBuf::from(c),
                (None, Some(r)) => r.to_path_buf(),
                (None, None) => std::env::current_dir().unwrap_or_default(),
            };
            let ctx = ShellCtx {
                cwd,
                scope: shell::Scope::Workspace { roots: &roots },
                protected: &loaded.protected,
                protected_branches: &loaded.protected_branches,
                scratch: &scratch,
                guarded: &guarded,
                read_script: &shell::read_script_file,
            };
            shell::analyze(d, command, &ctx)
        });

        let code = if call.name == "python" { call.args.get("code").and_then(Value::as_str) } else { None };
        let kind = if analysis.is_some() {
            Kind::Shell
        } else if is_mcp {
            Kind::Mcp
        } else if manifest.as_ref().is_some_and(|m| !m.path_args.is_empty() && effects.writes && !effects.runs) {
            Kind::FileWrite
        } else if manifest.as_ref().is_some_and(|m| !m.path_args.is_empty() && !effects.writes) {
            Kind::FileRead
        } else if host.egress_host(call).is_some() || manifest.as_ref().is_some_and(|m| m.group == ToolGroup::Web) {
            Kind::Web
        } else {
            Kind::Other
        };
        let mut subject = Subject::new(&call.name, kind);
        subject.shell = analysis.as_ref();
        subject.code = code;
        subject.root = root.as_deref();
        subject.home = self.inner.user_home.as_deref();
        subject.domain = host.egress_host(call).or_else(|| web_host(call));
        if let Some(m) = &manifest {
            for a in m.path_args {
                if let Some(p) = call.args.get(*a).and_then(Value::as_str).filter(|p| !p.trim().is_empty()) {
                    subject.paths.push(match root.as_deref() {
                        Some(r) => lexical_join(r, Path::new(p)),
                        None => PathBuf::from(p),
                    });
                }
            }
        }
        let mut rules = loaded.rules.clone();
        rules.extend(&lock(&self.inner.session));
        let rule = rules.evaluate(&subject);

        // The tool's own Ask, lifted where settings already answer it: a web call to a host the
        // settings allow (`network.allow_hosts`, a `domain:` allow rule), a file call whose every
        // path is inside a root (`additional_directories`, `/add-dir`) and not protected.
        let says = match (&says, kind) {
            _ if fixed.is_some() => says,
            (ToolSays::Ask(_), Kind::Web)
                if subject.domain.as_deref().is_some_and(|h| {
                    loaded.allow_hosts.iter().any(|d| domain_matches(d, h))
                        || rule.as_ref().is_some_and(|m| {
                            m.action == rules::Action::Allow
                                && rules::Rule::parse(&m.rule).is_ok_and(|r| matches!(r.spec, rules::Spec::Domain(_)))
                        })
                }) =>
            {
                ToolSays::Free
            }
            (ToolSays::Ask(_), Kind::FileWrite | Kind::FileRead)
                if !subject.paths.is_empty()
                    && subject.paths.iter().all(|p| {
                        roots.iter().any(|r| shell::within(r, p)) && !loaded.protected.iter().any(|x| shell::within(x, p))
                    }) =>
            {
                ToolSays::Free
            }
            _ => says,
        };

        let new_hosts: Vec<String> = analysis
            .as_ref()
            .map(|a| {
                a.hosts
                    .iter()
                    .filter(|h| !exposure.approved_hosts.contains(*h) && !loaded.allow_hosts.iter().any(|d| domain_matches(d, h)))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        let project_command = analysis.as_ref().is_some_and(|a| {
            let cmds: Vec<_> = a.rule_commands().collect();
            !cmds.is_empty()
                && cmds.iter().all(|c| {
                    c.dangerous.is_none()
                        && !c.writes_redirect
                        && loaded.project_commands.iter().any(|p| {
                            project_pattern(p).is_some_and(|pat| command_matches(&pat, c.typed_line.as_deref().unwrap_or(&c.line)))
                        })
                })
        });
        let extra_ask = fixed.or_else(|| code.and_then(|c| match shell::classify_code(c) {
            shell::Verdict::Ask(why) => Some(why),
            _ => None,
        }));
        // The workspace's own permission files and hook scripts: editing them could drop its
        // deny rules or its guard hooks, so a file tool asks in every mode (the shell analysis
        // asks the same through `guarded`).
        let extra_ask = extra_ask.or_else(|| {
            (kind == Kind::FileWrite)
                .then(|| subject.paths.iter().find(|p| guarded.iter().any(|g| shell::within(g, p))))
                .flatten()
                .map(|p| format!("changes the permission settings ({})", p.display()))
        });

        let mut facts = Facts::new(policy_mode(self.mode()), says.clone());
        facts.writes = effects.writes;
        facts.runs = effects.runs;
        facts.mcp = is_mcp;
        facts.shell = analysis.as_ref();
        facts.tainted = exposure.tainted;
        facts.new_hosts = new_hosts;
        facts.extra_ask = extra_ask;
        facts.rule = rule;
        facts.project_command = project_command;
        facts.hook = hook;
        let mut verdict = policy::decide(&facts);

        // A PowerShell line about to run unasked: PowerShell's own parser must read it as
        // Marlowe did.
        if verdict == Verdict::Free && dialect == Some(Dialect::PowerShell) {
            if let (Some(p), Some(a)) = (&self.inner.pwsh, analysis.as_ref()) {
                if let Some(why) = pwsh_ast::cross_check(a.literal.as_ref(), &p.parse(command)) {
                    facts.extra_ask = Some(why.to_string());
                    verdict = policy::decide(&facts);
                }
            }
        }

        match verdict {
            Verdict::Free => Gate::Free,
            Verdict::Deny(why) => Gate::Deny(why),
            Verdict::Ask { reason, tripwire } => {
                let base = match &says {
                    ToolSays::Ask(s) => s.clone(),
                    ToolSays::Free => host.describe(call),
                };
                let summary = if base.contains(&reason) { base } else { format!("{base} ({reason})") };
                let proposal = if tripwire { Vec::new() } else { rules::propose(&subject) };
                Gate::Ask { summary, proposal, hosts: analysis.as_ref().map(|a| a.hosts.clone()).unwrap_or_default(), tripwire }
            }
        }
    }
}

/// `bash(just test:*)` -> `just test:*`: the command pattern of a project-command rule.
fn project_pattern(rule: &str) -> Option<String> {
    match rules::Rule::parse(rule).ok()?.spec {
        rules::Spec::Command(p) => Some(p),
        _ => None,
    }
}

/// The host of a `url` argument (web tools without an egress declaration).
fn web_host(call: &ToolCall) -> Option<String> {
    call.args.get("url").and_then(Value::as_str).and_then(shell::host_of_url)
}

fn load(home: &Path, workspace: Option<&Path>, cli: Option<SettingsFile>) -> Loaded {
    let settings = Settings::load(home, workspace, cli);
    let mut roots: Vec<PathBuf> = workspace.map(Path::to_path_buf).into_iter().collect();
    roots.extend(settings.additional_directories());
    let mut protected = settings.protected_paths();
    protected.extend(default_protected());
    Loaded {
        rules: settings.rule_set(),
        project_commands: settings.project_commands(),
        allow_hosts: settings.allow_hosts(),
        protected_branches: settings.protected_branches(),
        roots,
        protected,
        settings,
    }
}

/// The files that hold `workspace`'s permission settings, and its hook script directories.
pub fn guarded_paths(workspace: &Path) -> Vec<PathBuf> {
    let (claude, marlowe) = (workspace.join(".claude"), workspace.join(".marlowe"));
    vec![
        claude.join("settings.json"),
        claude.join("settings.local.json"),
        claude.join("hooks"),
        marlowe.join("settings.toml"),
        marlowe.join("settings.local.toml"),
        marlowe.join("hooks"),
    ]
}

/// Paths no command may write (CLAUDE.md non-negotiable 6): the old harness and its profile.
fn default_protected() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        out.push(PathBuf::from(local).join("marlowe").join("default-profile"));
    }
    if let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
        out.push(PathBuf::from(home).join("Projects").join("Marlowe_Harness"));
    }
    out
}

impl Drop for Inner {
    fn drop(&mut self) {
        // Only an empty scratch dir is removed; anything left in it is the user's to keep.
        let _ = std::fs::remove_dir(&self.scratch);
    }
}

/// `<temp>\marlowe\scratch-<pid>-<n>`: a session's own scratch when the daemon names none.
fn default_scratch() -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join("marlowe").join(format!("scratch-{}-{n}", std::process::id()))
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

// ---- PowerShell's parser as a long-lived child

/// PowerShell's AST parser in one long-lived child (started lazily through `marlowe_os::child`,
/// restarted once if it dies), speaking `marlowe_permission::shell::pwsh_ast`'s line protocol.
pub struct PwshAstChild {
    exe: PathBuf,
    proc: Mutex<Option<AstProc>>,
}

struct AstProc {
    child: marlowe_os::child::ChildProc,
    stdin: std::process::ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
    next_id: u64,
}

impl Drop for AstProc {
    fn drop(&mut self) {
        self.child.kill_tree();
    }
}

impl PwshAstChild {
    pub fn new(exe: impl Into<PathBuf>) -> Self {
        PwshAstChild { exe: exe.into(), proc: Mutex::new(None) }
    }

    fn start(&self) -> Result<AstProc, String> {
        let mut cmd = Command::new(&self.exe);
        cmd.args(["-NoLogo", "-NoProfile", "-NonInteractive", "-EncodedCommand"])
            .arg(pwsh_ast::encoded_script())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = marlowe_os::child::spawn(&mut cmd, &marlowe_os::child::ChildSpec::new()).map_err(|e| e.to_string())?;
        let stdin = child.child_mut().stdin.take().ok_or("no stdin")?;
        let stdout = BufReader::new(child.child_mut().stdout.take().ok_or("no stdout")?);
        Ok(AstProc { child, stdin, stdout, next_id: 0 })
    }

    fn ask(p: &mut AstProc, script: &str) -> Result<AstOutcome, String> {
        let id = p.next_id;
        p.next_id += 1;
        p.stdin.write_all(pwsh_ast::Request::new(id, script).line().as_bytes()).map_err(|e| e.to_string())?;
        p.stdin.flush().map_err(|e| e.to_string())?;
        let mut line = String::new();
        if p.stdout.read_line(&mut line).map_err(|e| e.to_string())? == 0 {
            return Err("the parser closed its output".into());
        }
        pwsh_ast::read_response(&line, id)
    }
}

impl PwshAst for PwshAstChild {
    fn parse(&self, script: &str) -> AstOutcome {
        let mut slot = lock(&self.proc);
        for _ in 0..2 {
            if slot.is_none() {
                match self.start() {
                    Ok(p) => *slot = Some(p),
                    Err(_) => return AstOutcome::Failed,
                }
            }
            if let Some(p) = slot.as_mut() {
                match Self::ask(p, script) {
                    Ok(o) => return o,
                    Err(_) => *slot = None,
                }
            }
        }
        AstOutcome::Failed
    }
}

/// PowerShell's parser as [`Permissions::with_parser`] takes it.
pub type PwshParser = Arc<dyn PwshAst>;

/// Windows PowerShell's parser on Windows; none elsewhere (the `powershell` tool is Windows').
/// The daemon makes one and shares it between sessions, so one `powershell.exe` serves them all.
pub fn default_pwsh() -> Option<PwshParser> {
    if cfg!(windows) {
        Some(Arc::new(PwshAstChild::new("powershell.exe")))
    } else {
        None
    }
}

#[cfg(test)]
pub(crate) mod tests;
