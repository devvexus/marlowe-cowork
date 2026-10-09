//! Child processes: bash, powershell, python. Bounded in time and memory, killed as a tree.

use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use marlowe_model::CancelToken;

/// Default wall-clock limit for a shell call.
pub const SHELL_TIMEOUT_MS: u64 = 120_000;

/// Longest limit a call may request with `timeout_ms`.
pub const SHELL_TIMEOUT_MAX_MS: u64 = 600_000;

/// Output held per stream; a breach kills the child.
pub const MAX_SHELL_OUTPUT_BYTES: usize = 1_048_576;

/// Output returned to the model; longer output keeps its head and tail.
pub const MAX_SHELL_RESULT_BYTES: usize = 30_000;

const POLL: Duration = Duration::from_millis(10);
const READER_GRACE: Duration = Duration::from_millis(250);

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

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

/// Spawn `cmd`, read both pipes under a cap, wait under a deadline, and kill the whole process
/// tree on timeout, flood or cancel. stdin is null.
pub fn run_bounded(
    mut cmd: Command,
    limits: ShellLimits,
    cancel: Option<&CancelToken>,
) -> std::io::Result<ShellRun> {
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd.spawn()?;

    let over_cap = Arc::new(AtomicBool::new(false));
    let (tx, rx) = std::sync::mpsc::channel::<(bool, Vec<u8>)>();
    if let Some(p) = child.stdout.take() {
        spawn_reader(p, true, limits.max_output_bytes, &over_cap, &tx);
    }
    if let Some(p) = child.stderr.take() {
        spawn_reader(p, false, limits.max_output_bytes, &over_cap, &tx);
    }
    drop(tx);

    let started = Instant::now();
    let deadline = Duration::from_millis(limits.timeout_ms);
    let (mut timed_out, mut flooded, mut cancelled) = (false, false, false);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if over_cap.load(Ordering::Relaxed) {
            flooded = true;
        } else if cancel.is_some_and(CancelToken::is_cancelled) {
            cancelled = true;
        } else if started.elapsed() >= deadline {
            timed_out = true;
        } else {
            std::thread::sleep(POLL);
            continue;
        }
        kill_tree(&mut child);
        break child.wait()?;
    };

    let (mut out, mut err) = (Vec::new(), Vec::new());
    for _ in 0..2 {
        match rx.recv_timeout(READER_GRACE) {
            Ok((true, b)) => out = b,
            Ok((false, b)) => err = b,
            Err(_) => break,
        }
    }
    Ok(ShellRun {
        code: status.code().unwrap_or(-1),
        text: combine(&out, &err),
        timed_out,
        flooded: flooded || over_cap.load(Ordering::Relaxed),
        cancelled,
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

/// Read to EOF, keeping at most `cap` bytes and setting `flag` when the cap is passed.
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

/// Kill the child and everything it started.
fn kill_tree(child: &mut Child) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let _ = Command::new("taskkill")
            .args(["/PID", &child.id().to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .status();
    }
    #[cfg(unix)]
    {
        let _ = Command::new("kill")
            .args(["-KILL", &format!("-{}", child.id())])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
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
