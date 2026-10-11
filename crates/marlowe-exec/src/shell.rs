//! Child processes: bash, powershell, python. Bounded in time and memory; each starts through
//! `marlowe_os::child` (scrubbed environment, its own kill-on-close Job Object), so a tree kill
//! reaches every grandchild and no API key reaches the child. Track B owns this file (the
//! interpreters, Git Bash discovery, leftovers); the tools' runner is `specs/shell.rs`.

use std::path::PathBuf;
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use marlowe_model::CancelToken;
use marlowe_os::child::{self, ChildSpec, RunLimits};
use marlowe_os::workspace::ExecOutput;
use marlowe_os::JobObject;

/// Default wall-clock limit for a shell call.
pub const SHELL_TIMEOUT_MS: u64 = 120_000;

/// Longest limit a call may request with `timeout_ms`.
pub const SHELL_TIMEOUT_MAX_MS: u64 = 600_000;

/// Output a tool call may write before the child is killed. The tools keep it all in a log file
/// (the model sees at most [`MAX_SHELL_RESULT_BYTES`] of it), so this is a runaway guard, not a
/// context limit.
pub const MAX_SHELL_OUTPUT_BYTES: usize = 64 << 20;

/// Output returned to the model (`summarize::BUDGET`); the rest stays in the log.
pub const MAX_SHELL_RESULT_BYTES: usize = 30_000;

#[derive(Debug, Clone, Copy)]
pub struct ShellLimits {
    pub timeout_ms: u64,
    pub max_output_bytes: usize,
}

impl ShellLimits {
    pub fn production() -> Self {
        Self { timeout_ms: SHELL_TIMEOUT_MS, max_output_bytes: MAX_SHELL_OUTPUT_BYTES }
    }

    pub fn with_timeout(mut self, ms: u64) -> Self {
        self.timeout_ms = ms.clamp(1, SHELL_TIMEOUT_MAX_MS);
        self
    }
}

/// What a run produced and how it ended.
#[derive(Debug)]
pub struct ShellRun {
    pub code: i32,
    pub text: String,
    /// Killed at the time limit.
    pub timed_out: bool,
    /// Killed after exceeding the per-stream output cap.
    pub flooded: bool,
    /// Killed because the turn was interrupted.
    pub cancelled: bool,
    /// The job still holding processes the command left running (background `&` children).
    /// Dropping it kills them; see [`Leftovers`].
    pub leftover: Option<JobObject>,
}

impl ShellRun {
    /// A finished [`marlowe_os::workspace::Workspace::exec`], with stdout and stderr combined.
    pub fn from_exec(out: ExecOutput) -> Self {
        Self {
            code: out.code,
            text: combine(&out.stdout, &out.stderr),
            timed_out: out.timed_out,
            flooded: out.flooded,
            cancelled: out.cancelled,
            leftover: out.leftover,
        }
    }
}

/// Which interpreter a call uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interpreter {
    Bash,
    PowerShell,
    Python,
}

impl Interpreter {
    pub fn tool_name(self) -> &'static str {
        match self {
            Interpreter::Bash => "bash",
            Interpreter::PowerShell => "powershell",
            Interpreter::Python => "python",
        }
    }
}

/// The command for `bash -c <script>`.
pub fn bash_command(script: &str) -> std::io::Result<Command> {
    #[cfg(windows)]
    let program = git_bash().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "the `bash` tool needs Git Bash and it is not installed. Install Git for Windows, or \
             set MARLOWE_BASH to a bash.exe. The `powershell` and `python` tools do not need it.",
        )
    })?;
    #[cfg(not(windows))]
    let program = PathBuf::from("bash");
    let mut c = Command::new(program);
    c.arg("-c").arg(script);
    Ok(c)
}

/// The command for `powershell -NoProfile -NonInteractive -Command <script>`.
pub fn powershell_command(script: &str) -> Command {
    #[cfg(windows)]
    let program = "powershell.exe";
    #[cfg(not(windows))]
    let program = "pwsh";
    let mut c = Command::new(program);
    // Output is decoded as UTF-8, so make the console emit it.
    let script = format!("[Console]::OutputEncoding = [System.Text.Encoding]::UTF8; {script}");
    c.args(["-NoProfile", "-NonInteractive", "-Command"]).arg(script);
    c
}

