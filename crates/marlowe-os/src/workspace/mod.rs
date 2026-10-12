//! Where tools act: the [`Workspace`] port and the [`WorktreeProvider`] seam.
//!
//! [`Workspace`] is everything a file, search, shell or code tool needs from the machine it works
//! on: read, write, rename, walk, metadata, exec (run to completion) and spawn (a background
//! process behind an [`ExecHandle`]). [`LocalFs`] is this machine (through [`crate::child`]). Track K adds `workspace/docker.rs` (`DockerExec`), which runs the
//! same calls inside a container, so the eval's in-container tasks get the full tool surface.
//!
//! Where later tracks plug in:
//! - A (files), B (search, shells, jobs), C (code index, LSP fallbacks): take `&dyn Workspace`
//!   from the tool host (`marlowe_exec::BuiltinTools::workspace_port`) instead of calling
//!   `std::fs` or `std::process` directly. Paths are absolute in the workspace's own namespace.
//!   The shell tools already run through [`Workspace::exec`]; B's background jobs use
//!   [`Workspace::spawn`].
//! - K: implement [`Workspace`] for `DockerExec`; inject it with
//!   `BuiltinTools::with_workspace_port`.
//! - I: `Sandbox::create_child` implements [`WorktreeProvider`]; F's `agent{isolation:"worktree"}`
//!   and `enter_worktree` take an `Arc<dyn WorktreeProvider>` and never run `git worktree`
//!   themselves. [`NoWorktrees`] is the stand-in until then.

mod local;
pub mod docker;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

pub use local::LocalFs;

use crate::child::ChildSpec;
use crate::JobObject;

/// File metadata as the tools need it (A's `FileState` keys on size + mtime + content hash).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Meta {
    pub is_dir: bool,
    pub is_file: bool,
    pub len: u64,
    pub modified: Option<SystemTime>,
    pub readonly: bool,
}

/// One entry from [`Workspace::walk`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalkEntry {
    pub path: PathBuf,
    pub is_dir: bool,
    /// Depth below the walk root (its direct children are 1).
    pub depth: usize,
}

/// Bounds for [`Workspace::walk`]. `ignore`-style filtering (gitignore, build dirs) is B's to add
/// on top; the port only walks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalkOpts {
    pub max_depth: Option<usize>,
    /// Stop after this many entries.
    pub max_entries: Option<usize>,
    /// Directory names never descended into (e.g. `.git`, `target`, `node_modules`).
    pub skip_dirs: Vec<String>,
}

impl Default for WalkOpts {
    fn default() -> Self {
        Self { max_depth: None, max_entries: Some(100_000), skip_dirs: Vec::new() }
    }
}

/// A command for [`Workspace::exec`]: program + args, never a shell string (the caller picks the
/// shell and passes it as the program).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecRequest {
    pub program: String,
    pub args: Vec<String>,
    /// Environment, cwd and per-launch `env_allow` for the child.
    pub spec: ChildSpec,
    pub timeout: Duration,
    pub max_output_bytes: usize,
}

impl ExecRequest {
    pub fn new(program: impl Into<String>, args: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
            spec: ChildSpec::new(),
            timeout: Duration::from_secs(120),
            max_output_bytes: 1_048_576,
        }
    }

    /// The program, arguments, environment changes and cwd of a built `Command` (what the shell
    /// builders return), with default bounds.
    pub fn from_command(cmd: &Command) -> Self {
        let lossy = |s: &std::ffi::OsStr| s.to_string_lossy().into_owned();
        let mut req = Self::new(lossy(cmd.get_program()), cmd.get_args().map(lossy));
        for (k, v) in cmd.get_envs() {
            match v {
                Some(v) => req.spec.env.push((lossy(k), lossy(v))),
                None => req.spec.env_remove.push(lossy(k)),
            }
        }
        if let Some(dir) = cmd.get_current_dir() {
            req.spec.cwd = Some(dir.to_path_buf());
        }
        req
    }
}

/// What [`Workspace::exec`] produced.
#[derive(Debug)]
pub struct ExecOutput {
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub timed_out: bool,
    pub flooded: bool,
    pub cancelled: bool,
    /// The job still holding processes the command left running (background `&` children), on
    /// this machine. Dropping it kills them. Always `None` in a container.
    pub leftover: Option<JobObject>,
}

