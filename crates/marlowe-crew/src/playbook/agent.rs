//! The agent-backed [`StepRunner`]. Each playbook step is one `marlowe_agent::Agent` turn built
//! from the step's role card: the card's instructions as the system core, the card's effort, its
//! tool allowlist (narrowed by the step, never widened, never `dispatch`) over the run's sandboxed
//! tool host, and the model its slot resolves to. Independent steps run on scoped threads, at most
//! the provider's concurrency limit at once.
//!
//! What surrounds a call — checkpoints, the run budget, pause/cancel, live events, the step
//! record — belongs to the supervisor and comes in through [`StepHooks`].

use std::io::Read;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use marlowe_agent::{Agent, AgentEvent, ApprovalGate, Budget, Conversation, DenyAll, Steer, SystemPrompt, ToolHost, TurnStop};
use marlowe_model::{
    CancelToken, ChatRequest, Completion, Message, ModelInfo, Provider, ProviderError, ProviderKind, Role, StreamEvent, Usage,
};
use marlowe_proto::{RunId, SandboxKind, SessionId};

use super::{ShellResult, StepResult, StepRunner, StepSpec};
use crate::role::{RoleCard, Roles};
use crate::sandbox::Sandbox;
use crate::tools::CrewTools;
use crate::CrewError;

/// Continuation turns a step gets after stopping at its per-turn step or budget limit.
pub const MAX_CONTINUES: u32 = 3;

/// The prompt of a continuation turn.
pub const CONTINUE_PROMPT: &str =
    "You stopped at a step limit. Continue from where you stopped and finish the task; end with your answer.";

/// A shell command's wall-clock limit.
pub const SHELL_TIMEOUT: Duration = Duration::from_secs(20 * 60);

/// Characters of combined output a shell result keeps (the tail).
pub const SHELL_OUTPUT_CHARS: usize = 16_000;

/// The model a role's step runs on.
#[derive(Clone)]
pub struct StepModel {
    pub provider: Arc<dyn Provider>,
    pub model: ModelInfo,
}

/// Resolves a role card to its model: the card's slot chain, with the GPU fallback applied.
pub type ModelResolver = Arc<dyn Fn(&RoleCard) -> Result<StepModel, CrewError> + Send + Sync>;

/// Runs a command in the sandbox. The default is [`sandbox_shell`].
pub type ShellFn = Arc<dyn Fn(&Sandbox, &str, &CancelToken) -> Result<ShellResult, CrewError> + Send + Sync>;

/// What the supervisor wraps around each agent call. Every method has a do-nothing default.
pub trait StepHooks: Send + Sync {
    /// The stored result of a completed step `key`; a resumed run skips the call.
    fn completed(&self, _key: &str) -> Option<StepResult> {
        None
    }

    /// Called before every agent call, and again before a continuation turn. `Err` stops the
    /// run (cancelled, paused, budget extension refused); it may block (pause, budget approval).
    fn admit(&self, _spec: &StepSpec) -> Result<(), CrewError> {
        Ok(())
    }

    /// The per-turn limits for the next call: what is left of the run budget.
    fn remaining(&self) -> Budget {
        Budget::default()
    }

    /// Before every model call of a step (`local`: its model runs on this machine). It may block
    /// (a budget approval, a local-model slot); `Err` ends the step with that error.
    fn before_call(&self, _spec: &StepSpec, _local: bool) -> Result<(), CrewError> {
        Ok(())
    }

    /// After every call `before_call` admitted; `usage` is `None` when the call did not finish.
    fn after_call(&self, _spec: &StepSpec, _local: bool, _usage: Option<&Usage>) {}

    /// The run has read untrusted content before this step.
    fn tainted(&self) -> bool {
        false
    }

    /// Live events of a running step.
    fn event(&self, _spec: &StepSpec, _ev: &AgentEvent) {}

