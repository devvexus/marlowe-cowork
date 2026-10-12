//! The loop of a headless run: one daemon connection, one session, turns in, events out.
//!
//! Nothing here blocks forever. Every wait has an owner: the daemon's events, the SDK client's
//! stdin, the permission prompt tool (bounded by the MCP call timeout), or `--idle-timeout`.

use std::collections::HashMap;
use std::io::BufRead;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, unbounded, Receiver, RecvTimeoutError};
use marlowe_brains::{env_var, CredentialStore, KeyringStore};
use marlowe_model::{Effort, ModelRef, Pricing, ProviderKind, Usage};
use marlowe_proto::{
    ApprovalDecision, ApprovalId, DaemonInfo, Envelope, Event, HeadlessOptions, PermissionMode, PlanFileDecision, Question, Request,
    SessionId, SessionSummary, SettingsScope, Slot, TurnStop,
};
use serde_json::{json, Value};

use super::args::{Args, InputFormat, Resume};
use super::input::{self, Input};
use super::output::{Emitter, Init, TurnResult};
use super::policy::{cc_name, Policy, Verdict};
use super::prompt_tool::{self, Decision, PromptTool};
use super::{settings, worktree};
use crate::client::{Client, ClientError};

/// How often the loop looks at stdin, the interrupt flag and the clock between daemon events.
const TICK: Duration = Duration::from_millis(25);
/// How long a preamble request may take.
const OP_TIMEOUT: Duration = Duration::from_secs(30);
/// After a steer, how long to wait for the daemon to start a turn of it before ending.
const SETTLE: Duration = Duration::from_millis(400);
/// The text a question gets when nobody can answer it.
const NO_ANSWER: &str = "(no answer: running non-interactively)";
/// How long a prompt argument waits for piped data that never comes (a parent that left stdin
/// open) before the run goes on without it.
pub const STDIN_GRACE: Duration = Duration::from_secs(3);
/// Whether the shipped daemon applies [`HeadlessOptions`] to a turn: `disallowed_tools` become
/// session deny rules (a matching call is refused even when it needs no approval),
/// `allowed_tools` session allow rules, and `append_system_prompt` ends the stable prompt. It
/// does (`marlowe-daemon` `session::run_one`); a test's fake daemon that does not sets it false.
pub const DAEMON_APPLIES_OPTIONS: bool = true;

pub const EXIT_OK: i32 = 0;
pub const EXIT_ERROR: i32 = 1;
pub const EXIT_APPROVAL: i32 = 2;
pub const EXIT_BUDGET: i32 = 3;
pub const EXIT_CANCELLED: i32 = 130;

/// Starts or finds the daemon; the returned closure stops what this call started.
pub type Connector = Box<dyn FnOnce() -> Result<(DaemonInfo, Box<dyn FnOnce()>), String>>;

/// The process's edges, injected so a test can stand in for them.
pub struct Env {
    pub home: std::path::PathBuf,
    pub cwd: std::path::PathBuf,
    /// Piped stdin, or `None` for a terminal.
    pub stdin: Option<Box<dyn BufRead + Send>>,
    pub out: Box<dyn std::io::Write + Send>,
    pub err: Box<dyn std::io::Write + Send>,
    /// Set by Ctrl-C.
    pub interrupt: Arc<AtomicBool>,
    /// [`DAEMON_APPLIES_OPTIONS`], injected so a test can stand in for a daemon that does.
    pub daemon_applies_options: bool,
    /// [`STDIN_GRACE`].
    pub stdin_grace: Duration,
}

enum StdinMsg {
    Line(String),
    Eof,
}

enum Pending {
    Approval { approval: ApprovalId, tool: String, input: Value },
    Question { id: String, count: usize },
    /// `exit_plan` waits for `AnswerPlan` (the plan file is written; the client may have edited it).
    Plan { id: String },
}

#[derive(Default)]
struct Turn {
    started: Option<Instant>,
    text: String,
    /// Where the current model call's text begins in `text` (a retraction cuts back to it).
    call_start: usize,
    steps: u32,
    tool_started: HashMap<String, Instant>,
    /// Tool names by call id, for denials the daemon decided without asking.
    tool_names: HashMap<String, String>,
    tool_time: Duration,
    denials: Vec<Value>,
    error: Option<String>,
    approval_needed: Option<String>,
    step_limit: bool,
    steers: u32,
}

/// Where the provider's API key would come from, never the key: `credential_manager` when one is
/// stored, `env` for the provider's variable (read once at startup, scrubbed from every child by
/// `marlowe_os::child`), else `none`.
pub fn key_source(kind: ProviderKind, store: &dyn CredentialStore, env: &dyn Fn(&str) -> Option<String>) -> &'static str {
    if store.get(kind).ok().flatten().is_some_and(|k| !k.trim().is_empty()) {
        return "credential_manager";
    }
    match env_var(kind).and_then(env) {
        Some(v) if !v.trim().is_empty() => "env",
        _ => "none",
    }
}

struct Library {
    model: Option<ModelRef>,
    display: String,
    pricing: Option<Pricing>,
    has_model: Vec<ModelRef>,
    effort: Effort,
}

pub struct Driver {
    args: Args,
    client: Client,
    em: Emitter,
    session: SessionId,
    policy: Policy,
    mode: PermissionMode,
    options: Option<HeadlessOptions>,
    prompt_tool: Option<PromptTool>,
    lib: Library,
    interrupt: Arc<AtomicBool>,
    stdin: Option<Receiver<StdinMsg>>,
    stdin_done: bool,
    // run state
    turn: Option<Turn>,
    ask_pending: bool,
    settle_until: Option<Instant>,
    pending: HashMap<String, Pending>,
    next_req: u64,
    cancelled: bool,
    interrupt_sent: bool,
    fatal: Option<String>,
    results: u32,
    exit: i32,
    spent_usd: f64,
    last_activity: Instant,
    /// Events that arrived while a request was waiting for its answer, handled by the loop first.
    backlog: Vec<Envelope>,
    /// Run-only MCP servers this driver added, removed again when it ends.
    mcp_added: Vec<String>,
    workspace: Option<String>,
}

fn lib_from(events: &Event) -> Option<(Vec<marlowe_proto::LibraryModel>, Vec<marlowe_proto::SlotAssignment>)> {
    match events {
        Event::LibraryState { models, slots, .. } => Some((models.clone(), slots.clone())),
        _ => None,
    }
}

fn normalise(p: &str) -> String {
    p.replace('/', "\\").trim_end_matches('\\').to_ascii_lowercase()
}

