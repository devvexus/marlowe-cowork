//! Hooks (design 2.6 X1-X9): every Claude Code event, run as a [`ToolMiddleware`] and
//! [`TurnObserver`] ([`HookMiddleware`]) over a [`HookRunner`] (a daemon service).
//!
//! | Event | Where | What its answer can do |
//! |---|---|---|
//! | UserPromptSubmit | `before_turn` | block the prompt, add context |
//! | PreToolUse | `before_approval` | deny, ask, allow (ask rules still apply), rewrite the input, add context |
//! | PermissionRequest | `run_tools.rs`, before the modal | allow or deny it |
//! | PostToolUse / PostToolUseFailure | `after_call` | feed a reason or context back, replace an MCP result |
//! | Stop / SubagentStop | `before_final` | keep the agent going (at most [`MAX_FORCED_CONTINUES`] in a row) |
//! | PreCompact | `before_compact` | inform |
//! | SessionStart | `on_session_start` | context for the first turn |
//! | Notification | `on_notification` | inform (off the asking thread) |
//! | SessionEnd, SubagentStart, PostCompact | [`HookRunner::session_end`], [`HookRunner::subagent_start`], [`HookRunner::post_compact`] | inform; context for the subagent |
//!
//! Command hooks get the event's JSON on stdin (`marlowe_settings::protocol`), run through the
//! user's shell (Git Bash on Windows, as Claude Code) via `marlowe_os::child` (scrubbed
//! environment, Job Object) in the workspace with `MARLOWE_PROJECT_DIR` and `CLAUDE_PROJECT_DIR`
//! set, one at a time in configured order, each under its timeout (60 s by default). Prompt hooks
//! go to a [`PromptHookEvaluator`] the daemon supplies; its answer is read like a command hook's
//! stdout. `continue: false` from any hook ends the turn.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use marlowe_model::ToolCall;
use marlowe_permission::rules::policy::HookPermission;
use marlowe_proto::{PermissionMode, SessionId};
use marlowe_settings::hooks::{HookEntry, HookEvent, HookKind};
use marlowe_settings::protocol::{cc_mode, cc_tool_response, from_cc_input, input_json, interpret, EventData, HookContext, HookOutcome, HookRun};
use serde_json::Value;

use crate::approval::{HookVerdicts, Permissions};
use crate::convo::Conversation;
use crate::middleware::{CallFlow, FinalFlow, Notification, NotificationKind, StepInfo, ToolMiddleware, TurnFlow, TurnObserver};
use crate::tools::{CallCtx, ToolOutcome};

/// Stop hooks may keep the agent going at most this many times in a row.
pub const MAX_FORCED_CONTINUES: u32 = 8;

/// Bytes kept of a hook's stdout and stderr each.
const OUTPUT_CAP: usize = 1 << 20;

/// How long a hook's output may keep arriving after it exits before processes it left behind
/// are killed (they would otherwise hold its pipes, and the turn, open).
const READER_GRACE: Duration = Duration::from_millis(500);

/// Evaluates a `type: "prompt"` hook: an LLM reads the prompt (with `$ARGUMENTS` replaced by the
/// event's JSON) and answers with hook output JSON. The daemon supplies one (its quick slot).
pub trait PromptHookEvaluator: Send + Sync {
    fn evaluate(&self, prompt: &str) -> Result<String, String>;
}

/// Builds the command that runs a hook's command line.
pub type ShellFn = Arc<dyn Fn(&str) -> std::io::Result<Command> + Send + Sync>;

/// Runs a session's configured hooks. A daemon service: [`HookMiddleware`] and `run_tools.rs`
/// find it in `Services`.
pub struct HookRunner {
    hooks: RwLock<Vec<HookEntry>>,
    workspace: Option<PathBuf>,
    session: SessionId,
    model: RwLock<String>,
    transcript_path: Option<String>,
    shell: ShellFn,
    prompt: Option<Arc<dyn PromptHookEvaluator>>,
    perms: Option<Permissions>,
    /// Shown to the user (systemMessage, notes, diagnostics); drained by the daemon.
    messages: Mutex<Vec<String>>,
}

impl std::fmt::Debug for HookRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HookRunner").field("hooks", &self.hooks.read().map(|h| h.len()).unwrap_or(0)).finish_non_exhaustive()
    }
}