    /// A step finished: its whole conversation and its result. The checkpoint goes here.
    fn finished(&self, _spec: &StepSpec, _messages: &[Message], _result: &StepResult) {}

    /// The run's one-line current action.
    fn progress(&self, _action: &str) {}
}

/// Hooks that do nothing: no checkpoints, no budget, no events.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoHooks;

impl StepHooks for NoHooks {}

/// A [`StepRunner`] that runs every step as an agent turn. Fields are public so the supervisor
/// can set what it owns; [`AgentRunner::new`] fills the rest with safe defaults.
pub struct AgentRunner {
    pub run: RunId,
    /// The cards the run started with.
    pub roles: Roles,
    pub models: ModelResolver,
    /// The run's inner tool host (from the `CrewToolFactory`), rooted in the sandbox.
    pub tools: Arc<dyn ToolHost>,
    pub sandbox: Sandbox,
    /// The original workspace, read-only to the crew.
    pub workspace: Option<PathBuf>,
    pub approvals: Arc<dyn ApprovalGate>,
    pub cancel: CancelToken,
    /// Steers for the run; whichever running step reaches a step boundary first takes them.
    pub inbox: Receiver<Steer>,
    pub hooks: Arc<dyn StepHooks>,
    pub shell: ShellFn,
    /// Concurrent steps per cloud provider.
    pub per_provider: usize,
    /// Concurrent steps per local provider.
    pub per_local_provider: usize,
}

impl AgentRunner {
    /// A runner that denies approvals, has no hooks, no steers, and runs shells with
    /// [`sandbox_shell`]; limits 2 per cloud provider and 1 per local one.
    pub fn new(
        run: RunId,
        roles: Roles,
        models: ModelResolver,
        tools: Arc<dyn ToolHost>,
        sandbox: Sandbox,
        workspace: Option<PathBuf>,
    ) -> AgentRunner {
        AgentRunner {
            run,
            roles,
            models,
            tools,
            sandbox,
            workspace,
            approvals: Arc::new(DenyAll),
            cancel: CancelToken::new(),
            inbox: crossbeam_channel::never(),
            hooks: Arc::new(NoHooks),
            shell: Arc::new(sandbox_shell),
            per_provider: 2,
            per_local_provider: 1,
        }
    }

    fn card(&self, role: &str) -> Result<&RoleCard, CrewError> {
        self.roles.get(role).ok_or_else(|| CrewError::UnknownRole(role.to_string()))
    }

    /// The card's allowlist, narrowed to `narrow` when the step gives one.
    fn allowlist(card: &RoleCard, narrow: Option<&[String]>) -> Vec<String> {
        card.tools
            .iter()
            .filter(|t| narrow.is_none_or(|n| n.iter().any(|x| x == *t)))
            .cloned()
            .collect()
    }

    /// The system core for one step: the card, then a few lines on where the step runs.
    fn system(&self, card: &RoleCard, spec: &StepSpec, allow: &[String]) -> SystemPrompt {
        let mut ctx = format!(
            "\n\n## This run\n- Crew run {}; you are the {}, on the step `{}`.\n",
            self.run.short(),
            card.name,
            spec.label
        );
        let root = self.sandbox.root.display();
        match self.sandbox.kind {
            SandboxKind::Worktree => ctx.push_str(&format!(
                "- Your git worktree (yours to change, branch `{}`): {root}\n",
                self.sandbox.branch.as_deref().unwrap_or("crew")
            )),
            SandboxKind::Scratch => ctx.push_str(&format!("- Your scratch directory (yours to write): {root}\n")),
            SandboxKind::None => ctx.push_str("- You have no writable directory on this step.\n"),
        }
        if let Some(ws) = &self.workspace {
            ctx.push_str(&format!("- The original workspace, read-only: {}\n", ws.display()));
        }
        if allow.is_empty() {
            ctx.push_str("- You have no tools on this step; answer from what you are given.\n");
        } else {
            ctx.push_str(&format!("- Tools: {}\n", allow.join(", ")));
        }
        SystemPrompt::with_core(format!("{}{ctx}", card.instructions.trim_end()))
    }

