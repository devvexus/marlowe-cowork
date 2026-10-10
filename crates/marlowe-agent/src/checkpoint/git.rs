//! Running `git` against the shadow repository, through [`marlowe_os::child`] (scrubbed
//! environment, Job Object, bounded output).

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use marlowe_os::child::{run, spawn, ChildSpec, RunLimits};

use super::CheckpointError;

/// Variables that would point git at some other repository or index than the shadow one.
const GIT_STEERING: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_NAMESPACE",
    "GIT_PREFIX",
];

/// The longest a single git command may run (an `add -A` over a very large tree).
const TIMEOUT: Duration = Duration::from_secs(600);

/// Most bytes kept of one command's output (a `diff` of a huge change is cut, not buffered whole).
const MAX_OUTPUT: usize = 64 * 1024 * 1024;

/// Transient failures git reports when another process holds the index or a ref.
const LOCKED: &[&str] = &["index.lock", ".lock': File exists", "Unable to create", "cannot lock ref"];

/// What a git command printed.
pub(crate) struct GitOut {
    pub stdout: Vec<u8>,
    pub stderr: String,
    pub code: i32,
    pub truncated: bool,
}

impl GitOut {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    pub fn ok(&self) -> bool {
        self.code == 0
    }
}

/// One git invocation: arguments, the directory it runs in, and extra environment.
pub(crate) struct Git<'a> {
    pub git_dir: &'a Path,
    pub work_tree: &'a Path,
}

impl Git<'_> {
    /// Runs `git --git-dir G --work-tree W --literal-pathspecs <args>` in the work tree, retrying
    /// a few times while another process holds a lock. A non-zero exit is returned, not an error:
    /// callers decide (`rev-parse --verify` failing is an answer).
    pub fn run(&self, args: &[&str], env: &[(&str, &str)]) -> Result<GitOut, CheckpointError> {
        let mut out = self.once(args, env)?;
        for attempt in 0..5u64 {
            if out.ok() || !LOCKED.iter().any(|l| out.stderr.contains(l)) {
                break;
            }
            std::thread::sleep(Duration::from_millis(60 * (attempt + 1)));
            out = self.once(args, env)?;
        }
        Ok(out)
    }

    /// Like [`Git::run`], but a non-zero exit is an error carrying git's message.
    pub fn must(&self, args: &[&str], env: &[(&str, &str)]) -> Result<GitOut, CheckpointError> {
        let out = self.run(args, env)?;
        if out.ok() {
            Ok(out)
        } else {
            Err(CheckpointError::Git { args: args.first().copied().unwrap_or("").to_string(), message: out.stderr.trim().to_string() })
        }
    }

    /// Like [`Git::run`] with `input` on the command's stdin (`check-ignore --stdin`).
    pub fn run_stdin(&self, args: &[&str], input: &[u8]) -> Result<GitOut, CheckpointError> {
        let mut cmd = self.command(args);
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut proc = spawn(&mut cmd, &self.spec(&[])).map_err(|e| CheckpointError::Spawn(e.to_string()))?;
        let stdin = proc.child_mut().stdin.take();
        let stdout = proc.child_mut().stdout.take();
        let stderr = proc.child_mut().stderr.take();
        let (out, err) = std::thread::scope(|s| {
            s.spawn(move || {
                if let Some(mut w) = stdin {
                    let _ = w.write_all(input);
                }
            });
            let o = s.spawn(move || {
                let mut b = Vec::new();
                if let Some(mut r) = stdout {
                    let _ = r.read_to_end(&mut b);
                }
                b
            });
            let e = s.spawn(move || {
                let mut b = Vec::new();
                if let Some(mut r) = stderr {
                    let _ = r.read_to_end(&mut b);
                }
                b
            });
            (o.join().unwrap_or_default(), e.join().unwrap_or_default())
        });
        let code = proc.child_mut().wait().map(|s| s.code().unwrap_or(-1)).unwrap_or(-1);
        Ok(GitOut { stdout: out, stderr: String::from_utf8_lossy(&err).into_owned(), code, truncated: false })
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new("git");
        cmd.arg("--git-dir").arg(self.git_dir).arg("--work-tree").arg(self.work_tree);
        // `check-ignore` takes paths, and refuses any pathspec magic, `literal` included.
        if args.first() != Some(&"check-ignore") {
            cmd.arg("--literal-pathspecs");
        }
        cmd.args(args);
        cmd
    }

    fn spec(&self, env: &[(&str, &str)]) -> ChildSpec {
        ChildSpec::new()
            .with_cwd(self.work_tree)
            .with_env_remove(GIT_STEERING.iter().map(|s| s.to_string()))
            .with_env(
                [("GIT_TERMINAL_PROMPT", "0"), ("GIT_OPTIONAL_LOCKS", "0"), ("GIT_EDITOR", "true")]
                    .into_iter()
                    .chain(env.iter().copied())
                    .map(|(k, v)| (k.to_string(), v.to_string())),
            )
    }

    fn once(&self, args: &[&str], env: &[(&str, &str)]) -> Result<GitOut, CheckpointError> {
        let cmd = self.command(args);
        let spec = self.spec(env);
        let limits = RunLimits { timeout: TIMEOUT, max_output_bytes: MAX_OUTPUT };
        let r = run(cmd, &spec, limits, &|| false).map_err(|e| CheckpointError::Spawn(e.to_string()))?;
        if r.timed_out {
            return Err(CheckpointError::Git { args: args.first().copied().unwrap_or("").to_string(), message: "timed out".into() });
        }
        Ok(GitOut {
            code: r.code(),
            truncated: r.flooded,
            stderr: String::from_utf8_lossy(&r.stderr).into_owned(),
            stdout: r.stdout,
        })
    }
}

/// Runs `git <args>` with no repository (for `init`), in `cwd`.
pub(crate) fn bare(cwd: &Path, args: &[&str]) -> Result<GitOut, CheckpointError> {
    let mut cmd = Command::new("git");
    cmd.args(args);
    let spec = ChildSpec::new()
        .with_cwd(cwd)
        .with_env_remove(GIT_STEERING.iter().map(|s| s.to_string()))
        .with_env([("GIT_TERMINAL_PROMPT".to_string(), "0".to_string())]);
    let r = run(cmd, &spec, RunLimits { timeout: Duration::from_secs(60), max_output_bytes: 1 << 20 }, &|| false)
        .map_err(|e| CheckpointError::Spawn(e.to_string()))?;
    let out = GitOut { code: r.code(), truncated: r.flooded, stderr: String::from_utf8_lossy(&r.stderr).into_owned(), stdout: r.stdout };
    if out.ok() {
        Ok(out)
    } else {
        Err(CheckpointError::Git { args: args.first().copied().unwrap_or("").to_string(), message: out.stderr.trim().to_string() })
    }
}
