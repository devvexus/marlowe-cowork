//! The one way the harness starts a child process.
//!
//! Every child (shells, hooks, LSP servers, services, project commands, `gh`, training runs, the
//! Edge renderer) goes through [`spawn`] or [`run`]. Both apply the credential scrub before the
//! process exists, so non-negotiable 5 holds by construction rather than by each caller
//! remembering it:
//!
//! - every name in [`SCRUBBED_ENV`] is removed from the child's environment, except the names
//!   this launch lists in [`ChildSpec::env_allow`] (shown in the approval, e.g. `HF_TOKEN` for a
//!   `train_launch`, `GH_TOKEN` for the `gh` child);
//! - [`ChildSpec::env`] may not set a scrubbed name that is not in `env_allow` (refused with
//!   [`OsError::EnvNotAllowed`]), so a key cannot be re-injected by the back door;
//! - the child starts in its own kill-on-close Job Object ([`Containment::JobOrBare`] falls back
//!   to a bare child with a `taskkill /T` / process-group tree kill where the OS refuses a job).
//!
//! Where later tracks plug in:
//! - B (`jobs`, background shells): [`spawn`] with pipes, keep the [`ChildProc`]; its
//!   [`ChildProc::into_leftover`] job is what a background job holds.
//! - C (LSP), D (services, project commands), E (hooks), J (`gh`), J2 (`train_launch`): build a
//!   `Command`, describe the launch in a [`ChildSpec`], call [`spawn`] or [`run`]. Never
//!   `Command::spawn`/`output`/`status` directly: `tests/spawn_audit.rs` fails the build for a new
//!   direct spawn outside its allowlist.
//! - K (`DockerExec`): the container runtime itself is launched through here; what runs inside
//!   the container is the container's business.

use std::ffi::OsStr;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::job::{JobChild, JobObject};
use crate::OsError;

/// Environment variables removed from every child process the harness spawns (API keys and
/// tokens). A launch keeps one only by naming it in [`ChildSpec::env_allow`].
pub const SCRUBBED_ENV: &[&str] = &[
    "OPENROUTER_API_KEY",
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
    "XAI_API_KEY",
    "MISTRAL_API_KEY",
    "GROQ_API_KEY",
    "DEEPSEEK_API_KEY",
    "TOGETHER_API_KEY",
    "FIREWORKS_API_KEY",
    "HF_TOKEN",
    "HUGGING_FACE_HUB_TOKEN",
];

/// `CREATE_NO_WINDOW`: console children never flash a window.
#[cfg(windows)]
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;
#[cfg(not(windows))]
pub const CREATE_NO_WINDOW: u32 = 0;

/// How a child is contained.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Containment {
    /// Its own kill-on-close Job Object; if the OS refuses one (a nested-job policy, or not
    /// Windows), a bare child whose tree is killed with `taskkill /T` (or its process group).
    #[default]
    JobOrBare,
    /// A kill-on-close Job Object or nothing: the spawn fails rather than run uncontained.
    Job,
    /// Outlives Marlowe on purpose (the Ollama server): no job of ours, its own process group,
    /// no console, and out of any job Marlowe itself runs in when that job allows breakaway.
    /// Still scrubbed. Training runs are not this: J2 keeps them in a detached job it owns.
    Detached,
}

/// Everything about one launch that is not the program and its arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildSpec {
    /// Scrubbed names this launch keeps from the parent environment. Shown in the approval.
    pub env_allow: Vec<String>,
    /// Variables set on the child after the scrub (e.g. the crew's no-push `GIT_CONFIG_*`).
    pub env: Vec<(String, String)>,
    /// Further variables removed (beyond [`SCRUBBED_ENV`]).
    pub env_remove: Vec<String>,
    /// Working directory; `None` keeps the command's own (or the process cwd).
    pub cwd: Option<PathBuf>,
    /// Extra process creation flags (Windows); `CREATE_SUSPENDED` is added for job children.
    pub creation_flags: u32,
    /// Start from an empty environment keeping only these parent variables (e.g. `SystemRoot`),
    /// before `env` is applied. Stricter than the scrub; for helpers that need nothing.
    pub clear_env_keep: Option<Vec<String>>,
    pub containment: Containment,
}

impl Default for ChildSpec {
    fn default() -> Self {
        Self {
            env_allow: Vec::new(),
            env: Vec::new(),
            env_remove: Vec::new(),
            cwd: None,
            creation_flags: CREATE_NO_WINDOW,
            clear_env_keep: None,
            containment: Containment::JobOrBare,
        }
    }
}