    /// The concurrency limit for a batch: the tightest limit among the providers it uses.
    fn batch_limit(&self, specs: &[StepSpec]) -> usize {
        let mut limit = usize::MAX;
        for spec in specs {
            let Some(card) = self.roles.get(&spec.role) else { continue };
            let Ok(m) = (self.models)(card) else { continue };
            let local = m.model.local || m.provider.kind().is_local();
            limit = limit.min(if local { self.per_local_provider } else { self.per_provider });
        }
        if limit == usize::MAX {
            self.per_provider.max(1)
        } else {
            limit.max(1)
        }
    }

    fn run_step(&self, spec: &StepSpec) -> Result<StepResult, CrewError> {
        if let Some(done) = self.hooks.completed(&spec.key) {
            return Ok(done);
        }
        let card = self.card(&spec.role)?;
        let allow = Self::allowlist(card, spec.tools.as_deref());
        let model = (self.models)(card)?;
        let tools = CrewTools::new(self.tools.clone(), allow.clone(), self.sandbox.clone());
        let failed = Arc::new(Mutex::new(None));
        let metered = Metered {
            local: model.model.local || model.provider.kind().is_local(),
            inner: model.provider,
            hooks: self.hooks.clone(),
            spec: spec.clone(),
            failed: failed.clone(),
        };
        let mut agent = Agent::new(Arc::new(metered), model.model, Arc::new(tools), self.system(card, spec, &allow));
        agent.effort = spec.effort.unwrap_or(card.effort);
        agent.approvals = self.approvals.clone();

        let session = SessionId::from(format!("crew:{}", self.run).as_str());
        let mut convo = Conversation::new(session, Some(self.sandbox.root.clone()));
        convo.exposure.tainted = self.hooks.tainted();

        self.hooks.progress(&spec.label);
        let mut usage = Usage::default();
        let mut prompt = spec.prompt.clone();
        let mut finished = false;
        for _ in 0..=MAX_CONTINUES {
            self.hooks.admit(spec)?;
            if self.cancel.is_cancelled() {
                return Err(CrewError::Cancelled);
            }
            agent.budget = self.hooks.remaining();
            let out = agent.run_turn(&mut convo, &prompt, &self.cancel, &self.inbox, &mut |ev| self.hooks.event(spec, &ev));
            usage.add(&out.usage);
            // A call the hooks refused (budget refused, paused, cancelled) ends the step with why.
            if let Some(e) = failed.lock().unwrap_or_else(|e| e.into_inner()).take() {
                return Err(e);
            }
            match out.stop {
                TurnStop::Done => {
                    finished = true;
                    break;
                }
                TurnStop::Interrupted => return Err(CrewError::Cancelled),
                TurnStop::Error => {
                    return Err(CrewError::Step {
                        step: spec.key.clone(),
                        message: out.error.unwrap_or_else(|| "the model call failed".into()),
                    })
                }
                TurnStop::StepLimit | TurnStop::Budget => prompt = CONTINUE_PROMPT.to_string(),
            }
        }
        let text = last_assistant_text(&convo.messages);
        if !finished || text.trim().is_empty() {
            return Err(CrewError::Step {
                step: spec.key.clone(),
                message: if finished { "the agent gave no answer".into() } else { "the step did not finish".into() },
            });
        }
        let result = StepResult { text, usage, tainted: convo.exposure.tainted };
        self.hooks.finished(spec, &convo.messages, &result);
        Ok(result)
    }
}

/// The text of the last assistant message that has any.
fn last_assistant_text(messages: &[Message]) -> String {
    messages
        .iter()
        .rev()
        .filter(|m| m.role == Role::Assistant)
        .map(Message::joined_text)
        .find(|t| !t.trim().is_empty())
        .unwrap_or_default()
}

