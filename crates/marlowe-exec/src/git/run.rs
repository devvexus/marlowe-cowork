//! Running `git` and `gh`: the host's view of how to start them, kept as a cheap clone so tools,
//! consequence checks and the loop middleware all run commands the same way.
//!
//! `git` goes through the host's workspace port (so a container host runs it inside the container)
//! with the host's credential scrub and extra child environment (the crew's no-push `GIT_CONFIG_*`
//! is how a crew run's `git push` fails). `gh` always runs on this machine, since GitHub's login
//! lives here, and gets `GH_TOKEN` and friends only through `env_allow`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use marlowe_os::workspace::{ExecRequest, LocalFs, Workspace};

use super::GitState;
use crate::{paths, text, BuiltinTools};

/// Read-only commands.
pub(crate) const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// Commands that write, run hooks or talk to a remote.
pub(crate) const WRITE_TIMEOUT: Duration = Duration::from_secs(120);
/// Bytes kept of one command's output stream.
const MAX_BYTES: usize = 4 * 1024 * 1024;
/// Bytes of command output handed to the model.
pub(crate) const MAX_RESULT_BYTES: usize = 40_000;

/// Credentials `gh` may be given by the user's environment.
const GH_ENV: &[&str] = &["GH_TOKEN", "GITHUB_TOKEN", "GH_ENTERPRISE_TOKEN", "GITHUB_ENTERPRISE_TOKEN"];

/// How a git command touches the world.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    /// Reads only: no optional locks.
    Read,
    /// Writes the repository (hooks may run).
    Write,
    /// Talks to a remote: GitHub's own tokens are allowed through for a credential helper.
    Net,
}

/// What one command produced.
#[derive(Debug, Clone, Default)]
pub(crate) struct Out {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    pub cancelled: bool,
    /// The program could not be started (not installed, not allowed).
    pub spawn_error: Option<String>,
}

impl Out {
    pub fn ok(&self) -> bool {
        self.spawn_error.is_none() && !self.timed_out && !self.cancelled && self.code == 0
    }

    /// Standard output, trimmed at the end.
    pub fn out(&self) -> &str {
        self.stdout.trim_end()
    }

    /// Everything the command said, stdout then stderr (git writes progress and push results to
    /// stderr).
    pub fn all(&self) -> String {
        let (o, e) = (self.stdout.trim_end(), self.stderr.trim_end());
        match (o.is_empty(), e.is_empty()) {
            (true, true) => String::new(),
            (false, true) => o.to_string(),
            (true, false) => e.to_string(),
            (false, false) => format!("{o}\n{e}"),
        }
    }

    /// Why it failed, for the model: the cause (spawn, timeout, interrupt) or the command's own
    /// words.
    pub fn failure(&self, what: &str) -> String {
        if let Some(e) = &self.spawn_error {
            return format!("could not run {what}: {e}");
        }
        if self.cancelled {
            return format!("{what} was stopped because the turn was interrupted");
        }
        if self.timed_out {
            return format!("{what} timed out; the output so far:\n{}", text::head_and_tail(&self.all(), MAX_RESULT_BYTES));
        }
        let said = self.all();
        if said.is_empty() {
            format!("{what} failed (exit {})", self.code)
        } else {
            format!("{what} failed (exit {}):\n{}", self.code, text::head_and_tail(&said, MAX_RESULT_BYTES))
        }
    }
}

/// How the host starts git and `gh`.
#[derive(Clone)]
pub(crate) struct Env {
    root: Option<PathBuf>,
    shell_dir: Option<PathBuf>,
    port: Arc<dyn Workspace>,
    scrub: Vec<String>,
    child_env: Vec<(String, String)>,
    pub(crate) state: Arc<GitState>,
}

impl Env {
    pub fn of(tools: &BuiltinTools) -> Env {
        Env {
            root: tools.root.clone(),
            shell_dir: tools.shell_dir.clone(),
            port: tools.port.clone(),
            scrub: tools.scrub.clone(),
            child_env: tools.child_env.clone(),
            state: tools.state::<GitState>(),
        }
    }

    /// Where a call runs: `cwd` (relative to the pinned dir, else the root, else the session's
    /// workspace), or that base itself. A sandbox's `cwd` must stay inside it.
    pub fn dir(&self, workspace: Option<&Path>, cwd: Option<&str>) -> Result<PathBuf, String> {
        let base = self.shell_dir.clone().or_else(|| self.root.clone()).or_else(|| workspace.map(Path::to_path_buf));
        let dir = match cwd.filter(|c| !c.trim().is_empty()) {
            Some(c) => paths::resolve(base.as_deref(), c),
            None => base.unwrap_or_else(|| paths::resolve(None, ".")),
        };
        if !dir.is_dir() {
            return Err(paths::not_a_directory(self.root.as_deref().or(workspace), &dir));
        }
        if let Some(pin) = self.shell_dir.as_deref().filter(|p| !paths::inside(p, &dir)) {
            return Err(format!("{} is outside the sandbox {}; git runs inside it", dir.display(), pin.display()));
        }
        Ok(dir)
    }