impl ChildSpec {
    /// A scrubbed, windowless child in its own job (or bare, when the OS refuses a job).
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_env_allow(mut self, names: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.env_allow.extend(names.into_iter().map(Into::into));
        self
    }

    pub fn with_env(mut self, env: impl IntoIterator<Item = (String, String)>) -> Self {
        self.env.extend(env);
        self
    }

    pub fn with_env_remove(mut self, names: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.env_remove.extend(names.into_iter().map(Into::into));
        self
    }

    pub fn with_cwd(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cwd = Some(dir.into());
        self
    }

    pub fn with_creation_flags(mut self, flags: u32) -> Self {
        self.creation_flags = flags;
        self
    }

    /// Starts the child from an empty environment plus `keep` (see [`ChildSpec::clear_env_keep`]).
    pub fn with_clear_env(mut self, keep: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.clear_env_keep = Some(keep.into_iter().map(Into::into).collect());
        self
    }

    pub fn with_containment(mut self, containment: Containment) -> Self {
        self.containment = containment;
        self
    }

    /// The scrubbed names this launch removes (every [`SCRUBBED_ENV`] name not in `env_allow`).
    pub fn scrubbed(&self) -> impl Iterator<Item = &'static str> + '_ {
        SCRUBBED_ENV.iter().copied().filter(|n| !allowed(&self.env_allow, n))
    }

    /// Applies the environment and cwd to `cmd`. Refuses an `env` entry that would re-inject a
    /// scrubbed name this launch did not allow.
    pub fn apply(&self, cmd: &mut Command) -> Result<(), OsError> {
        if let Some((k, _)) = self.env.iter().find(|(k, _)| is_scrubbed(k) && !allowed(&self.env_allow, k)) {
            return Err(OsError::EnvNotAllowed(k.clone()));
        }
        if let Some(keep) = &self.clear_env_keep {
            cmd.env_clear();
            for name in keep.iter().filter(|n| !is_scrubbed(n) || allowed(&self.env_allow, n)) {
                if let Some(v) = std::env::var_os(name) {
                    cmd.env(name, v);
                }
            }
        }
        for name in self.scrubbed() {
            cmd.env_remove(name);
        }
        for name in &self.env_remove {
            cmd.env_remove(name);
        }
        cmd.envs(self.env.iter().map(|(k, v)| (k, v)));
        if let Some(dir) = &self.cwd {
            cmd.current_dir(dir);
        }
        Ok(())
    }
}

/// Whether `name` is on the scrub list (case-insensitive, as Windows environment names are).
pub fn is_scrubbed(name: &str) -> bool {
    SCRUBBED_ENV.iter().any(|s| s.eq_ignore_ascii_case(name))
}

fn allowed(allow: &[String], name: &str) -> bool {
    allow.iter().any(|a| a.eq_ignore_ascii_case(name))
}

/// A started child: inside its own kill-on-close job, or bare where the OS refused a job.
#[derive(Debug)]
pub enum ChildProc {
    Job(JobChild),
    Bare(Child),
}

impl ChildProc {
    pub fn id(&self) -> u32 {
        match self {
            ChildProc::Job(j) => j.id(),
            ChildProc::Bare(c) => c.id(),
        }
    }

    /// The child, for its pipes and `wait`/`try_wait`.
    pub fn child_mut(&mut self) -> &mut Child {
        match self {
            ChildProc::Job(j) => j.child_mut(),
            ChildProc::Bare(c) => c,
        }
    }

    /// The job, when the child has one.
    pub fn job(&self) -> Option<&JobObject> {
        match self {
            ChildProc::Job(j) => Some(j.job()),
            ChildProc::Bare(_) => None,
        }
    }

    /// Kills the child and everything it started (does not reap; call `wait` after).
    pub fn kill_tree(&mut self) {
        match self {
            ChildProc::Job(j) => {
                if j.kill_tree().is_err() {
                    legacy_kill_tree(j.child_mut());
                }
            }
            ChildProc::Bare(c) => legacy_kill_tree(c),
        }
    }

    /// The job, when it still holds processes the command left running. Dropping it kills them.
    pub fn into_leftover(self) -> Option<JobObject> {
        match self {
            ChildProc::Job(j) => {
                let (_, job) = j.into_parts();
                job.active_processes().is_ok_and(|n| n > 0).then_some(job)
            }
            ChildProc::Bare(_) => None,
        }
    }
}