/// The step's provider with the hooks around every model call (compaction included).
struct Metered {
    inner: Arc<dyn Provider>,
    hooks: Arc<dyn StepHooks>,
    spec: StepSpec,
    local: bool,
    /// Why `before_call` refused, for `run_step` to return.
    failed: Arc<Mutex<Option<CrewError>>>,
}

/// Calls `after_call` once, also when the provider panics.
struct AfterCall<'a> {
    m: &'a Metered,
    usage: Option<Usage>,
}

impl Drop for AfterCall<'_> {
    fn drop(&mut self) {
        self.m.hooks.after_call(&self.m.spec, self.m.local, self.usage.as_ref());
    }
}

impl Provider for Metered {
    fn kind(&self) -> ProviderKind {
        self.inner.kind()
    }

    fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        self.inner.list_models()
    }

    fn stream(&self, req: &ChatRequest, cancel: &CancelToken, on: &mut dyn FnMut(StreamEvent)) -> Result<Completion, ProviderError> {
        if let Err(e) = self.hooks.before_call(&self.spec, self.local) {
            *self.failed.lock().unwrap_or_else(|e| e.into_inner()) = Some(e);
            return Err(ProviderError::cancelled());
        }
        let mut after = AfterCall { m: self, usage: None };
        let result = self.inner.stream(req, cancel, on);
        after.usage = result.as_ref().ok().map(|c| c.usage);
        result
    }
}

impl StepRunner for AgentRunner {
    fn step(&self, spec: StepSpec) -> Result<StepResult, CrewError> {
        self.run_step(&spec)
    }

    fn completed(&self, key: &str) -> Option<StepResult> {
        self.hooks.completed(key)
    }

    fn parallel(&self, specs: Vec<StepSpec>) -> Vec<Result<StepResult, CrewError>> {
        let n = specs.len();
        let limit = self.batch_limit(&specs).min(n.max(1));
        let slots: Vec<Mutex<Option<Result<StepResult, CrewError>>>> = (0..n).map(|_| Mutex::new(None)).collect();
        let next = AtomicUsize::new(0);
        std::thread::scope(|s| {
            for _ in 0..limit {
                s.spawn(|| loop {
                    let i = next.fetch_add(1, Ordering::SeqCst);
                    if i >= n {
                        break;
                    }
                    let spec = &specs[i];
                    let r = catch_unwind(AssertUnwindSafe(|| self.run_step(spec))).unwrap_or_else(|_| {
                        Err(CrewError::Step { step: spec.key.clone(), message: "the step panicked".into() })
                    });
                    *slots[i].lock().unwrap_or_else(|e| e.into_inner()) = Some(r);
                });
            }
        });
        slots
            .into_iter()
            .zip(&specs)
            .map(|(m, spec)| {
                m.into_inner().unwrap_or_else(|e| e.into_inner()).unwrap_or_else(|| {
                    Err(CrewError::Step { step: spec.key.clone(), message: "the step never ran".into() })
                })
            })
            .collect()
    }

    fn shell(&self, command: &str) -> Result<ShellResult, CrewError> {
        if self.cancel.is_cancelled() {
            return Err(CrewError::Cancelled);
        }
        (self.shell)(&self.sandbox, command, &self.cancel)
    }

    fn sandbox(&self) -> &Sandbox {
        &self.sandbox
    }

    fn progress(&self, action: &str) {
        self.hooks.progress(action);
    }
}

// ---------------------------------------------------------------- shell

/// A process started for a crew shell: in a kill-on-close job where the OS has jobs.
enum Proc {
    Job(marlowe_os::JobChild),
    Plain(Child),
}

impl Proc {
    fn child(&mut self) -> &mut Child {
        match self {
            Proc::Job(j) => j.child_mut(),
            Proc::Plain(c) => c,
        }
    }