impl HookRunner {
    pub fn new(hooks: Vec<HookEntry>, workspace: Option<PathBuf>, session: SessionId) -> Self {
        HookRunner {
            hooks: RwLock::new(hooks),
            workspace,
            session,
            model: RwLock::new(String::new()),
            transcript_path: None,
            shell: Arc::new(default_shell),
            prompt: None,
            perms: None,
            messages: Mutex::new(Vec::new()),
        }
    }

    pub fn with_shell(mut self, shell: ShellFn) -> Self {
        self.shell = shell;
        self
    }

    pub fn with_prompt_evaluator(mut self, e: Arc<dyn PromptHookEvaluator>) -> Self {
        self.prompt = Some(e);
        self
    }

    /// Reads the mode from the session's permissions.
    pub fn with_permissions(mut self, p: Permissions) -> Self {
        self.perms = Some(p);
        self
    }

    pub fn with_transcript(mut self, path: Option<String>) -> Self {
        self.transcript_path = path;
        self
    }

    pub fn set_model(&self, model: impl Into<String>) {
        *self.model.write().unwrap_or_else(|p| p.into_inner()) = model.into();
    }

    /// Replaces the hooks (after a reload or a trust).
    pub fn set_hooks(&self, hooks: Vec<HookEntry>) {
        *self.hooks.write().unwrap_or_else(|p| p.into_inner()) = hooks;
    }

    /// Whether any hook listens to `event`.
    pub fn has(&self, event: HookEvent) -> bool {
        self.hooks.read().unwrap_or_else(|p| p.into_inner()).iter().any(|h| h.event == event)
    }

    /// Messages for the user since the last call (systemMessages, failures, diagnostics).
    pub fn take_messages(&self) -> Vec<String> {
        std::mem::take(&mut *self.messages.lock().unwrap_or_else(|p| p.into_inner()))
    }

    /// The directory hooks run in.
    pub fn cwd(&self) -> Option<&Path> {
        self.workspace.as_deref()
    }

    fn mode(&self) -> PermissionMode {
        self.perms.as_ref().map(Permissions::mode).unwrap_or_default()
    }

    fn context(&self, turn: Option<u32>) -> HookContext {
        HookContext {
            session_id: self.session.0.clone(),
            transcript_path: self.transcript_path.clone(),
            cwd: self.workspace.as_deref().map(|p| p.display().to_string()).unwrap_or_default(),
            model: self.model.read().unwrap_or_else(|p| p.into_inner()).clone(),
            permission_mode: cc_mode(self.mode()).to_string(),
            turn_id: turn.map(|t| t.to_string()).unwrap_or_default(),
            agent_id: None,
            agent_type: None,
        }
    }

    /// Runs every hook of `data`'s event whose matcher selects it, in order, and folds their
    /// answers. Stops early when `cancelled()`.
    pub fn run(&self, data: &EventData, ctx: &HookContext, cancelled: &dyn Fn() -> bool) -> HookOutcome {
        let event = data.event();
        let target = data.matcher_target();
        let hooks: Vec<HookEntry> = self
            .hooks
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .filter(|h| h.event == event && target.is_none_or(|t| h.matches(t)))
            .cloned()
            .collect();
        let mut out = HookOutcome::default();
        if hooks.is_empty() {
            return out;
        }
        let input = input_json(ctx, data).to_string();
        let tool = match data {
            EventData::PreToolUse { tool_name, .. }
            | EventData::PostToolUse { tool_name, .. }
            | EventData::PostToolUseFailure { tool_name, .. }
            | EventData::PermissionRequest { tool_name, .. } => Some(tool_name.clone()),
            _ => None,
        };
        for h in hooks {
            if cancelled() {
                break;
            }
            let mut run = match &h.kind {
                HookKind::Command(cmd) => self.exec(cmd, &input, Duration::from_secs(h.timeout), cancelled),
                HookKind::Prompt(p) => self.ask_llm(p, &input),
            };
            run.tool = tool.clone();
            let o = interpret(event, &run);
            {
                let mut m = self.messages.lock().unwrap_or_else(|p| p.into_inner());
                m.extend(o.system_messages.iter().cloned());
                if !o.suppress_output {
                    m.extend(o.notes.iter().cloned());
                }
                m.extend(o.diagnostics.iter().cloned());
            }
            out.merge(o);
        }
        out
    }