/// A process started by [`Workspace::spawn`]: B's background jobs (`run_in_background`, Ctrl+B
/// detach), on this machine or in K's container. Dropping the handle kills the process tree.
pub trait ExecHandle: Send {
    /// The process id where there is one (`None` in a container).
    fn pid(&self) -> Option<u32>;
    /// Output (stdout and stderr, interleaved as read) from byte offset `since`, and the offset
    /// to ask from next. Only the last `max_output_bytes` are kept; asking for bytes already
    /// dropped starts at the oldest kept one.
    fn read_since(&self, since: u64) -> (Vec<u8>, u64);
    /// The exit code once the process has exited (`-1` when there is none).
    fn try_wait(&mut self) -> std::io::Result<Option<i32>>;
    /// Kills the process and everything it started, and reaps it.
    fn kill_tree(&mut self) -> std::io::Result<()>;
}

/// The filesystem-and-exec port tools act through. Implementations may be called from several
/// tool threads at once.
pub trait Workspace: Send + Sync {
    /// A short name for logs and `/doctor` (`local`, `docker:<container>`).
    fn kind(&self) -> String;
    fn read(&self, path: &Path) -> std::io::Result<Vec<u8>>;
    /// Writes the whole file, creating parent directories. [`LocalFs`] writes a temp file beside
    /// it and renames it over, so a crash leaves the original.
    fn write(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()>;
    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()>;
    fn walk(&self, dir: &Path, opts: &WalkOpts) -> std::io::Result<Vec<WalkEntry>>;
    fn metadata(&self, path: &Path) -> std::io::Result<Meta>;
    /// Runs a command to completion under the request's bounds; `cancelled` is polled.
    fn exec(&self, req: &ExecRequest, cancelled: &dyn Fn() -> bool) -> std::io::Result<ExecOutput>;
    /// Starts a command in the background (stdin closed, output kept up to the request's
    /// `max_output_bytes`; `timeout` is not applied). The default refuses, for ports without
    /// background processes.
    fn spawn(&self, req: &ExecRequest) -> std::io::Result<Box<dyn ExecHandle>> {
        let _ = req;
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            format!("the {} workspace cannot run background processes", self.kind()),
        ))
    }
}

/// A request for a git worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeRequest {
    /// The repository (or worktree) to branch from.
    pub repo: PathBuf,
    /// A short name; the branch and directory derive from it.
    pub name: String,
    /// The ref to start from (`None`: the repo's current HEAD).
    pub base: Option<String>,
    /// For a child of an existing worktree (build_v2 stage 3), the parent's id.
    pub parent: Option<String>,
}

/// A worktree that exists on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeHandle {
    /// Stable id; [`WorktreeProvider::attach`] finds it again after a restart.
    pub id: String,
    pub root: PathBuf,
    pub branch: String,
    /// The commit it started from.
    pub base: String,
}

/// Why a worktree operation failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WorktreeError {
    #[error("worktrees are not available here: {0}")]
    Unavailable(String),
    #[error("no worktree {0}")]
    NotFound(String),
    #[error("{0}")]
    Failed(String),
}

/// Creates, re-attaches and removes git worktrees. Implemented by the crew's `Sandbox` (track I);
/// used by F (`agent{isolation:"worktree"}`, `enter_worktree`/`exit_worktree`) and I (build_v2
/// child worktrees).
pub trait WorktreeProvider: Send + Sync {
    fn create(&self, req: &WorktreeRequest) -> Result<WorktreeHandle, WorktreeError>;
    /// Finds a worktree created earlier (resume re-attaches; never re-creates).
    fn attach(&self, id: &str) -> Result<WorktreeHandle, WorktreeError>;
    /// Removes the directory; `keep_branch` keeps the branch (exit_worktree's `keep`).
    fn remove(&self, handle: &WorktreeHandle, keep_branch: bool) -> Result<(), WorktreeError>;
}

/// The provider until track I lands: every call says worktrees are unavailable.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoWorktrees;

impl WorktreeProvider for NoWorktrees {
    fn create(&self, _req: &WorktreeRequest) -> Result<WorktreeHandle, WorktreeError> {
        Err(WorktreeError::Unavailable("no worktree provider is installed".into()))
    }

    fn attach(&self, id: &str) -> Result<WorktreeHandle, WorktreeError> {
        Err(WorktreeError::NotFound(id.to_string()))
    }

    fn remove(&self, handle: &WorktreeHandle, _keep_branch: bool) -> Result<(), WorktreeError> {
        Err(WorktreeError::NotFound(handle.id.clone()))
    }
}