    fn kill_tree(&mut self) {
        match self {
            Proc::Job(j) => {
                let _ = j.kill_tree();
            }
            Proc::Plain(c) => {
                let _ = c.kill();
                let _ = c.wait();
            }
        }
    }
}

/// `cmd /C` on Windows, `sh -c` elsewhere.
fn shell_command(command: &str) -> Command {
    if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(command);
        c
    } else {
        let mut c = Command::new("sh");
        c.arg("-c").arg(command);
        c
    }
}

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
#[cfg(not(windows))]
const CREATE_NO_WINDOW: u32 = 0;

/// Runs `command` with cwd = the sandbox root, API keys scrubbed, the sandbox's no-push git
/// environment, in a kill-on-close Job Object. Output is the bounded tail of stdout + stderr.
/// Cancel kills the tree and returns `Cancelled`; [`SHELL_TIMEOUT`] kills it and fails.
pub fn sandbox_shell(sandbox: &Sandbox, command: &str, cancel: &CancelToken) -> Result<ShellResult, CrewError> {
    let mut cmd = shell_command(command);
    cmd.current_dir(&sandbox.root).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    for k in marlowe_brains::scrubbed_env() {
        cmd.env_remove(k);
    }
    for (k, v) in sandbox.child_env() {
        cmd.env(k, v);
    }
    let mut proc = match marlowe_os::spawn_in_job_with_flags(&mut cmd, CREATE_NO_WINDOW) {
        Ok(j) => Proc::Job(j),
        Err(marlowe_os::OsError::Unsupported(_)) => Proc::Plain(cmd.spawn()?),
        Err(marlowe_os::OsError::Io(e)) => return Err(CrewError::Io(e)),
        Err(e) => return Err(CrewError::Io(std::io::Error::other(e.to_string()))),
    };
    let readers = [
        proc.child().stdout.take().map(|r| read_tail(Box::new(r))),
        proc.child().stderr.take().map(|r| read_tail(Box::new(r))),
    ];
    let start = Instant::now();
    let status = loop {
        if cancel.is_cancelled() {
            proc.kill_tree();
            return Err(CrewError::Cancelled);
        }
        if start.elapsed() > SHELL_TIMEOUT {
            proc.kill_tree();
            break None;
        }
        match proc.child().try_wait()? {
            Some(s) => break Some(s),
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    };
    // Background processes the command left behind die with the job, closing the pipes.
    proc.kill_tree();
    let mut output = String::new();
    for r in readers.into_iter().flatten() {
        let bytes = r.join().unwrap_or_default();
        let text = String::from_utf8_lossy(&bytes);
        if !text.trim().is_empty() {
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str(text.trim_end());
        }
    }
    let mut output = tail_chars(&output, SHELL_OUTPUT_CHARS);
    Ok(match status {
        Some(s) => ShellResult { ok: s.success(), exit_code: s.code(), output },
        None => {
            output.push_str(&format!("\n[killed after {} minutes]", SHELL_TIMEOUT.as_secs() / 60));
            ShellResult { ok: false, exit_code: None, output }
        }
    })
}

/// Reads a pipe to its end on a thread, keeping at most the last 256 KiB.
fn read_tail(mut r: Box<dyn Read + Send>) -> std::thread::JoinHandle<Vec<u8>> {
    const KEEP: usize = 256 * 1024;
    std::thread::spawn(move || {
        let mut out = Vec::new();
        let mut buf = [0u8; 8192];
        while let Ok(n) = r.read(&mut buf) {
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
            if out.len() > 2 * KEEP {
                out.drain(..out.len() - KEEP);
            }
        }
        out
    })
}

/// The last `max` characters of `text`, marked when cut.
pub fn tail_chars(text: &str, max: usize) -> String {
    let n = text.chars().count();
    if n <= max {
        return text.to_string();
    }
    let tail: String = text.chars().skip(n - max).collect();
    format!("[… {} earlier characters cut]\n{tail}", n - max)
}