    fn ask_llm(&self, prompt: &str, input: &str) -> HookRun {
        let shown = format!("prompt: {}", prompt.chars().take(60).collect::<String>());
        match &self.prompt {
            None => HookRun { command: shown, spawn_error: Some("no model is available for prompt hooks".into()), ..HookRun::default() },
            Some(e) => {
                let full = if prompt.contains("$ARGUMENTS") { prompt.replace("$ARGUMENTS", input) } else { format!("{prompt}\n\n{input}") };
                match e.evaluate(&full) {
                    Ok(text) => HookRun { command: shown, exit: Some(0), stdout: text, ..HookRun::default() },
                    Err(why) => HookRun { command: shown, spawn_error: Some(why), ..HookRun::default() },
                }
            }
        }
    }

    /// Runs one command hook: the input on stdin, both outputs read under a cap, killed (with
    /// its tree) at the timeout or on cancel.
    fn exec(&self, command: &str, input: &str, timeout: Duration, cancelled: &dyn Fn() -> bool) -> HookRun {
        let mut run = HookRun { command: command.to_string(), ..HookRun::default() };
        let mut cmd = match (self.shell)(command) {
            Ok(c) => c,
            Err(e) => {
                run.spawn_error = Some(e.to_string());
                return run;
            }
        };
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut spec = marlowe_os::child::ChildSpec::new();
        if let Some(ws) = &self.workspace {
            let dir = ws.display().to_string();
            spec = spec.with_cwd(ws.clone()).with_env([("MARLOWE_PROJECT_DIR".to_string(), dir.clone()), ("CLAUDE_PROJECT_DIR".to_string(), dir)]);
        }
        let mut proc = match marlowe_os::child::spawn(&mut cmd, &spec) {
            Ok(p) => p,
            Err(e) => {
                run.spawn_error = Some(e.to_string());
                return run;
            }
        };
        let child = proc.child_mut();
        if let Some(mut stdin) = child.stdin.take() {
            let input = input.to_string();
            std::thread::spawn(move || {
                let _ = stdin.write_all(input.as_bytes());
            });
        }
        let pipes: [Option<Box<dyn Read + Send>>; 2] = [
            child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>),
            child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>),
        ];
        // Each reader fills a shared buffer and reports EOF on the channel, so output read so
        // far is kept even when a background process the hook started holds a pipe open.
        let (done_tx, done_rx) = crossbeam_channel::unbounded::<()>();
        let bufs: Vec<Arc<Mutex<Vec<u8>>>> = pipes
            .into_iter()
            .map(|pipe| {
                let buf = Arc::new(Mutex::new(Vec::new()));
                let (fill, done) = (buf.clone(), done_tx.clone());
                std::thread::spawn(move || {
                    if let Some(mut p) = pipe {
                        let mut chunk = [0u8; 8192];
                        while let Ok(n) = p.read(&mut chunk) {
                            if n == 0 {
                                break;
                            }
                            let mut b = fill.lock().unwrap_or_else(|e| e.into_inner());
                            let room = OUTPUT_CAP.saturating_sub(b.len());
                            b.extend_from_slice(&chunk[..n.min(room)]);
                        }
                    }
                    let _ = done.send(());
                });
                buf
            })
            .collect();
        drop(done_tx);
        let started = Instant::now();
        let status = loop {
            match proc.child_mut().try_wait() {
                Ok(Some(s)) => break Some(s),
                Ok(None) => {}
                Err(_) => break None,
            }
            if cancelled() {
                run.cancelled = true;
            } else if started.elapsed() >= timeout {
                run.timed_out = true;
            } else {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            proc.kill_tree();
            let _ = proc.child_mut().wait();
            break None;
        };
        // The hook is done; what it left running (`npm run dev &`) must not hold the turn.
        let drained = |grace: Duration| (0..2).all(|_| done_rx.recv_timeout(grace).is_ok());
        if !drained(READER_GRACE) {
            proc.kill_tree();
            let _ = drained(READER_GRACE);
        }
        let text = |b: &Arc<Mutex<Vec<u8>>>| String::from_utf8_lossy(&b.lock().unwrap_or_else(|e| e.into_inner())).into_owned();
        run.stdout = text(&bufs[0]);
        run.stderr = text(&bufs[1]);
        run.exit = status.and_then(|s| s.code());
        run
    }

    /// PermissionRequest hooks for an approval about to be shown: `Some((true, _))` allows it,
    /// `Some((false, why))` denies it (`interrupt` also ends the turn), `None` leaves it to the
    /// user.
    pub fn permission_request(&self, tool: &str, args: &Value, cx: &CallCtx, mode: PermissionMode) -> Option<(bool, Option<String>)> {
        if !self.has(HookEvent::PermissionRequest) {
            return None;
        }
        let mut ctx = self.context(Some(cx.turn));
        ctx.permission_mode = cc_mode(mode).to_string();
        let data = EventData::PermissionRequest { tool_name: tool.to_string(), tool_input: args.clone() };
        let cancel = cx.cancel.clone();
        let o = self.run(&data, &ctx, &move || cancel.is_cancelled());
        if let Some(d) = o.permission_request {
            if d.interrupt && !d.allow {
                cx.cancel.cancel();
            }
            return Some((d.allow, d.message));
        }
        o.block.map(|why| (false, Some(why)))
    }

    /// SessionEnd hooks (the daemon calls this when a session closes). `reason`: `clear`,
    /// `logout`, `prompt_input_exit`, `other`.
    pub fn session_end(&self, reason: &str) -> HookOutcome {
        self.run(&EventData::SessionEnd { reason: reason.to_string() }, &self.context(None), &|| false)
    }

    /// PostCompact hooks (the compaction path calls this after history was compacted;
    /// `trigger`: `manual` or `auto`).
    pub fn post_compact(&self, trigger: &str) -> HookOutcome {
        self.run(&EventData::PostCompact { trigger: trigger.to_string() }, &self.context(None), &|| false)
    }

    /// SubagentStart hooks (the subagent runner calls this); their context goes to the subagent.
    pub fn subagent_start(&self, agent_id: &str, agent_type: &str) -> HookOutcome {
        let mut ctx = self.context(None);
        ctx.agent_id = Some(agent_id.to_string());
        ctx.agent_type = Some(agent_type.to_string());
        self.run(&EventData::SubagentStart { agent_id: agent_id.to_string(), agent_type: agent_type.to_string() }, &ctx, &|| false)
    }

    /// SubagentStop hooks for an agent that ran outside this loop (a workflow agent, X22): they
    /// inform; the agent has already ended, so a block cannot keep it going.
    pub fn subagent_stop(&self, agent_id: &str, agent_type: &str, last_message: &str) -> HookOutcome {
        let mut ctx = self.context(None);
        ctx.agent_id = Some(agent_id.to_string());
        ctx.agent_type = Some(agent_type.to_string());
        let data = EventData::SubagentStop {
            agent_id: agent_id.to_string(),
            agent_type: agent_type.to_string(),
            agent_transcript_path: None,
            stop_hook_active: false,
            last_assistant_message: last_message.to_string(),
        };
        self.run(&data, &ctx, &|| false)
    }
}

