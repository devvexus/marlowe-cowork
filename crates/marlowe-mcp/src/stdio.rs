//! The stdio transport: a child process speaking one JSON message per line on stdin/stdout. The
//! child starts through `marlowe_os::child` (scrubbed environment, kill-on-close Job Object).
//! Its stderr is drained into a bounded tail kept for error messages.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};

use crossbeam_channel::{unbounded, Receiver};
use marlowe_os::child::{self, ChildProc, ChildSpec};
use serde_json::Value;

use crate::rpc::Wire;
use crate::McpError;

/// Bytes of a server's stderr kept for diagnostics.
const STDERR_TAIL: usize = 4096;

/// How to start a stdio server.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StdioSpec {
    pub command: String,
    pub args: Vec<String>,
    /// Variables the config sets for the server.
    pub env: Vec<(String, String)>,
    /// Scrubbed names (API keys) the config lets through from the parent environment.
    pub env_allow: Vec<String>,
}

pub struct StdioWire {
    server: String,
    stdin: Mutex<Option<ChildStdin>>,
    proc: Mutex<Option<ChildProc>>,
    stderr: Arc<Mutex<VecDeque<u8>>>,
}

/// Starts the server. Returns the wire and the channel of messages the server writes.
pub fn start(server: &str, spec: &StdioSpec, cwd: Option<&Path>) -> Result<(StdioWire, Receiver<Value>), McpError> {
    let program = resolve_command(&spec.command);
    let mut cmd = Command::new(&program);
    cmd.args(&spec.args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut cs = ChildSpec::new().with_env(spec.env.clone());
    // A name the config sets itself is the user's own value, not one of Marlowe's keys.
    let own = spec.env.iter().map(|(k, _)| k.clone());
    cs = cs.with_env_allow(spec.env_allow.iter().cloned().chain(own));
    if let Some(dir) = cwd.filter(|d| d.is_dir()) {
        cs = cs.with_cwd(dir);
    }
    let fail = |detail: String| McpError::Spawn { server: server.to_string(), command: spec.command.clone(), detail };
    let mut proc = child::spawn(&mut cmd, &cs).map_err(|e| fail(e.to_string()))?;
    let c: &mut Child = proc.child_mut();
    let stdin = c.stdin.take().ok_or_else(|| fail("no stdin".into()))?;
    let stdout = c.stdout.take().ok_or_else(|| fail("no stdout".into()))?;
    let stderr = c.stderr.take();

    let tail = Arc::new(Mutex::new(VecDeque::new()));
    let (tx, rx) = unbounded::<Value>();
    std::thread::Builder::new()
        .name(format!("mcp-{server}-out"))
        .spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                // A line that is not JSON is a server printing to stdout; skip it.
                if let Ok(v) = serde_json::from_str::<Value>(line) {
                    if tx.send(v).is_err() {
                        break;
                    }
                }
            }
        })
        .ok();
    if let Some(mut err) = stderr {
        let tail = tail.clone();
        std::thread::Builder::new()
            .name(format!("mcp-{server}-err"))
            .spawn(move || {
                let mut buf = [0u8; 1024];
                while let Ok(n) = std::io::Read::read(&mut err, &mut buf) {
                    if n == 0 {
                        break;
                    }
                    let mut t = tail.lock().unwrap_or_else(|e| e.into_inner());
                    t.extend(&buf[..n]);
                    let over = t.len().saturating_sub(STDERR_TAIL);
                    t.drain(..over);
                }
            })
            .ok();
    }
    Ok((
        StdioWire { server: server.to_string(), stdin: Mutex::new(Some(stdin)), proc: Mutex::new(Some(proc)), stderr: tail },
        rx,
    ))
}

impl StdioWire {
    /// The last of the server's stderr, trimmed to printable text.
    pub fn stderr_tail(&self) -> String {
        let t = self.stderr.lock().unwrap_or_else(|e| e.into_inner());
        let bytes: Vec<u8> = t.iter().copied().collect();
        String::from_utf8_lossy(&bytes).trim().chars().filter(|c| !c.is_control() || *c == '\n').collect()
    }
}

impl Wire for StdioWire {
    fn send(&self, msg: &Value) -> Result<(), McpError> {
        let mut guard = self.stdin.lock().unwrap_or_else(|e| e.into_inner());
        let closed = || McpError::Closed { server: self.server.clone(), detail: "the server has exited".into() };
        let stdin = guard.as_mut().ok_or_else(closed)?;
        let line = format!("{msg}\n");
        stdin.write_all(line.as_bytes()).and_then(|()| stdin.flush()).map_err(|_| closed())
    }

    fn close(&self) {
        self.stdin.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(mut p) = self.proc.lock().unwrap_or_else(|e| e.into_inner()).take() {
            p.kill_tree();
            let _ = p.child_mut().wait();
        }
    }

    fn exit_note(&self) -> Option<String> {
        let mut g = self.proc.lock().unwrap_or_else(|e| e.into_inner());
        let status = match g.as_mut() {
            Some(p) => p.child_mut().try_wait().ok().flatten().map(|s| s.to_string()),
            None => Some("closed".into()),
        };
        let tail = self.stderr_tail();
        match (status, tail.is_empty()) {
            (Some(s), true) => Some(format!("the server exited ({s})")),
            (Some(s), false) => Some(format!("the server exited ({s}): {}", last_lines(&tail, 6))),
            (None, true) => None,
            (None, false) => Some(last_lines(&tail, 6)),
        }
    }
}

impl Drop for StdioWire {
    fn drop(&mut self) {
        self.close();
    }
}

fn last_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    lines[lines.len().saturating_sub(n)..].join(" | ")
}

/// `npx`, `uvx` and friends are `.cmd` shims on Windows, which `CreateProcess` will not find by
/// the bare name. Looks the command up on PATH with PATHEXT; anything with a path or an extension
/// is used as given.
pub fn resolve_command(command: &str) -> PathBuf {
    let p = Path::new(command);
    if !cfg!(windows) || p.extension().is_some() || command.contains(['/', '\\']) {
        return p.to_path_buf();
    }
    let Some(path) = std::env::var_os("PATH") else { return p.to_path_buf() };
    let exts = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
    for dir in std::env::split_paths(&path) {
        for ext in exts.split(';').filter(|e| !e.is_empty()) {
            let candidate = dir.join(format!("{command}{ext}"));
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    p.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_command_is_a_named_spawn_error() {
        let spec = StdioSpec { command: "definitely-not-an-executable-anywhere".into(), ..Default::default() };
        match start("ghost", &spec, None) {
            Err(McpError::Spawn { server, .. }) => assert_eq!(server, "ghost"),
            Err(e) => panic!("{e:?}"),
            Ok(_) => panic!("started a command that does not exist"),
        }
    }

    #[test]
    fn explicit_paths_and_extensions_are_not_searched() {
        assert_eq!(resolve_command("C:\\tools\\srv.exe"), PathBuf::from("C:\\tools\\srv.exe"));
        assert_eq!(resolve_command("srv.cmd"), PathBuf::from("srv.cmd"));
    }
}