/// What to run for the `python` tool.
pub enum PythonTarget<'a> {
    Code(&'a str),
    Script(&'a str),
}

/// The command for the user's python, or an error saying none was found.
pub fn python_command(target: PythonTarget<'_>, args: &[String]) -> std::io::Result<Command> {
    let (program, prefix) = find_python().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no python interpreter was found (tried `py -3`, `python`, `python3`). Install Python \
             or set MARLOWE_PYTHON to its path.",
        )
    })?;
    let mut c = Command::new(program);
    c.args(prefix).arg("-u");
    match target {
        PythonTarget::Code(code) => c.arg("-c").arg(code),
        PythonTarget::Script(path) => c.arg(path),
    };
    c.args(args).env("PYTHONIOENCODING", "utf-8");
    Ok(c)
}

type PythonChoice = (PathBuf, Vec<&'static str>);

fn find_python() -> Option<PythonChoice> {
    static FOUND: OnceLock<Option<PythonChoice>> = OnceLock::new();
    FOUND
        .get_or_init(|| {
            if let Some(explicit) = std::env::var_os("MARLOWE_PYTHON") {
                return Some((PathBuf::from(explicit), Vec::new()));
            }
            let candidates: [(&str, Vec<&'static str>); 3] =
                [("py", vec!["-3"]), ("python", vec![]), ("python3", vec![])];
            for (program, prefix) in candidates {
                let mut probe = Command::new(program);
                probe.args(&prefix).args(["-c", "import sys; sys.exit(0)"]);
                let limits = ShellLimits { timeout_ms: 8_000, max_output_bytes: 4096 };
                if let Ok(run) = run_bounded(probe, limits, None) {
                    if run.code == 0 && !run.timed_out {
                        return Some((PathBuf::from(program), prefix));
                    }
                }
            }
            None
        })
        .clone()
}

/// Git Bash, never the WSL launcher that sits first on a default PATH.
#[cfg(windows)]
fn git_bash() -> Option<PathBuf> {
    static FOUND: OnceLock<Option<PathBuf>> = OnceLock::new();
    FOUND
        .get_or_init(|| {
            if let Some(explicit) = std::env::var_os("MARLOWE_BASH") {
                let p = PathBuf::from(explicit);
                return p.is_file().then_some(p);
            }
            let mut roots: Vec<PathBuf> = Vec::new();
            for var in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
                if let Some(v) = std::env::var_os(var) {
                    roots.push(PathBuf::from(&v).join("Git"));
                    roots.push(PathBuf::from(&v).join("Programs").join("Git"));
                }
            }
            roots.push(PathBuf::from(r"C:\Program Files\Git"));
            roots.into_iter().find_map(|root| {
                [r"bin\bash.exe", r"usr\bin\bash.exe"]
                    .iter()
                    .map(|rel| root.join(rel))
                    .find(|c| c.is_file())
            })
        })
        .clone()
}

/// Per-call spawn settings: where the child starts and what its environment gains or loses.
#[derive(Debug, Clone, Default)]
pub struct SpawnOpts {
    /// Working directory; `None` keeps the command's own (or the process cwd).
    pub cwd: Option<PathBuf>,
    /// Variables set on the child (the crew's `GIT_CONFIG_*` no-push entries).
    pub env: Vec<(String, String)>,
    /// Variables removed from the child, before `env` is applied.
    pub env_remove: Vec<String>,
}

impl SpawnOpts {
    /// Removes the credential variables of [`marlowe_os::SCRUBBED_ENV`]. Every child is scrubbed
    /// by `marlowe_os::child` anyway; this states it at the call site.
    pub fn scrubbed() -> Self {
        Self { env_remove: marlowe_os::SCRUBBED_ENV.iter().map(|s| s.to_string()).collect(), ..Self::default() }
    }

    pub fn with_cwd(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cwd = Some(dir.into());
        self
    }

    pub fn with_env(mut self, env: impl IntoIterator<Item = (String, String)>) -> Self {
        self.env.extend(env);
        self
    }

    pub fn apply(&self, cmd: &mut Command) {
        for name in &self.env_remove {
            cmd.env_remove(name);
        }
        cmd.envs(self.env.iter().map(|(k, v)| (k, v)));
        if let Some(dir) = &self.cwd {
            cmd.current_dir(dir);
        }
    }
}

/// Runs `script` with `kind` (python: `script` is code) under `opts`, bounded like a tool call.
/// For callers outside a tool host, e.g. a crew playbook's test command.
pub fn run_script(
    kind: Interpreter,
    script: &str,
    opts: &SpawnOpts,
    limits: ShellLimits,
    cancel: Option<&CancelToken>,
) -> std::io::Result<ShellRun> {
    let mut cmd = match kind {
        Interpreter::Bash => bash_command(script)?,
        Interpreter::PowerShell => powershell_command(script),
        Interpreter::Python => python_command(PythonTarget::Code(script), &[])?,
    };
    opts.apply(&mut cmd);
    run_bounded(cmd, limits, cancel)
}

/// What happens to processes a command leaves behind when it exits on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Leftovers {
    /// They run on outside any kill-on-close job: a background server or training run started
    /// by the conductor outlives the call, as it always has.
    #[default]
    Release,
    /// They stay in their kill-on-close job, held by the host, and die when the host is dropped
    /// or [`HeldJobs::kill_all`] runs (crew runs: nothing outlives the run).
    KillWithHost,
}

/// Jobs whose commands finished but left processes running, kept so they die with the owner.
#[derive(Debug, Default)]
pub struct HeldJobs {
    jobs: Mutex<Vec<JobObject>>,
}

impl HeldJobs {
    /// Takes `run`'s leftover job according to `mode`.
    pub fn settle(&self, run: &mut ShellRun, mode: Leftovers) {
        let Some(job) = run.leftover.take() else { return };
        match mode {
            Leftovers::Release => {
                let _ = job.release();
            }
            Leftovers::KillWithHost => {
                let mut jobs = self.jobs.lock().unwrap_or_else(|p| p.into_inner());
                jobs.retain(|j| j.active_processes().map_or(true, |n| n > 0));
                jobs.push(job);
            }
        }
    }

    /// Processes still running in held jobs.
    pub fn active(&self) -> u32 {
        let jobs = self.jobs.lock().unwrap_or_else(|p| p.into_inner());
        jobs.iter().filter_map(|j| j.active_processes().ok()).sum()
    }

    /// Kills every held process now.
    pub fn kill_all(&self) {
        let mut jobs = self.jobs.lock().unwrap_or_else(|p| p.into_inner());
        for job in jobs.drain(..) {
            let _ = job.terminate(1);
        }
    }
}

/// Spawn `cmd` through `marlowe_os::child` (scrubbed environment, kill-on-close Job Object, no
/// window), read both pipes under a cap, wait under a deadline, and kill the whole process tree on
/// timeout, flood or cancel. stdin is null. If the command exits on its own while processes it
/// started still run, their job comes back in [`ShellRun::leftover`]: dropping it kills them,
/// [`HeldJobs::settle`] decides.
pub fn run_bounded(cmd: Command, limits: ShellLimits, cancel: Option<&CancelToken>) -> std::io::Result<ShellRun> {
    let run_limits =
        RunLimits { timeout: Duration::from_millis(limits.timeout_ms), max_output_bytes: limits.max_output_bytes };
    let cancelled = || cancel.is_some_and(CancelToken::is_cancelled);
    let out = child::run(cmd, &ChildSpec::new(), run_limits, &cancelled).map_err(|e| match e {
        marlowe_os::OsError::Io(io) => io,
        other => std::io::Error::other(other.to_string()),
    })?;
    Ok(ShellRun {
        code: out.code(),
        text: combine(&out.stdout, &out.stderr),
        timed_out: out.timed_out,
        flooded: out.flooded,
        cancelled: out.cancelled,
        leftover: out.leftover,
    })
}

fn combine(stdout: &[u8], stderr: &[u8]) -> String {
    let mut s = String::from_utf8_lossy(stdout).into_owned();
    if !stderr.is_empty() {
        if !s.is_empty() && !s.ends_with('\n') {
            s.push('\n');
        }
        s.push_str(&String::from_utf8_lossy(stderr));
    }
    s
}