/// The mode a plan approval asked for: `accept_edits` (either spelling), else `default`.
fn plan_mode(asked: Option<&str>) -> PermissionMode {
    match asked.map(|m| m.trim().to_ascii_lowercase().replace('_', "")).as_deref() {
        Some("acceptedits") => PermissionMode::AcceptEdits,
        _ => PermissionMode::Default,
    }
}

/// One `ask_user` question as the control request carries it: Claude Code's fields, plus the
/// options' descriptions and previews (`option_details`) when there are any (T33).
fn question_json(q: &Question) -> Value {
    let mut v = json!({"question": q.question, "header": q.header, "options": q.options, "multiSelect": q.multi_select});
    if !q.option_details.is_empty() {
        v["option_details"] = json!(q.option_details);
    }
    v
}

fn spawn_stdin(reader: Box<dyn BufRead + Send>) -> Receiver<StdinMsg> {
    let (tx, rx) = unbounded();
    let _ = std::thread::Builder::new().name("marlowe-stdin".into()).spawn(move || {
        for line in reader.lines() {
            match line {
                Ok(l) => {
                    if tx.send(StdinMsg::Line(l)).is_err() {
                        return;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = tx.send(StdinMsg::Eof);
    });
    rx
}

enum Chunk {
    Data(Vec<u8>),
    Eof,
    Failed(String),
}

/// Piped stdin read on a thread, so that a pipe nobody closes cannot hold the run. With a prompt
/// argument, stdin that sends nothing for `grace` is given up on (a warning, then the argument
/// alone: a parent that spawned us with an open, silent pipe is the usual cause); once data has
/// started, it is waited for as long as `idle` between pieces. Without a prompt the data is the
/// prompt, so the first byte is waited for as long as `idle` as well.
fn read_piped(reader: Box<dyn BufRead + Send>, have_prompt: bool, grace: Duration, idle: Duration, em: &mut Emitter) -> Result<String, String> {
    let (tx, rx) = bounded::<Chunk>(4);
    let spawned = std::thread::Builder::new().name("marlowe-stdin-prompt".into()).spawn(move || {
        let mut r = reader;
        loop {
            let chunk = match r.fill_buf() {
                Ok([]) => Chunk::Eof,
                Ok(b) => {
                    let v = b.to_vec();
                    let n = v.len();
                    r.consume(n);
                    Chunk::Data(v)
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => Chunk::Failed(e.to_string()),
            };
            let last = !matches!(chunk, Chunk::Data(_));
            if tx.send(chunk).is_err() || last {
                return;
            }
        }
    });
    if spawned.is_err() {
        return Err("cannot start a thread to read stdin".into());
    }
    let mut bytes = Vec::new();
    let mut wait = if have_prompt { grace } else { idle };
    loop {
        match rx.recv_timeout(wait) {
            Ok(Chunk::Data(d)) => {
                bytes.extend_from_slice(&d);
                wait = idle;
            }
            Ok(Chunk::Eof) => break,
            Ok(Chunk::Failed(e)) => return Err(format!("cannot read stdin: {e}")),
            Err(RecvTimeoutError::Timeout) if bytes.is_empty() && have_prompt => {
                em.warn(&format!("no data on stdin after {} s; going on with the prompt argument alone", grace.as_secs_f32().max(0.1)));
                break;
            }
            Err(RecvTimeoutError::Timeout) => return Err(format!("stdin went quiet for {} s without ending (--idle-timeout)", idle.as_secs())),
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// The prompt: the argument, then piped text. A terminal has no text to add.
fn read_prompt(args: &Args, stdin: &mut Option<Box<dyn BufRead + Send>>, grace: Duration, em: &mut Emitter) -> Result<Option<String>, String> {
    if args.input == InputFormat::StreamJson {
        return Ok(args.prompt.clone());
    }
    let mut piped = String::new();
    if let Some(r) = stdin.take() {
        piped = read_piped(r, args.prompt.is_some(), grace, args.idle_timeout, em)?;
    }
    let piped = piped.trim_end().to_string();
    match (&args.prompt, piped.is_empty()) {
        (Some(p), true) => Ok(Some(p.clone())),
        (Some(p), false) => Ok(Some(format!("{p}\n\n{piped}"))),
        (None, false) => Ok(Some(piped)),
        (None, true) => Err("nothing to do: give a prompt (marlowe -p \"...\") or pipe one on stdin".into()),
    }
}

/// Runs one headless invocation to its exit code.
pub fn run(args: Args, mut env: Env, connect: Connector) -> i32 {
    let mut em = Emitter::new(args.output, args.partial, args.verbose, env.out, env.err);

    let prompt = match read_prompt(&args, &mut env.stdin, env.stdin_grace, &mut em) {
        Ok(p) => p,
        Err(e) => {
            em.failure(&e, "", EXIT_ERROR);
            return EXIT_ERROR;
        }
    };
    let rs = match settings::load(&args.settings, &env.home) {
        Ok(s) => s,
        Err(e) => {
            em.failure(&e, "", EXIT_ERROR);
            return EXIT_ERROR;
        }
    };
    let mcp = match settings::mcp_entries(&args.mcp_config, &env.home) {
        Ok(m) => m,
        Err(e) => {
            em.failure(&e, "", EXIT_ERROR);
            return EXIT_ERROR;
        }
    };
    for (name, entry) in &mcp {
        match marlowe_mcp::config::parse_entry(name, marlowe_mcp::config::Scope::Session, entry) {
            Ok(cfg) => cfg.ignored.iter().for_each(|w| em.warn(&format!("--mcp-config: {w}"))),
            Err(e) => {
                em.failure(&format!("--mcp-config: {e}"), "", EXIT_ERROR);
                return EXIT_ERROR;
            }
        }
    }
    // Options only the daemon can keep. A denied tool that needs no approval (a write inside the
    // workspace, a read, a safe command) never reaches this process's approval check, and an
    // appended system prompt is the daemon's to append, so without a daemon that does both the
    // flags would be accepted and do nothing. Better a clear exit 1 than a quiet hole.
    if !env.daemon_applies_options {
        let wanted: Vec<&str> = [
            (!args.disallowed.is_empty() || !rs.deny.is_empty()).then_some("--disallowed-tools (or deny rules in --settings)"),
            args.append_system_prompt.is_some().then_some("--append-system-prompt"),
        ]
        .into_iter()
        .flatten()
        .collect();
        if !wanted.is_empty() {
            em.failure(
                &format!(
                    "{} cannot be honoured yet: this daemon applies neither per-run tool filters nor an appended system prompt, so a denied tool that needs no approval would still run. \
                     Not running; remove the flag(s) or use --permission-mode plan / --allowed-tools to narrow the run",
                    wanted.join(" and ")
                ),
                "",
                EXIT_ERROR,
            );
            return EXIT_ERROR;
        }
    }

    // The workspace: the flag, else here unless here is too broad to be one; -w moves into a worktree.
    let explicit = args.workspace.is_some();
    let mut workspace = args.workspace.as_ref().map(std::path::PathBuf::from).unwrap_or_else(|| env.cwd.clone());
    if let Some(name) = &args.worktree {
        match worktree::ensure(&workspace, name) {
            Ok(w) => workspace = w,
            Err(e) => {
                em.failure(&e, "", EXIT_ERROR);
                return EXIT_ERROR;
            }
        }
    } else if !explicit && marlowe_exec::paths::is_broad_root(&workspace) {
        em.progress("the current directory is too broad to be a workspace; running without one");
    }
    let workspace_s = (explicit || args.worktree.is_some() || !marlowe_exec::paths::is_broad_root(&workspace))
        .then(|| crate::strip_verbatim(std::fs::canonicalize(&workspace).unwrap_or(workspace.clone())).display().to_string());

    let (info, stop) = match connect() {
        Ok(c) => c,
        Err(e) => {
            em.failure(&e, "", EXIT_ERROR);
            return EXIT_ERROR;
        }
    };
    let code = match Client::connect(&info, "marlowe-print") {
        Ok(client) => {
            let allow: Vec<String> = args.allowed.iter().chain(&rs.allow).cloned().collect();
            let deny: Vec<String> = args.disallowed.iter().chain(&rs.deny).cloned().collect();
            let mode = args.mode.or(rs.mode).unwrap_or_default();
            let options = options_of(&args, &allow, &deny, mode);
            let policy = Policy::new(&allow, &deny, workspace_s.as_ref().map(std::path::PathBuf::from));
            let interrupt = env.interrupt.clone();
            let stdin = env.stdin.take().filter(|_| args.input == InputFormat::StreamJson).map(spawn_stdin);
            let mut d = Driver {
                args,
                client,
                em,
                session: SessionId(String::new()),
                policy,
                mode,
                options,
                prompt_tool: None,
                lib: Library { model: None, display: String::new(), pricing: None, has_model: Vec::new(), effort: Effort::default() },
                interrupt,
                stdin,
                stdin_done: false,
                turn: None,
                ask_pending: false,
                settle_until: None,
                pending: HashMap::new(),
                next_req: 0,
                cancelled: false,
                interrupt_sent: false,
                fatal: None,
                results: 0,
                exit: EXIT_OK,
                spent_usd: 0.0,
                last_activity: Instant::now(),
                backlog: Vec::new(),
                mcp_added: Vec::new(),
                workspace: workspace_s.clone(),
            };
            let code = d.go(&env.home, workspace_s.as_deref(), prompt, rs, mcp);
            d.remove_run_servers();
            code
        }
        Err(e) => {
            em.failure(&format!("cannot connect to the daemon: {e}"), "", EXIT_ERROR);
            EXIT_ERROR
        }
    };
    stop();
    code
}

fn options_of(args: &Args, allow: &[String], deny: &[String], mode: PermissionMode) -> Option<HeadlessOptions> {
    let o = HeadlessOptions {
        allowed_tools: allow.to_vec(),
        disallowed_tools: deny.to_vec(),
        max_steps: args.max_steps,
        max_budget_usd: args.max_budget_usd,
        append_system_prompt: args.append_system_prompt.clone(),
        permission_mode: (mode != PermissionMode::Default || args.mode.is_some()).then_some(mode),
        print: true,
    };
    (o != HeadlessOptions::default()).then_some(o)
}

impl Driver {
    fn go(&mut self, home: &std::path::Path, workspace: Option<&str>, prompt: Option<String>, rs: settings::RunSettings, mcp: Vec<(String, Value)>) -> i32 {
        match self.setup(home, workspace, &rs, &mcp) {
            Ok(()) => {}
            Err(e) => {
                self.em.failure(&e, &self.session.0.clone(), EXIT_ERROR);
                return EXIT_ERROR;
            }
        }
        if let Some(p) = prompt {
            self.send_user(&p);
        } else if self.stdin.is_none() {
            self.em.failure("nothing to do: no prompt", &self.session.0.clone(), EXIT_ERROR);
            return EXIT_ERROR;
        }
        self.drive()
    }

    // ------------------------------------------------------------ setup

    fn send(&mut self, req: &Request) -> Result<(), String> {
        self.client.send(req).map_err(|e| e.to_string())
    }

    /// Sends a request whose success has no reply of its own and returns the `Error` it drew, if
    /// any: a `Status` round trip marks the end of what the daemon answers at once.
    fn configure(&mut self, req: Request) -> Result<(), String> {
        self.send(&req)?;
        self.send(&Request::Status)?;
        let end = Instant::now() + Duration::from_secs(5);
        let mut err = None;
        while Instant::now() < end {
            match self.client.next_event(Duration::from_millis(100)) {
                Ok(Some(Envelope { event: Event::Status(_), .. })) => break,
                Ok(Some(Envelope { event: Event::Error { message, .. }, .. })) => err = Some(message),
                // A turn may be running (a mode switch mid-run): its events are the loop's.
                Ok(Some(other)) => self.backlog.push(other),
                Ok(None) => {}
                Err(e) => return Err(e.to_string()),
            }
        }
        // Replies from worker threads can land just after the barrier.
        let grace = Instant::now() + Duration::from_millis(60);
        while Instant::now() < grace {
            match self.client.next_event(Duration::from_millis(20)) {
                Ok(Some(Envelope { event: Event::Error { message, .. }, .. })) => err = Some(message),
                Ok(Some(other)) => self.backlog.push(other),
                _ => {}
            }
        }
        err.map_or(Ok(()), Err)
    }

    /// A configure step whose daemon side may not exist yet: a missing feature is a warning, any
    /// other failure ends the run.
    fn configure_soft(&mut self, flag: &str, req: Request) -> Result<(), String> {
        match self.configure(req) {
            Ok(()) => Ok(()),
            Err(m) if m.contains("is not available yet") => {
                self.em.warn(&format!("{flag} is not supported by this daemon yet ({m}); continuing without it"));
                Ok(())
            }
            Err(m) => Err(format!("{flag}: {m}")),
        }
    }

    /// Switches the session's mode. A daemon without modes yet is tolerable for every mode but
    /// `plan`: without it this process still denies whatever asks (as strict as `default`, or
    /// stricter), but a plan run promises no edits, writes inside the workspace never ask, and only
    /// the daemon can hold that promise. For `plan` a daemon that cannot is an error.
    fn set_mode(&mut self, flag: &str, mode: PermissionMode) -> Result<(), String> {
        match self.configure(Request::SetMode { session: self.session.clone(), mode }) {
            Ok(()) => Ok(()),
            Err(m) if m.contains("is not available yet") && mode == PermissionMode::Plan => {
                Err(format!("{flag} plan: this daemon cannot hold plan mode yet ({m}); not running a session that could edit files"))
            }
            Err(m) if m.contains("is not available yet") => {
                self.em.warn(&format!("{flag} is not supported by this daemon yet ({m}); continuing without it"));
                Ok(())
            }
            Err(m) => Err(format!("{flag}: {m}")),
        }
    }

    /// Takes this run's `--mcp-config` servers back out of the daemon, so that they do not outlive
    /// the run in a daemon that serves other sessions. Best effort: a daemon that is gone has
    /// nothing to clean (and drops them itself when the connection ends).
    fn remove_run_servers(&mut self) {
        for name in std::mem::take(&mut self.mcp_added) {
            let req = Request::McpRemove { name, scope: SettingsScope::Session, workspace: self.workspace.clone() };
            // An error reply (already gone) is fine; so is a dead connection.
            let _ = self.configure(req);
        }
    }

    fn library(&mut self) -> Result<(), String> {
        self.send(&Request::LibraryState)?;
        let (models, slots) = self.client.wait_for(OP_TIMEOUT, lib_from).map_err(|e| e.to_string())??;
        let conductor = slots.iter().find(|s| s.slot == Slot::Conductor);
        let chosen = conductor.and_then(|s| s.model.clone()).or_else(|| models.first().map(|m| m.model.clone()));
        self.lib.effort = conductor.map(|s| s.effort).unwrap_or_default();
        self.lib.has_model = models.iter().map(|m| m.model.clone()).collect();
        self.apply_model(&models, chosen);
        Ok(())
    }

    fn apply_model(&mut self, models: &[marlowe_proto::LibraryModel], chosen: Option<ModelRef>) {
        let found = chosen.as_ref().and_then(|m| models.iter().find(|x| &x.model == m));
        self.lib.display = chosen.as_ref().map(|m| m.to_string()).unwrap_or_default();
        self.lib.pricing = found.and_then(|m| m.info.pricing);
        self.lib.model = chosen;
    }

    fn pick_model(&mut self, flag: &str) -> Result<(), String> {
        let model: ModelRef = flag.parse().map_err(|e| format!("--model: {e}"))?;
        if !self.lib.has_model.contains(&model) {
            self.send(&Request::AddModel { provider: model.provider, model_id: model.model_id.clone() })?;
            let (models, _) = self.client.wait_for(OP_TIMEOUT, lib_from).map_err(|e| e.to_string())?.map_err(|m| format!("--model {model}: {m}"))?;
            self.lib.has_model = models.iter().map(|m| m.model.clone()).collect();
        }
        // For this session only, once the daemon can; otherwise the Conductor slot, as `marlowe ask --model` does.
        let switched = self.configure(Request::SwitchModel { session: self.session.clone(), model: model.clone(), effort: None });
        match switched {
            Ok(()) => {}
            Err(m) if m.contains("is not available yet") => {
                self.send(&Request::SetSlot { slot: Slot::Conductor, model: Some(model.clone()), effort: self.lib.effort })?;
                self.client.wait_for(OP_TIMEOUT, |e| matches!(e, Event::LibraryState { .. }).then_some(())).map_err(|e| e.to_string())?.map_err(|m| format!("--model {model}: {m}"))?;
            }
            Err(m) => return Err(format!("--model {model}: {m}")),
        }
        self.send(&Request::LibraryState)?;
        let (models, _) = self.client.wait_for(OP_TIMEOUT, lib_from).map_err(|e| e.to_string())??;
        self.apply_model(&models, Some(model));
        Ok(())
    }

    /// The `SessionOpened` addressed to this client (`seq` 0), for `want` when given.
    fn wait_session(&mut self, want: Option<&SessionId>) -> Result<SessionId, String> {
        let end = Instant::now() + OP_TIMEOUT;
        loop {
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err("timed out waiting for the daemon to open the session".into());
            }
            match self.client.next_event(left.min(Duration::from_secs(5))).map_err(|e| e.to_string())? {
                Some(Envelope { seq: 0, event: Event::SessionOpened { session, .. }, .. }) if want.is_none_or(|w| *w == session) => return Ok(session),
                Some(Envelope { event: Event::Error { message, .. }, .. }) => return Err(message),
                _ => {}
            }
        }
    }

    fn find_session(&mut self, workspace: Option<&str>) -> Result<Option<SessionSummary>, String> {
        self.send(&Request::ListSessions { limit: 500 })?;
        let list = self
            .client
            .wait_for(OP_TIMEOUT, |e| match e {
                Event::SessionList { sessions } => Some(sessions.clone()),
                _ => None,
            })
            .map_err(|e| e.to_string())??;
        Ok(match &self.args.resume {
            Resume::Id(id) => list.into_iter().find(|s| s.id.0 == *id || (id.len() >= 8 && s.id.0.starts_with(id.as_str()))),
            Resume::Continue => {
                let want = workspace.map(normalise);
                list.into_iter().filter(|s| s.workspace.as_deref().map(normalise) == want).max_by_key(|s| s.updated_ms)
            }
            Resume::No => None,
        })
    }

    fn setup(&mut self, home: &std::path::Path, workspace: Option<&str>, rs: &settings::RunSettings, mcp: &[(String, Value)]) -> Result<(), String> {
        self.library()?;

        // The session.
        let resumed = !matches!(self.args.resume, Resume::No);
        if resumed {
            let found = self.find_session(workspace)?;
            match found {
                Some(s) => self.session = s.id,
                None => {
                    return Err(match &self.args.resume {
                        Resume::Id(id) => format!("no session {id}"),
                        _ => "no earlier session in this workspace to continue".into(),
                    })
                }
            }
            // Loads it into the daemon.
            let want = self.session.clone();
            self.send(&Request::OpenSession { session: want.clone() })?;
            self.wait_session(Some(&want))?;
        } else {
            self.send(&Request::NewSession { workspace: workspace.map(str::to_string) })?;
            self.session = self.wait_session(None)?;
        }

        // MCP servers for this run, then settings, directories, mode and model.
        for (name, entry) in mcp {
            self.send(&Request::McpAdd { name: name.clone(), scope: SettingsScope::Session, config: entry.clone(), workspace: workspace.map(str::to_string) })?;
            self.mcp_added.push(name.clone());
            self.client.wait_for(OP_TIMEOUT, |e| matches!(e, Event::McpServers { .. }).then_some(())).map_err(|e| e.to_string())?.map_err(|m| format!("--mcp-config {name}: {m}"))?;
        }
        for (key, value) in &rs.forward {
            self.configure_soft("--settings", Request::SetSetting { scope: SettingsScope::Session, key: key.clone(), value: value.clone(), workspace: workspace.map(str::to_string) })?;
        }
        for dir in self.args.add_dirs.clone() {
            let abs = std::fs::canonicalize(&dir).map(crate::strip_verbatim).map_err(|e| format!("--add-dir {dir}: {e}"))?;
            self.configure_soft("--add-dir", Request::AddDir { session: self.session.clone(), path: abs.display().to_string() })?;
        }
        if self.args.mode.is_some() || rs.mode.is_some() {
            self.set_mode("--permission-mode", self.mode)?;
        }
        if let Some(m) = self.args.model.clone() {
            self.pick_model(&m)?;
        }
        if let Some(name) = self.args.prompt_tool.clone().filter(|n| n != "stdio") {
            let ws = workspace.map(std::path::PathBuf::from);
            self.prompt_tool = Some(PromptTool::connect(&name, home, ws.as_deref(), mcp.to_vec(), self.args.strict_mcp)?);
        }

        // Which servers the session has, for the init line.
        let servers = if self.em.stream() {
            self.send(&Request::McpList { workspace: workspace.map(str::to_string) })?;
            self.client
                .wait_for(OP_TIMEOUT, |e| match e {
                    Event::McpServers { list } => Some(list.iter().map(|s| (s.name.clone(), s.status.clone())).collect::<Vec<_>>()),
                    _ => None,
                })
                .ok()
                .and_then(Result::ok)
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let cwd = workspace.unwrap_or("").to_string();
        let (sid, model, mode) = (self.session.0.clone(), self.lib.display.clone(), self.mode.as_str());
        let api_key_source = match self.lib.model.as_ref().map(|m| m.provider) {
            Some(kind) if !kind.is_local() => key_source(kind, &KeyringStore::new(), &|k| std::env::var(k).ok()),
            _ => "none",
        };
        self.em.system_init(&Init { session: &sid, cwd: &cwd, model: &model, mode, mcp_servers: servers, resumed, api_key_source });
        self.last_activity = Instant::now();
        Ok(())
    }

    // ------------------------------------------------------------ the loop

    fn send_user(&mut self, text: &str) {
        let steer = self.turn.is_some();
        if steer {
            if let Some(t) = self.turn.as_mut() {
                t.steers += 1;
            }
        } else {
            self.ask_pending = true;
        }
        self.settle_until = None;
        let req = Request::Ask { session: Some(self.session.clone()), text: text.to_string(), workspace: None, images: Vec::new(), options: self.options.clone() };
        if let Err(e) = self.send(&req) {
            self.fatal = Some(e);
        }
        self.last_activity = Instant::now();
    }

    fn interrupt_turn(&mut self) {
        if self.interrupt_sent {
            return;
        }
        self.interrupt_sent = true;
        let _ = self.send(&Request::Interrupt { session: self.session.clone() });
    }

    fn drive(&mut self) -> i32 {
        loop {
            if self.interrupt.load(Ordering::Relaxed) && !self.cancelled {
                self.cancelled = true;
                if self.turn.is_some() || self.ask_pending {
                    self.interrupt_turn();
                }
            }
            while let Some(msg) = self.stdin.as_ref().and_then(|rx| rx.try_recv().ok()) {
                self.on_stdin(msg);
            }
            if !self.backlog.is_empty() {
                for env in std::mem::take(&mut self.backlog) {
                    self.on_event(env);
                }
            }
            match self.client.next_event(TICK) {
                Ok(Some(env)) => self.on_event(env),
                Ok(None) => {}
                Err(ClientError::Closed) => self.fatal = Some("the daemon closed the connection".into()),
                Err(e) => self.fatal = Some(e.to_string()),
            }
            if let Some(code) = self.finished() {
                return code;
            }
        }
    }

    /// The exit code once nothing is left to wait for.
    fn finished(&mut self) -> Option<i32> {
        if let Some(msg) = self.fatal.take() {
            let sid = self.session.0.clone();
            self.em.failure(&msg, &sid, EXIT_ERROR);
            return Some(EXIT_ERROR);
        }
        let busy = self.turn.is_some() || self.ask_pending || !self.pending.is_empty();
        // Ctrl-C: out as soon as the cancelled turn has reported (or there was none to wait for).
        if self.cancelled && self.turn.is_none() {
            return Some(EXIT_CANCELLED);
        }
        if busy {
            let idle = self.args.idle_timeout;
            if self.last_activity.elapsed() > idle {
                self.interrupt_turn();
                let sid = self.session.0.clone();
                self.em.failure(&format!("no activity for {} s; giving up (--idle-timeout)", idle.as_secs()), &sid, EXIT_ERROR);
                return Some(EXIT_ERROR);
            }
            return None;
        }
        if let Some(until) = self.settle_until {
            if Instant::now() < until {
                return None;
            }
        }
        // Single-prompt runs end with their turn; a stream ends when stdin does.
        if self.stdin.is_none() && self.results > 0 {
            return Some(self.exit);
        }
        if self.exit == EXIT_BUDGET {
            return Some(self.exit);
        }
        if self.stdin.is_some() && self.stdin_done {
            return Some(self.exit);
        }
        None
    }

    // ------------------------------------------------------------ stdin

    fn on_stdin(&mut self, msg: StdinMsg) {
        self.last_activity = Instant::now();
        let line = match msg {
            StdinMsg::Eof => {
                self.stdin_done = true;
                // Nobody is left to answer what is outstanding: refuse it now, or the run would
                // wait for an answer that cannot come until `--idle-timeout`.
                self.settle_pending("the client closed its input");
                return;
            }
            StdinMsg::Line(l) => l,
        };
        let parsed = match input::parse_line(&line) {
            Ok(Some(i)) => i,
            Ok(None) => return,
            Err(e) => {
                self.em.warn(&e);
                return;
            }
        };
        match parsed {
            Input::User { text } => {
                if self.exit == EXIT_BUDGET {
                    self.em.warn("the budget is spent; ignoring further input");
                } else {
                    self.send_user(&text);
                }
            }
            Input::Interrupt { id } => {
                self.interrupt_sent = false;
                self.em.control_response(&id, Ok(json!({})));
                self.interrupt_turn();
            }
            Input::SetMode { id, mode } => match self.set_mode("set_permission_mode", mode) {
                Ok(()) => {
                    self.mode = mode;
                    self.em.control_response(&id, Ok(json!({"mode": mode.as_str()})));
                }
                // The mode stays what it was; the client is told it did not take.
                Err(e) => self.em.control_response(&id, Err(e)),
            },
            Input::Initialize { id } => {
                let model = self.lib.display.clone();
                self.em.control_response(&id, Ok(json!({"commands": [], "models": [model], "permission_mode": self.mode.as_str()})));
            }
            Input::AnswerQuestion { id, question, answers } => {
                let _ = self.send(&Request::AnswerQuestion { session: self.session.clone(), id: question, answers });
                self.em.control_response(&id, Ok(json!({})));
            }
            Input::UnknownControl { id, subtype } => self.em.control_response(&id, Err(format!("unsupported control request {subtype:?}"))),
            Input::Response { request_id, result } => self.on_control_response(&request_id, result),
        }
    }

    fn on_control_response(&mut self, id: &str, result: Result<Value, String>) {
        let Some(p) = self.pending.remove(id) else {
            self.em.warn(&format!("control_response for unknown request {id:?}"));
            return;
        };
        match p {
            Pending::Approval { approval, tool, input } => {
                let verdict = match result {
                    Ok(v) => match v.get("behavior").and_then(Value::as_str) {
                        // Running something other than what was asked about is not an allow.
                        Some("allow") => prompt_tool::rewritten(&v, &input).map_or(Ok(()), Err),
                        Some("deny") => Err(v.get("message").and_then(Value::as_str).unwrap_or("denied by the client").to_string()),
                        _ => Err("the client's answer has no behavior of allow or deny".to_string()),
                    },
                    Err(e) => Err(e),
                };
                match verdict {
                    Ok(()) => self.decide(approval, ApprovalDecision::Allow),
                    Err(why) => self.deny(approval, &tool, &input, &why),
                }
            }
            Pending::Plan { id: pid } => {
                let decision = match &result {
                    Ok(v) if v.get("behavior").and_then(Value::as_str) == Some("allow") => {
                        let asked = v.get("updatedInput").and_then(|i| i.get("mode").or_else(|| i.get("permission_mode"))).and_then(Value::as_str);
                        PlanFileDecision::Approve { mode: plan_mode(asked) }
                    }
                    Ok(v) => PlanFileDecision::Reject { feedback: v.get("message").and_then(Value::as_str).map(str::to_string) },
                    Err(e) => PlanFileDecision::Reject { feedback: Some(e.clone()) },
                };
                let _ = self.send(&Request::AnswerPlan { session: self.session.clone(), id: pid, decision });
            }
            Pending::Question { id: qid, count } => {
                let answers = match &result {
                    Ok(v) if v.get("behavior").and_then(Value::as_str) == Some("allow") => {
                        let mut a = v.get("updatedInput").map(input::answers_of).unwrap_or_default();
                        a.resize(count, String::new());
                        a
                    }
                    _ => vec![NO_ANSWER.to_string(); count],
                };
                let _ = self.send(&Request::AnswerQuestion { session: self.session.clone(), id: qid, answers });
            }
        }
    }

    /// Answers everything the client was asked and has not answered: approvals are denied, questions
    /// get [`NO_ANSWER`].
    fn settle_pending(&mut self, why: &str) {
        let mut open: Vec<(String, Pending)> = self.pending.drain().collect();
        open.sort_by(|a, b| a.0.cmp(&b.0));
        for (rid, p) in open {
            self.em.control_cancel(&rid);
            match p {
                Pending::Approval { approval, tool, input } => self.deny(approval, &tool, &input, why),
                Pending::Question { id, count } => {
                    let _ = self.send(&Request::AnswerQuestion { session: self.session.clone(), id, answers: vec![NO_ANSWER.to_string(); count] });
                }
                Pending::Plan { id } => {
                    let _ = self.send(&Request::AnswerPlan { session: self.session.clone(), id, decision: PlanFileDecision::Reject { feedback: Some(why.to_string()) } });
                }
            }
        }
    }

    /// Forgets an approval the daemon settled on its own (timeout, interrupt, another client) and
    /// tells the client that its request is moot.
    fn approval_resolved(&mut self, approval: ApprovalId) {
        let gone: Vec<String> = self.pending.iter().filter(|(_, p)| matches!(p, Pending::Approval { approval: a, .. } if *a == approval)).map(|(k, _)| k.clone()).collect();
        for rid in gone {
            self.pending.remove(&rid);
            self.em.control_cancel(&rid);
        }
    }

    // ------------------------------------------------------------ daemon events

    fn on_event(&mut self, env: Envelope) {
        self.last_activity = Instant::now();
        let ev = env.event;
        if let Event::ApprovalResolved { approval, .. } = &ev {
            // Approval ids are the daemon's own, so this needs no session check.
            self.approval_resolved(*approval);
            return;
        }
        if let Event::Error { session, message } = &ev {
            if session.is_none() || session.as_ref() == Some(&self.session) {
                self.on_error(message);
            }
            return;
        }
        if ev.session() != Some(&self.session) {
            return;
        }
        match ev {
            Event::TurnStarted { .. } => {
                self.ask_pending = false;
                if self.cancelled {
                    // Ctrl-C arrived between the ask and its turn.
                    self.interrupt_sent = false;
                    self.interrupt_turn();
                }
                self.settle_until = None;
                self.interrupt_sent = false;
                self.turn = Some(Turn { started: Some(Instant::now()), ..Turn::default() });
            }
            Event::TextDelta { text, .. } => {
                if let Some(t) = self.turn.as_mut() {
                    t.text.push_str(&text);
                }
                let sid = self.session.0.clone();
                self.em.text_delta(&sid, &text);
            }
            Event::ReasoningDelta { text, .. } => {
                let sid = self.session.0.clone();
                self.em.reasoning_delta(&sid, &text);
            }
            Event::TextRetracted { .. } => {
                if let Some(t) = self.turn.as_mut() {
                    t.text.truncate(t.call_start);
                }
            }
            Event::ToolStarted { call_id, tool, summary, .. } => {
                let (sid, model) = (self.session.0.clone(), self.lib.display.clone());
                let text = self.turn.as_mut().map(|t| {
                    t.tool_started.insert(call_id.clone(), Instant::now());
                    t.tool_names.insert(call_id.clone(), tool.clone());
                    t.call_start = 0;
                    std::mem::take(&mut t.text)
                });
                self.em.assistant(&sid, &model, text.as_deref().unwrap_or(""), Some((&call_id, &tool, &json!({"summary": summary}))));
                self.em.progress(&format!("  {tool}: {summary}"));
            }
            Event::ToolFinished { call_id, ok, summary, .. } => {
                if let Some(t) = self.turn.as_mut() {
                    if let Some(at) = t.tool_started.remove(&call_id) {
                        t.tool_time += at.elapsed();
                    }
                    // Refused by the daemon's permission layer (a deny rule, plan or dont_ask
                    // mode) without an approval: a denial all the same.
                    if let (false, Some(reason)) = (ok, summary.strip_prefix(marlowe_agent::REFUSED_SUMMARY)) {
                        let tool = t.tool_names.get(&call_id).cloned().unwrap_or_default();
                        let reason = reason.trim_start_matches(':').trim();
                        t.denials.push(json!({"tool_name": tool, "tool_use_id": call_id, "tool_input": {}, "reason": reason}));
                    }
                }
                let sid = self.session.0.clone();
                self.em.tool_result(&sid, &call_id, ok, &summary);
            }
            Event::ContextUsage { .. } => {
                let limit = self.args.max_steps;
                let mut over = false;
                if let Some(t) = self.turn.as_mut() {
                    t.steps += 1;
                    t.call_start = t.text.len();
                    over = limit.is_some_and(|m| t.steps > m) && !t.step_limit;
                    t.step_limit |= over;
                }
                if over {
                    self.interrupt_turn();
                }
            }
            Event::ApprovalRequested { approval, tool, summary, detail, run: None, .. } => self.on_approval(approval, &tool, &summary, &detail),
            Event::QuestionAsked { id, questions, .. } => self.on_question(&id, &questions),
            Event::PlanReady { id, path, text, .. } => self.on_plan(&id, &path, &text),
            Event::TurnFinished { usage, stop, .. } => self.on_turn_finished(usage, stop),
            _ => {}
        }
    }

    fn on_error(&mut self, message: &str) {
        if message.contains("is not available yet") {
            self.em.warn(message);
            return;
        }
        if self.turn.is_none() && self.ask_pending {
            // The daemon refused the ask or could not start the turn: no TurnFinished will come.
            self.fatal = Some(message.to_string());
            return;
        }
        if let Some(t) = self.turn.as_mut() {
            t.error.get_or_insert_with(|| message.to_string());
        }
        self.em.warn(message);
    }

    // ------------------------------------------------------------ approvals and questions

    fn decide(&mut self, approval: ApprovalId, decision: ApprovalDecision) {
        let _ = self.send(&Request::Approve { approval, decision, reason: None });
    }

    fn deny(&mut self, approval: ApprovalId, tool: &str, input: &Value, why: &str) {
        if let Some(t) = self.turn.as_mut() {
            t.denials.push(json!({"tool_name": tool, "tool_use_id": approval.0.to_string(), "tool_input": input, "reason": why}));
        }
        self.em.progress(&format!("  denied {tool}: {why}"));
        self.decide(approval, ApprovalDecision::Deny);
    }

    fn on_approval(&mut self, approval: ApprovalId, tool: &str, summary: &str, detail: &str) {
        let input: Value = serde_json::from_str(detail).unwrap_or_else(|_| json!({"detail": detail}));
        match self.policy.check(tool, &input) {
            Verdict::Allow => return self.decide(approval, ApprovalDecision::Allow),
            Verdict::Deny(why) => return self.deny(approval, tool, &input, &why),
            Verdict::Open => {}
        }
        if self.mode == PermissionMode::DontAsk {
            return self.deny(approval, tool, &input, &format!("{tool} is not allowed in dont_ask mode ({summary})"));
        }
        if self.args.prompt_tool.as_deref() == Some("stdio") {
            self.next_req += 1;
            let id = format!("perm-{}", self.next_req);
            self.em.control_request(
                &id,
                json!({"subtype": "can_use_tool", "tool_name": cc_name(tool), "marlowe_tool": tool, "input": input, "summary": summary, "tool_use_id": approval.0.to_string()}),
            );
            self.pending.insert(id, Pending::Approval { approval, tool: tool.to_string(), input });
            return;
        }
        if let Some(pt) = &self.prompt_tool {
            let flag = self.interrupt.clone();
            let answer = pt.ask(tool, &input, &move || flag.load(Ordering::Relaxed));
            return match answer {
                Ok(Decision::Allow) => self.decide(approval, ApprovalDecision::Allow),
                Ok(Decision::Deny(why)) => self.deny(approval, tool, &input, &why),
                Err(e) => self.deny(approval, tool, &input, &format!("permission prompt tool: {e}")),
            };
        }
        if self.args.fail_on_ask {
            if let Some(t) = self.turn.as_mut() {
                t.approval_needed.get_or_insert_with(|| format!("{tool}: {summary}"));
            }
            self.deny(approval, tool, &input, "approval needed (--fail-on-ask)");
            self.interrupt_turn();
            return;
        }
        self.deny(approval, tool, &input, "nothing allowed it and nobody can be asked in print mode (see --allowed-tools, --permission-mode, --permission-prompt-tool)");
    }

    /// `exit_plan` is waiting: the plan file is written. A stream-json client gets a `can_use_tool`
    /// for `ExitPlanMode` (the plan and its file; `allow` approves, `updatedInput.mode` picks
    /// `accept_edits` or `default`, `deny` rejects with its `message` as feedback). Otherwise the
    /// permission prompt tool decides, and with none nobody can approve: rejected, plan mode stays.
    fn on_plan(&mut self, id: &str, path: &str, text: &str) {
        let reject = |this: &mut Self, why: &str| {
            let _ = this.send(&Request::AnswerPlan {
                session: this.session.clone(),
                id: id.to_string(),
                decision: PlanFileDecision::Reject { feedback: Some(why.to_string()) },
            });
        };
        let input = json!({"plan": text, "planFilePath": path, "id": id});
        if self.args.prompt_tool.as_deref() == Some("stdio") {
            self.next_req += 1;
            let rid = format!("plan-{}", self.next_req);
            self.em.control_request(&rid, json!({"subtype": "can_use_tool", "tool_name": "ExitPlanMode", "marlowe_tool": "exit_plan", "input": input}));
            self.pending.insert(rid, Pending::Plan { id: id.to_string() });
            return;
        }
        if let Some(pt) = &self.prompt_tool {
            let flag = self.interrupt.clone();
            let answer = pt.ask("exit_plan", &input, &move || flag.load(Ordering::Relaxed));
            return match answer {
                Ok(Decision::Allow) => {
                    let _ = self.send(&Request::AnswerPlan {
                        session: self.session.clone(),
                        id: id.to_string(),
                        decision: PlanFileDecision::Approve { mode: PermissionMode::Default },
                    });
                }
                Ok(Decision::Deny(why)) => reject(self, &why),
                Err(e) => reject(self, &format!("permission prompt tool: {e}")),
            };
        }
        if self.args.fail_on_ask {
            if let Some(t) = self.turn.as_mut() {
                t.approval_needed.get_or_insert_with(|| "exit_plan: a plan needs approval".to_string());
            }
            reject(self, "approval needed (--fail-on-ask)");
            self.interrupt_turn();
            return;
        }
        reject(self, "nobody can approve a plan in print mode (see --permission-prompt-tool); plan mode stays on");
    }

    fn on_question(&mut self, id: &str, questions: &[Question]) {
        let count = questions.len().max(1);
        if self.args.input == InputFormat::StreamJson {
            self.next_req += 1;
            let rid = format!("ask-{}", self.next_req);
            let qs: Vec<Value> = questions
                .iter()
                .map(|q| question_json(q))
                .collect();
            self.em.control_request(
                &rid,
                json!({"subtype": "can_use_tool", "tool_name": "AskUserQuestion", "input": {"questions": qs, "id": id}}),
            );
            self.pending.insert(rid, Pending::Question { id: id.to_string(), count });
        } else {
            let _ = self.send(&Request::AnswerQuestion { session: self.session.clone(), id: id.to_string(), answers: vec![NO_ANSWER.to_string(); count] });
        }
    }

    // ------------------------------------------------------------ results

    fn cost_of(&self, usage: &Usage) -> f64 {
        usage.cost_usd.or_else(|| self.lib.pricing.map(|p| p.cost(usage.input, usage.output))).unwrap_or(0.0)
    }

    fn on_turn_finished(&mut self, usage: Usage, stop: TurnStop) {
        let Some(turn) = self.turn.take() else { return };
        // Whatever was still outstanding died with the turn (an interrupt, a step limit); the
        // daemon has already settled it, so the client is only told to stop waiting.
        let stale: Vec<String> = self.pending.drain().map(|(k, _)| k).collect();
        for rid in stale {
            self.em.control_cancel(&rid);
        }
        let sid = self.session.0.clone();
        let model = self.lib.display.clone();
        self.em.assistant(&sid, &model, &turn.text, None);
        let cost = self.cost_of(&usage);
        self.spent_usd += cost;
        let over_budget = self.args.max_budget_usd.is_some_and(|b| self.spent_usd > b);

        let (subtype, exit, stop_name, error): (&'static str, i32, &'static str, Option<String>) = if self.cancelled && stop == TurnStop::Interrupted {
            ("error_during_execution", EXIT_CANCELLED, "interrupted", Some("cancelled".into()))
        } else if let Some(what) = &turn.approval_needed {
            ("error_approval_needed", EXIT_APPROVAL, "interrupted", Some(format!("approval needed for {what}")))
        } else if turn.step_limit || stop == TurnStop::StepLimit {
            ("error_max_turns", EXIT_ERROR, "step_limit", Some(format!("stopped after {} model calls (--max-steps)", self.args.max_steps.unwrap_or(turn.steps))))
        } else if stop == TurnStop::Budget || over_budget {
            ("error_max_budget_usd", EXIT_BUDGET, "budget", Some(format!("the budget is spent (${:.4})", self.spent_usd)))
        } else {
            match stop {
                TurnStop::Done => ("success", EXIT_OK, "done", None),
                TurnStop::Interrupted => ("error_during_execution", EXIT_CANCELLED, "interrupted", Some("interrupted".into())),
                _ => ("error_during_execution", EXIT_ERROR, "error", Some(turn.error.clone().unwrap_or_else(|| "the turn failed".into()))),
            }
        };
        let elapsed = turn.started.map(|s| s.elapsed()).unwrap_or_default();
        let result = TurnResult {
            subtype,
            is_error: exit != EXIT_OK,
            text: turn.text.clone(),
            session: sid,
            duration_ms: elapsed.as_millis() as u64,
            api_ms: elapsed.saturating_sub(turn.tool_time).as_millis() as u64,
            num_turns: turn.steps,
            usage,
            cost_usd: cost,
            denials: turn.denials.clone(),
            stop: stop_name,
            exit_code: exit,
            error,
        };
        self.em.result(&result);
        self.results += 1;
        self.exit = exit;
        if turn.steers > 0 {
            self.settle_until = Some(Instant::now() + SETTLE);
        }
        self.interrupt_sent = false;
    }
}

#[cfg(test)]
mod f3_tests {
    use marlowe_proto::{PermissionMode, Question, QuestionOption};
    use serde_json::json;

    use super::{plan_mode, question_json};

    #[test]
    fn parity_t33_the_headless_question_carries_descriptions_previews_and_multi_select() {
        let q = Question {
            question: "Which?".into(),
            header: "Pick".into(),
            options: vec!["a".into(), "b".into()],
            multi_select: true,
            option_details: vec![
                QuestionOption { label: "a".into(), description: "first".into(), preview: Some("fn a() {}".into()) },
                QuestionOption { label: "b".into(), ..Default::default() },
            ],
        };
        let v = question_json(&q);
        assert_eq!(v["multiSelect"], json!(true));
        assert_eq!(v["options"], json!(["a", "b"]));
        assert_eq!(v["option_details"][0], json!({"label": "a", "description": "first", "preview": "fn a() {}"}));
        // A question with plain labels keeps Claude Code's exact shape.
        let plain = Question { option_details: Vec::new(), ..q };
        assert!(question_json(&plain).get("option_details").is_none());
    }

    #[test]
    fn a_plan_approval_names_accept_edits_or_falls_back_to_default() {
        assert_eq!(plan_mode(Some("acceptEdits")), PermissionMode::AcceptEdits);
        assert_eq!(plan_mode(Some("accept_edits")), PermissionMode::AcceptEdits);
        assert_eq!(plan_mode(Some("bypassPermissions")), PermissionMode::Default);
        assert_eq!(plan_mode(None), PermissionMode::Default);
    }
}