/// Starts `cmd` under `spec`: scrubbed environment, cwd, creation flags, Job Object.
pub fn spawn(cmd: &mut Command, spec: &ChildSpec) -> Result<ChildProc, OsError> {
    spec.apply(cmd)?;
    if spec.containment == Containment::Detached {
        return spawn_detached(cmd, spec.creation_flags).map(ChildProc::Bare).map_err(OsError::Io);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        match crate::job::spawn_in_job_with_flags(cmd, spec.creation_flags) {
            Ok(j) => return Ok(ChildProc::Job(j)),
            Err(OsError::Io(e)) => return Err(OsError::Io(e)),
            Err(e) if spec.containment == Containment::Job => return Err(e),
            Err(_) => {
                cmd.creation_flags(spec.creation_flags);
            }
        }
    }
    #[cfg(not(windows))]
    {
        if spec.containment == Containment::Job {
            return Err(OsError::Unsupported("job objects"));
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    Ok(ChildProc::Bare(cmd.spawn()?))
}

/// No console, its own process group, and out of Marlowe's own job when that job allows it.
fn spawn_detached(cmd: &mut Command, flags: u32) -> std::io::Result<Child> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        // DETACHED_PROCESS and CREATE_NO_WINDOW are mutually exclusive; detached wins.
        let base = (flags & !CREATE_NO_WINDOW) | DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;
        cmd.creation_flags(base | CREATE_BREAKAWAY_FROM_JOB);
        match cmd.spawn() {
            Ok(c) => Ok(c),
            // A job without JOB_OBJECT_LIMIT_BREAKAWAY_OK refuses the flag.
            Err(_) => {
                cmd.creation_flags(base);
                cmd.spawn()
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = flags;
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        cmd.spawn()
    }
}

/// Bounds for [`run`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunLimits {
    pub timeout: Duration,
    /// Bytes kept per stream; passing it kills the tree.
    pub max_output_bytes: usize,
}

/// What [`run`] produced and how it ended.
#[derive(Debug)]
pub struct RunOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub timed_out: bool,
    /// A stream passed [`RunLimits::max_output_bytes`].
    pub flooded: bool,
    pub cancelled: bool,
    /// The job still holding processes the command left running (background `&` children).
    /// Dropping it kills them.
    pub leftover: Option<JobObject>,
}

impl RunOutput {
    /// The exit code, `-1` when there is none.
    pub fn code(&self) -> i32 {
        self.status.code().unwrap_or(-1)
    }
}

const POLL: Duration = Duration::from_millis(10);
const READER_GRACE: Duration = Duration::from_millis(250);

/// Spawns `cmd` under `spec` with stdin null and both pipes read under a cap, waits under the
/// deadline, and kills the whole tree on timeout, flood or `cancelled()`. If the command exits on
/// its own while processes it started still run, their job comes back in
/// [`RunOutput::leftover`].
pub fn run(
    mut cmd: Command,
    spec: &ChildSpec,
    limits: RunLimits,
    cancelled: &dyn Fn() -> bool,
) -> Result<RunOutput, OsError> {
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut proc = spawn(&mut cmd, spec)?;

    let over_cap = Arc::new(AtomicBool::new(false));
    let (tx, rx) = std::sync::mpsc::channel::<(bool, Vec<u8>)>();
    if let Some(p) = proc.child_mut().stdout.take() {
        spawn_reader(p, true, limits.max_output_bytes, &over_cap, &tx);
    }
    if let Some(p) = proc.child_mut().stderr.take() {
        spawn_reader(p, false, limits.max_output_bytes, &over_cap, &tx);
    }
    drop(tx);

    let started = Instant::now();
    let (mut timed_out, mut flooded, mut was_cancelled) = (false, false, false);
    let status = loop {
        if let Some(status) = proc.child_mut().try_wait()? {
            break status;
        }
        if over_cap.load(Ordering::Relaxed) {
            flooded = true;
        } else if cancelled() {
            was_cancelled = true;
        } else if started.elapsed() >= limits.timeout {
            timed_out = true;
        } else {
            std::thread::sleep(POLL);
            continue;
        }
        proc.kill_tree();
        break proc.child_mut().wait()?;
    };

    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    for _ in 0..2 {
        match rx.recv_timeout(READER_GRACE) {
            Ok((true, b)) => stdout = b,
            Ok((false, b)) => stderr = b,
            Err(_) => break,
        }
    }
    let killed = timed_out || flooded || was_cancelled;
    Ok(RunOutput {
        status,
        stdout,
        stderr,
        timed_out,
        flooded: flooded || over_cap.load(Ordering::Relaxed),
        cancelled: was_cancelled,
        leftover: if killed { None } else { proc.into_leftover() },
    })
}