/// Git Bash on Windows (as Claude Code runs hooks), `sh -c` elsewhere.
fn default_shell(command: &str) -> std::io::Result<Command> {
    #[cfg(windows)]
    {
        let bash = git_bash().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "hooks need Git Bash (or MARLOWE_BASH)"))?;
        let mut c = Command::new(bash);
        c.arg("-c").arg(command);
        Ok(c)
    }
    #[cfg(not(windows))]
    {
        let mut c = Command::new("sh");
        c.arg("-c").arg(command);
        Ok(c)
    }
}

#[cfg(windows)]
fn git_bash() -> Option<PathBuf> {
    static FOUND: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    FOUND
        .get_or_init(|| {
            if let Some(p) = std::env::var_os("MARLOWE_BASH").map(PathBuf::from).filter(|p| p.is_file()) {
                return Some(p);
            }
            ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"]
                .iter()
                .filter_map(std::env::var_os)
                .flat_map(|v| [PathBuf::from(&v).join("Git"), PathBuf::from(&v).join("Programs").join("Git")])
                .chain([PathBuf::from(r"C:\Program Files\Git")])
                .flat_map(|root| [root.join(r"bin\bash.exe"), root.join(r"usr\bin\bash.exe")])
                .find(|c| c.is_file())
        })
        .clone()
}

/// The hooks as loop middleware and session observer.
pub struct HookMiddleware {
    runner: Arc<HookRunner>,
    /// Set on a subagent's agent: its final answer fires SubagentStop (with this id and type).
    subagent: Option<(String, String)>,
    /// SessionStart context waiting for the first turn.
    pending_context: Mutex<Vec<String>>,
    /// PreToolUse context per call id, appended to the call's result.
    pre_context: Mutex<HashMap<String, String>>,
    /// Stop hooks' forced continuations in a row.
    continues: AtomicU32,
}

