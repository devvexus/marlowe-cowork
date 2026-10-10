//! Running `git` against the shadow repository, through [`marlowe_os::child`] (scrubbed
//! environment, Job Object, bounded output).

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use marlowe_model::CancelToken;
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
    /// The top of the tree git reads ignore files from: the workspace, or the user's repository
    /// root above it.
    pub work_tree: &'a Path,
    /// Where git runs, inside `work_tree`: paths in and out are relative to it, and it limits
    /// what `add` and `ls-files` see.
    pub cwd: &'a Path,
    /// Stops the command (and its process tree) when the turn is interrupted.
    pub cancel: Option<&'a CancelToken>,
}

impl Git<'_> {
    /// Runs `git --git-dir G --work-tree W --literal-pathspecs <args>` in `cwd`, retrying
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

    fn cancelled(&self) -> bool {
        self.cancel.is_some_and(|c| c.is_cancelled())
    }

    /// Runs a command whose stdout is a NUL-separated list and hands each item to `each` as it
    /// arrives, without buffering the output. `each` returning false stops the command early
    /// (`Ok(false)`); an interrupt stops it with [`CheckpointError::Cancelled`]. `Ok(true)` means
    /// the list ended and git exited cleanly.
    pub fn stream_nul(&self, args: &[&str], each: &mut dyn FnMut(&[u8]) -> bool) -> Result<bool, CheckpointError> {
        if self.cancelled() {
            return Err(CheckpointError::Cancelled);
        }
        let mut cmd = self.command(args);
        cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
        let mut proc = spawn(&mut cmd, &self.spec(&[])).map_err(|e| CheckpointError::Spawn(e.to_string()))?;
        let stdout = proc.child_mut().stdout.take();
        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let started = std::time::Instant::now();
        let mut pending: Vec<u8> = Vec::new();
        let mut outcome: Result<bool, CheckpointError> = Ok(true);
        std::thread::scope(|s| {
            s.spawn(move || {
                let Some(mut r) = stdout else { return };
                let mut buf = vec![0u8; 64 * 1024];
                while let Ok(n) = r.read(&mut buf) {
                    if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            });
            'read: loop {
                if self.cancelled() {
                    outcome = Err(CheckpointError::Cancelled);
                    break;
                }
                if started.elapsed() > TIMEOUT {
                    outcome = Err(CheckpointError::Git { args: args.first().copied().unwrap_or("").to_string(), message: "timed out".into() });
                    break;
                }
                match rx.recv_timeout(Duration::from_millis(50)) {
                    Ok(chunk) => {
                        pending.extend_from_slice(&chunk);
                        while let Some(at) = pending.iter().position(|b| *b == 0) {
                            let item: Vec<u8> = pending.drain(..=at).collect();
                            if !item[..at].is_empty() && !each(&item[..at]) {
                                outcome = Ok(false);
                                break 'read;
                            }
                        }
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            if !matches!(outcome, Ok(true)) {
                proc.kill_tree();
            }
        });
        let code = proc.child_mut().wait().map(|s| s.code().unwrap_or(-1)).unwrap_or(-1);
        match outcome {
            Ok(true) if code != 0 => Ok(false),
            other => other,
        }
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
            .with_cwd(self.cwd)
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
        if self.cancelled() {
            return Err(CheckpointError::Cancelled);
        }
        let r = run(cmd, &spec, limits, &|| self.cancelled()).map_err(|e| CheckpointError::Spawn(e.to_string()))?;
        if r.cancelled {
            return Err(CheckpointError::Cancelled);
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(name: &str, files: usize) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("marlowe-gitstream-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        bare(&dir, &["init", "-q"]).unwrap();
        for i in 0..files {
            std::fs::write(dir.join(format!("f{i}.txt")), b"x").unwrap();
        }
        dir
    }

    fn git_in<'a>(dir: &'a Path, git_dir: &'a Path, cancel: Option<&'a CancelToken>) -> Git<'a> {
        Git { git_dir, work_tree: dir, cwd: dir, cancel }
    }

    #[test]
    fn a_listing_is_read_as_it_arrives_and_can_stop_early() {
        let dir = repo("early", 5);
        let git_dir = dir.join(".git");
        let g = git_in(&dir, &git_dir, None);
        let mut seen = 0;
        assert!(g.stream_nul(&["ls-files", "-o", "-z"], &mut |_| { seen += 1; true }).unwrap());
        assert_eq!(seen, 5);
        let mut seen = 0;
        let finished = g.stream_nul(&["ls-files", "-o", "-z"], &mut |_| { seen += 1; seen < 2 }).unwrap();
        assert!(!finished && seen == 2, "stopped at the second item");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_interrupt_stops_a_listing_and_a_command() {
        let dir = repo("cancel", 2);
        let git_dir = dir.join(".git");
        let token = CancelToken::new();
        token.cancel();
        let g = git_in(&dir, &git_dir, Some(&token));
        assert!(matches!(g.stream_nul(&["ls-files", "-o", "-z"], &mut |_| true), Err(CheckpointError::Cancelled)));
        assert!(matches!(g.run(&["status"], &[]), Err(CheckpointError::Cancelled)));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