fn spawn_reader(
    pipe: impl Read + Send + 'static,
    is_stdout: bool,
    cap: usize,
    flag: &Arc<AtomicBool>,
    tx: &std::sync::mpsc::Sender<(bool, Vec<u8>)>,
) {
    let flag = Arc::clone(flag);
    let tx = tx.clone();
    std::thread::spawn(move || {
        let buf = drain_capped(pipe, cap, &flag);
        let _ = tx.send((is_stdout, buf));
    });
}

/// Reads to EOF, keeping at most `cap` bytes and setting `flag` when the cap is passed.
fn drain_capped(mut r: impl Read, cap: usize, flag: &AtomicBool) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
    loop {
        match r.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let room = cap.saturating_sub(buf.len());
                buf.extend_from_slice(&chunk[..n.min(room)]);
                if n > room {
                    flag.store(true, Ordering::Relaxed);
                }
            }
        }
    }
    buf
}

/// Tree kill without a job: `taskkill /T` (Windows) or the process group (unix). The killer is
/// a system tool given no environment beyond the scrubbed parent's.
fn legacy_kill_tree(child: &mut Child) {
    let pid = child.id().to_string();
    #[cfg(windows)]
    let killer = Some(("taskkill", vec!["/PID", pid.as_str(), "/T", "/F"]));
    #[cfg(unix)]
    let group = format!("-{pid}");
    #[cfg(unix)]
    let killer = Some(("kill", vec!["-KILL", group.as_str()]));
    #[cfg(not(any(windows, unix)))]
    let killer: Option<(&str, Vec<&str>)> = None;
    if let Some((program, args)) = killer {
        let mut cmd = Command::new(program);
        cmd.args(args.iter().map(OsStr::new)).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        for name in SCRUBBED_ENV {
            cmd.env_remove(name);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        let _ = cmd.status();
    }
    let _ = child.kill();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_scrub_list_is_the_credential_list_and_allow_reopens_one_name() {
        let spec = ChildSpec::new();
        assert_eq!(spec.scrubbed().count(), SCRUBBED_ENV.len());
        let spec = ChildSpec::new().with_env_allow(["hf_token"]);
        assert!(spec.scrubbed().all(|n| n != "HF_TOKEN"), "allow is case-insensitive");
        assert_eq!(spec.scrubbed().count(), SCRUBBED_ENV.len() - 1);
    }

    #[test]
    fn a_scrubbed_name_cannot_be_reinjected_through_env() {
        let spec = ChildSpec::new().with_env([("OPENROUTER_API_KEY".to_string(), "sk".to_string())]);
        let mut cmd = Command::new("x");
        assert!(matches!(spec.apply(&mut cmd), Err(OsError::EnvNotAllowed(n)) if n == "OPENROUTER_API_KEY"));
        let spec = spec.with_env_allow(["OPENROUTER_API_KEY"]);
        assert!(spec.apply(&mut cmd).is_ok());
    }

    #[test]
    fn apply_removes_every_scrubbed_name_from_the_command() {
        let mut cmd = Command::new("x");
        cmd.env("ANTHROPIC_API_KEY", "sk-ant").env("PATHLIKE", "keep");
        ChildSpec::new().apply(&mut cmd).unwrap();
        let envs: Vec<(String, Option<String>)> = cmd
            .get_envs()
            .map(|(k, v)| (k.to_string_lossy().into_owned(), v.map(|v| v.to_string_lossy().into_owned())))
            .collect();
        for name in SCRUBBED_ENV {
            assert!(envs.contains(&(name.to_string(), None)), "{name} is removed");
        }
        assert!(envs.contains(&("PATHLIKE".to_string(), Some("keep".to_string()))));
    }

    #[cfg(windows)]
    #[test]
    fn a_run_child_does_not_see_a_key_the_parent_has() {
        // The variable is set on the command (as if inherited); the scrub must still win.
        let mut cmd = Command::new("cmd.exe");
        cmd.args(["/d", "/c", "echo [%OPENROUTER_API_KEY%]"]).env("OPENROUTER_API_KEY", "sk-canary");
        let limits = RunLimits { timeout: Duration::from_secs(20), max_output_bytes: 4096 };
        let out = run(cmd, &ChildSpec::new(), limits, &|| false).unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(!text.contains("sk-canary"), "{text}");
        assert_eq!(out.code(), 0);
    }
}