impl HookMiddleware {
    pub fn new(runner: Arc<HookRunner>) -> Self {
        HookMiddleware {
            runner,
            subagent: None,
            pending_context: Mutex::new(Vec::new()),
            pre_context: Mutex::new(HashMap::new()),
            continues: AtomicU32::new(0),
        }
    }

    /// For a subagent's agent: Stop becomes SubagentStop.
    pub fn for_subagent(runner: Arc<HookRunner>, agent_id: &str, agent_type: &str) -> Self {
        HookMiddleware { subagent: Some((agent_id.to_string(), agent_type.to_string())), ..Self::new(runner) }
    }

    pub fn runner(&self) -> &Arc<HookRunner> {
        &self.runner
    }

    fn ctx(&self, turn: Option<u32>) -> HookContext {
        let mut c = self.runner.context(turn);
        if let Some((id, ty)) = &self.subagent {
            c.agent_id = Some(id.clone());
            c.agent_type = Some(ty.clone());
        }
        c
    }
}

fn wrap_context(event: &str, text: &str) -> String {
    format!("<{event}-hook>\n{text}\n</{event}-hook>")
}

impl ToolMiddleware for HookMiddleware {
    fn name(&self) -> &str {
        "hooks"
    }

    /// Hook context never joins the user's text (the memory engine ingests that at user trust,
    /// and hook output is not the user's words): it waits in `Conversation::pending_context`
    /// and rides along as its own part of the user message.
    fn before_turn(&self, convo: &mut Conversation, text: &mut String) -> TurnFlow {
        let mut context = Vec::new();
        if self.runner.has(HookEvent::UserPromptSubmit) {
            let o = self.runner.run(&EventData::UserPromptSubmit { prompt: text.clone() }, &self.ctx(Some(convo.next_turn)), &|| false);
            if let Some(why) = o.stop.clone().or_else(|| o.block.clone()) {
                return TurnFlow::Block(why);
            }
            if let Some(c) = o.context_text() {
                context.push(wrap_context("user-prompt-submit", &c));
            }
        }
        let pending = std::mem::take(&mut *self.pending_context.lock().unwrap_or_else(|p| p.into_inner()));
        if !pending.is_empty() {
            context.insert(0, wrap_context("session-start", &pending.join("\n")));
        }
        convo.pending_context.extend(context);
        TurnFlow::Continue
    }

    fn before_approval(&self, call: &mut ToolCall, cx: &CallCtx) -> CallFlow {
        if !self.runner.has(HookEvent::PreToolUse) {
            return CallFlow::Continue;
        }
        let data = EventData::PreToolUse { tool_name: call.name.clone(), tool_input: call.args.clone(), tool_use_id: call.id.clone() };
        let cancel = cx.cancel.clone();
        let o = self.runner.run(&data, &self.ctx(Some(cx.turn)), &move || cancel.is_cancelled());
        if let Some(why) = &o.stop {
            cx.cancel.cancel();
            return CallFlow::Done(ToolOutcome { summary: "stopped by a hook".into(), ..ToolOutcome::error(format!("Not run: {why}")) });
        }
        if let Some(HookPermission::Deny(why)) = &o.permission {
            return CallFlow::Done(ToolOutcome {
                summary: "blocked by a hook".into(),
                ..ToolOutcome::error(format!("Not run: a PreToolUse hook blocked it: {why}"))
            });
        }
        if let Some(updated) = &o.updated_input {
            let new = from_cc_input(&call.name, &call.args, updated);
            if let (Value::Object(new), Value::Object(old)) = (new, &mut call.args) {
                for (k, v) in new {
                    old.insert(k, v);
                }
            }
        }
        if let Some(c) = o.context_text() {
            self.pre_context.lock().unwrap_or_else(|p| p.into_inner()).insert(call.id.clone(), c);
        }
        if let Some(p) = o.permission {
            cx.scope.with_ext(|e| {
                e.get_or_default::<HookVerdicts>().0.insert(call.id.clone(), p);
            });
        }
        CallFlow::Continue
    }

