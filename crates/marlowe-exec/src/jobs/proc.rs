//! Starting a job's process: on this machine through `marlowe_os::child` (scrubbed environment,
//! kill-on-close Job Object, leftovers), anywhere else through the workspace port's `spawn`.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::thread::JoinHandle;

use marlowe_os::child::{self, ChildProc};
use marlowe_os::workspace::{ExecHandle, ExecRequest, Workspace};
use marlowe_os::JobObject;

/// A running process, local or behind the port.
pub(crate) enum Proc {
    Local(ChildProc),
    Port(Box<dyn ExecHandle>),
}

/// Where the bytes a process writes go. Called from the reader threads, in order per stream.
pub(crate) type OnChunk = Arc<dyn Fn(&[u8]) + Send + Sync>;

pub(crate) struct Started {
    pub proc: Proc,
    /// The reader threads (empty for a port, which the monitor polls).
    pub pumps: Vec<JoinHandle<()>>,
}

fn io_err(e: marlowe_os::OsError) -> std::io::Error {
    match e {
        marlowe_os::OsError::Io(io) => io,
        other => std::io::Error::other(other.to_string()),
    }
}

/// Starts `req` with stdin closed and both output streams delivered to `on_chunk`.
pub(crate) fn start(port: &dyn Workspace, req: &ExecRequest, on_chunk: OnChunk) -> std::io::Result<Started> {
    if port.kind() != "local" {
        return Ok(Started { proc: Proc::Port(port.spawn(req)?), pumps: Vec::new() });
    }
    let mut cmd = Command::new(&req.program);
    cmd.args(&req.args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut proc = child::spawn(&mut cmd, &req.spec).map_err(io_err)?;
    let mut pumps = Vec::new();
    if let Some(p) = proc.child_mut().stdout.take() {
        pumps.push(pump(p, Arc::clone(&on_chunk)));
    }
    if let Some(p) = proc.child_mut().stderr.take() {
        pumps.push(pump(p, on_chunk));
    }
    Ok(Started { proc: Proc::Local(proc), pumps })
}

fn pump(mut pipe: impl Read + Send + 'static, on_chunk: OnChunk) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut buf = [0u8; 16 * 1024];
        while let Ok(n) = pipe.read(&mut buf) {
            if n == 0 {
                break;
            }
            on_chunk(&buf[..n]);
        }
    })
}

impl Proc {
    /// The exit code once the process has exited (`-1` when it has none).
    pub fn try_wait(&mut self) -> std::io::Result<Option<i32>> {
        match self {
            Proc::Local(p) => Ok(p.child_mut().try_wait()?.map(|s| s.code().unwrap_or(-1))),
            Proc::Port(h) => h.try_wait(),
        }
    }

    /// New output from a port's process since `*offset` (a local process pushes through its pumps).
    pub fn drain(&mut self, offset: &mut u64) -> Vec<u8> {
        match self {
            Proc::Local(_) => Vec::new(),
            Proc::Port(h) => {
                let (bytes, next) = h.read_since(*offset);
                *offset = next;
                bytes
            }
        }
    }

    /// Kills the process and everything it started, and reaps it.
    pub fn kill_tree(&mut self) {
        match self {
            Proc::Local(p) => {
                p.kill_tree();
                let _ = p.child_mut().wait();
            }
            Proc::Port(h) => {
                let _ = h.kill_tree();
            }
        }
    }

    /// The Job Object, if processes the command started are still running in it.
    pub fn into_leftover(self) -> Option<JobObject> {
        match self {
            Proc::Local(p) => p.into_leftover(),
            Proc::Port(_) => None,
        }
    }
}