    /// Runs `git args` in `dir`.
    pub fn git(&self, dir: &Path, args: &[&str], timeout: Duration, mode: Mode, cancel: &dyn Fn() -> bool) -> Out {
        let mut full: Vec<&str> =
            vec!["--no-pager", "-c", "core.quotepath=false", "-c", "color.ui=never", "-c", "core.fsmonitor=false"];
        full.extend_from_slice(args);
        let mut req = ExecRequest::new("git", full.iter().copied());
        if mode == Mode::Net {
            req.spec.env_allow = GH_ENV.iter().map(|s| s.to_string()).collect();
        }
        req.spec.env_remove.extend(self.scrub.iter().filter(|n| mode != Mode::Net || !GH_ENV.contains(&n.as_str())).cloned());
        req.spec.env.extend(
            [
                ("GIT_TERMINAL_PROMPT", "0"),
                ("GCM_INTERACTIVE", "never"),
                ("GIT_PAGER", "cat"),
                ("GIT_EDITOR", "true"),
                ("GIT_SEQUENCE_EDITOR", "true"),
            ]
            .map(|(k, v)| (k.to_string(), v.to_string())),
        );
        if mode == Mode::Read {
            req.spec.env.push(("GIT_OPTIONAL_LOCKS".into(), "0".into()));
        }
        req.spec.env.extend(self.child_env.iter().cloned());
        req.spec.cwd = Some(dir.to_path_buf());
        req.timeout = timeout;
        req.max_output_bytes = MAX_BYTES;
        finish(self.port.exec(&req, cancel))
    }

    /// Runs `gh args` in `dir` on this machine, with the user's GitHub credentials allowed
    /// through and every prompt switched off.
    pub fn gh(&self, dir: &Path, args: &[&str], timeout: Duration, cancel: &dyn Fn() -> bool) -> Out {
        let Some(cmd) = self.state.gh.as_ref().filter(|c| !c.is_empty()) else {
            return Out { code: -1, spawn_error: Some("the GitHub CLI (`gh`) is not installed".into()), ..Out::default() };
        };
        let mut req = ExecRequest::new(cmd[0].clone(), cmd[1..].iter().cloned().chain(args.iter().map(|a| a.to_string())));
        req.spec.env_allow = GH_ENV.iter().map(|s| s.to_string()).collect();
        req.spec.env_remove.extend(self.scrub.iter().filter(|n| !GH_ENV.contains(&n.as_str())).cloned());
        req.spec.env.extend(
            [
                ("GH_PROMPT_DISABLED", "1"),
                ("NO_COLOR", "1"),
                ("GH_NO_UPDATE_NOTIFIER", "1"),
                ("GH_SPINNER_DISABLED", "1"),
                ("GIT_TERMINAL_PROMPT", "0"),
                ("GCM_INTERACTIVE", "never"),
                ("GH_PAGER", "cat"),
            ]
            .map(|(k, v)| (k.to_string(), v.to_string())),
        );
        req.spec.cwd = Some(dir.to_path_buf());
        req.timeout = timeout;
        req.max_output_bytes = MAX_BYTES;
        finish(LocalFs.exec(&req, cancel))
    }
}

fn finish(res: std::io::Result<marlowe_os::workspace::ExecOutput>) -> Out {
    match res {
        Err(e) => Out { code: -1, spawn_error: Some(e.to_string()), ..Out::default() },
        Ok(o) => Out {
            code: o.code,
            stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
            timed_out: o.timed_out,
            cancelled: o.cancelled,
            spawn_error: None,
        },
    }
}

/// Text longer than this goes to `gh` in a file, not on the command line (Windows caps a command
/// line near 32k characters).
pub(crate) const INLINE_MAX: usize = 6_000;

/// A file in the OS temp folder, removed when dropped.
pub(crate) struct TempFile(pub PathBuf);

impl TempFile {
    pub fn new(hint: &str, content: &str) -> std::io::Result<TempFile> {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        let path = std::env::temp_dir().join(format!("marlowe-{hint}-{}-{nanos}", std::process::id()));
        std::fs::write(&path, content)?;
        Ok(TempFile(path))
    }

    pub fn path(&self) -> String {
        self.0.to_string_lossy().into_owned()
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// `--body=text`, or `--body-file=<temp file>` for long text; the file lives as long as the guard.
pub(crate) fn body_arg(text: &str) -> Result<(String, Option<TempFile>), String> {
    if text.len() <= INLINE_MAX {
        return Ok((format!("--body={text}"), None));
    }
    let f = TempFile::new("body.md", text).map_err(|e| format!("could not stage the text: {e}"))?;
    Ok((format!("--body-file={}", f.path()), Some(f)))
}