    fn after_call(&self, call: &ToolCall, cx: &CallCtx, outcome: &mut ToolOutcome) {
        if let Some(c) = self.pre_context.lock().unwrap_or_else(|p| p.into_inner()).remove(&call.id) {
            outcome.content.push_str("\n\n");
            outcome.content.push_str(&wrap_context("pre-tool-use", &c));
        }
        let data = if outcome.is_error {
            if !self.runner.has(HookEvent::PostToolUseFailure) {
                return;
            }
            EventData::PostToolUseFailure {
                tool_name: call.name.clone(),
                tool_input: call.args.clone(),
                tool_use_id: call.id.clone(),
                error: outcome.content.clone(),
            }
        } else {
            if !self.runner.has(HookEvent::PostToolUse) {
                return;
            }
            EventData::PostToolUse {
                tool_name: call.name.clone(),
                tool_input: call.args.clone(),
                tool_response: cc_tool_response(&call.name, &call.args, &outcome.content),
                tool_use_id: call.id.clone(),
            }
        };
        let cancel = cx.cancel.clone();
        let o = self.runner.run(&data, &self.ctx(Some(cx.turn)), &move || cancel.is_cancelled());
        if call.name.starts_with("mcp__") {
            if let Some(v) = &o.updated_mcp_output {
                outcome.content = v.as_str().map_or_else(|| v.to_string(), str::to_string);
            }
        }
        if let Some(why) = &o.block {
            outcome.content.push_str("\n\n");
            outcome.content.push_str(&wrap_context("post-tool-use", &format!("A hook reported a problem with this result: {why}")));
        }
        if let Some(c) = o.context_text() {
            outcome.content.push_str("\n\n");
            outcome.content.push_str(&wrap_context("post-tool-use", &c));
        }
        if let Some(why) = o.stop {
            cx.cancel.cancel();
            self.runner.messages.lock().unwrap_or_else(|p| p.into_inner()).push(why);
        }
    }

    fn before_final(&self, info: StepInfo, _convo: &mut Conversation, text: &str) -> FinalFlow {
        let event = if self.subagent.is_some() { HookEvent::SubagentStop } else { HookEvent::Stop };
        if !self.runner.has(event) {
            return FinalFlow::Accept;
        }
        let active = self.continues.load(Ordering::SeqCst) > 0;
        let data = match &self.subagent {
            Some((id, ty)) => EventData::SubagentStop {
                agent_id: id.clone(),
                agent_type: ty.clone(),
                agent_transcript_path: None,
                stop_hook_active: active,
                last_assistant_message: text.to_string(),
            },
            None => EventData::Stop { stop_hook_active: active, last_assistant_message: text.to_string() },
        };
        let o = self.runner.run(&data, &self.ctx(Some(info.turn)), &|| false);
        match o.block {
            Some(why) if o.stop.is_none() && self.continues.load(Ordering::SeqCst) < MAX_FORCED_CONTINUES => {
                self.continues.fetch_add(1, Ordering::SeqCst);
                FinalFlow::Continue(wrap_context("stop", &why))
            }
            _ => {
                self.continues.store(0, Ordering::SeqCst);
                FinalFlow::Accept
            }
        }
    }

    fn before_compact(&self, convo: &mut Conversation, focus: Option<&str>) {
        if !self.runner.has(HookEvent::PreCompact) {
            return;
        }
        let trigger = if focus.is_some() { "manual" } else { "auto" };
        let data = EventData::PreCompact { trigger: trigger.into(), custom_instructions: focus.map(str::to_string) };
        let _ = self.runner.run(&data, &self.ctx(Some(convo.next_turn)), &|| false);
    }
}

impl TurnObserver for HookMiddleware {
    fn on_session_start(&self, convo: &Conversation) {
        if !self.runner.has(HookEvent::SessionStart) {
            return;
        }
        let source = if convo.messages.is_empty() { "startup" } else { "resume" };
        let o = self.runner.run(&EventData::SessionStart { source: source.into() }, &self.ctx(None), &|| false);
        if let Some(c) = o.context_text() {
            self.pending_context.lock().unwrap_or_else(|p| p.into_inner()).push(c);
        }
    }

    fn on_notification(&self, _session: &SessionId, note: &Notification) {
        if !self.runner.has(HookEvent::Notification) {
            return;
        }
        let kind = match note.kind {
            NotificationKind::PermissionPrompt => "permission_prompt",
            NotificationKind::Question => "elicitation_dialog",
            NotificationKind::IdlePrompt => "idle_prompt",
        };
        let data = EventData::Notification { message: note.message.clone(), notification_type: kind.into() };
        let ctx = self.ctx(None);
        let runner = self.runner.clone();
        // Off the asking thread: a slow hook must not hold the approval back.
        std::thread::spawn(move || {
            let _ = runner.run(&data, &ctx, &|| false);
        });
    }
}

#[cfg(test)]
mod tests;
