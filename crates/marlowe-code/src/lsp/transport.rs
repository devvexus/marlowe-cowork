//! Getting messages to and from a language server: [`Launch`] starts one and hands back a
//! [`Channel`] of `lsp_server` messages. [`StdioLauncher`] runs a real server as a child process
//! (through `marlowe_os::child`, so it is scrubbed and in its own kill-on-close Job Object) with a
//! reader thread, a writer thread and a stderr tail; the fake server in [`super::fake`] gives an
//! in-memory channel instead.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use crossbeam_channel::{Receiver, Sender};
use lsp_server::Message;
use marlowe_os::child::{self, ChildProc, ChildSpec};

use super::servers::ServerSpec;
use super::LspError;

/// Lines of a server's stderr kept for error messages.
const STDERR_TAIL: usize = 40;

/// Keeps a server process alive; dropping it kills the process tree.
pub trait ProcessGuard: Send {
    /// Whether the process is still running.
    fn alive(&mut self) -> bool;
}

/// A started server: messages out, messages in (closed when the server goes away), the process.
pub struct Channel {
    pub tx: Sender<Message>,
    pub rx: Receiver<Message>,
    pub guard: Option<Box<dyn ProcessGuard>>,
    /// The last lines the server wrote to stderr.
    pub stderr: Arc<Mutex<VecDeque<String>>>,
}

/// Starts language servers.
pub trait Launch: Send + Sync {
    /// Starts `spec` for `root`; `trusted` picks its command (see [`super::servers`]).
    fn launch(&self, spec: &ServerSpec, root: &Path, trusted: bool) -> Result<Channel, LspError>;
}

/// Real servers over stdio.
#[derive(Debug, Default, Clone, Copy)]
pub struct StdioLauncher;

struct ChildGuard(ChildProc);

impl ProcessGuard for ChildGuard {
    fn alive(&mut self) -> bool {
        matches!(self.0.child_mut().try_wait(), Ok(None))
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.0.kill_tree();
        let _ = self.0.child_mut().wait();
    }
}

impl Launch for StdioLauncher {
    fn launch(&self, spec: &ServerSpec, root: &Path, trusted: bool) -> Result<Channel, LspError> {
        let command = spec.launch_command(root, trusted).ok_or_else(|| LspError::NotInstalled(spec.id.to_string()))?;
        let program = PathBuf::from(&command[0]);
        let mut cmd = std::process::Command::new(&program);
        cmd.args(&command[1..]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        // The server's cwd is the workspace: without this, `cmd.exe` (npm's `.cmd` shims) and
        // Node look for a bare `node` / `python` there before `PATH`, so a planted
        // `<root>\node.exe` would run.
        let no_cwd_search = ("NoDefaultCurrentDirectoryInExePath".to_string(), "1".to_string());
        let launch = ChildSpec::new()
            .with_cwd(root)
            .with_env(std::iter::once(no_cwd_search).chain(spec.env.iter().map(|(k, v)| (k.to_string(), v.to_string()))))
            .with_creation_flags(child::CREATE_NO_WINDOW);
        let mut proc = child::spawn(&mut cmd, &launch)
            .map_err(|e| LspError::Start(format!("{} ({}): {e}", spec.id, program.display())))?;
        let c = proc.child_mut();
        let (Some(stdin), Some(stdout), Some(stderr)) = (c.stdin.take(), c.stdout.take(), c.stderr.take()) else {
            return Err(LspError::Start(format!("{}: no stdio pipes", spec.id)));
        };

        let (out_tx, out_rx) = crossbeam_channel::unbounded::<Message>();
        let (in_tx, in_rx) = crossbeam_channel::unbounded::<Message>();
        let name = spec.id;
        std::thread::Builder::new()
            .name(format!("lsp-write-{name}"))
            .spawn(move || {
                let mut w = std::io::BufWriter::new(stdin);
                for msg in out_rx {
                    if msg.write(&mut w).is_err() || w.flush().is_err() {
                        break;
                    }
                }
            })
            .map_err(|e| LspError::Start(e.to_string()))?;
        std::thread::Builder::new()
            .name(format!("lsp-read-{name}"))
            .spawn(move || read_loop(BufReader::new(stdout), &in_tx))
            .map_err(|e| LspError::Start(e.to_string()))?;
        let tail = Arc::new(Mutex::new(VecDeque::new()));
        let t = Arc::clone(&tail);
        std::thread::Builder::new()
            .name(format!("lsp-stderr-{name}"))
            .spawn(move || {
                for line in BufReader::new(stderr).lines() {
                    let Ok(line) = line else { break };
                    let mut q = t.lock().unwrap_or_else(|p| p.into_inner());
                    if q.len() == STDERR_TAIL {
                        q.pop_front();
                    }
                    q.push_back(line);
                }
            })
            .map_err(|e| LspError::Start(e.to_string()))?;
        Ok(Channel { tx: out_tx, rx: in_rx, guard: Some(Box::new(ChildGuard(proc))), stderr: tail })
    }
}

/// Reads framed messages until the stream ends. A body that is not a JSON-RPC message we know is
/// skipped (its frame was consumed); a broken frame ends the stream.
pub(crate) fn read_loop(mut r: impl BufRead, tx: &Sender<Message>) {
    loop {
        match Message::read(&mut r) {
            Ok(Some(msg)) => {
                if tx.send(msg).is_err() {
                    return;
                }
            }
            Ok(None) => return,
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData && e.to_string().starts_with("malformed LSP payload") => {
                continue;
            }
            Err(_) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reader_skips_unknown_bodies_and_stops_at_a_broken_frame() {
        let frame = |body: &str| format!("Content-Length: {}\r\n\r\n{body}", body.len());
        let stream = [
            frame(r#"{"jsonrpc":"2.0","id":1,"result":null}"#),
            frame(r#"{"jsonrpc":"2.0","surprise":true}"#),
            frame(r#"{"jsonrpc":"2.0","method":"window/logMessage","params":{"type":3,"message":"hi"}}"#),
            "garbage\n".to_string(),
            frame(r#"{"jsonrpc":"2.0","id":2,"result":1}"#),
        ]
        .concat();
        let (tx, rx) = crossbeam_channel::unbounded();
        read_loop(std::io::Cursor::new(stream.into_bytes()), &tx);
        drop(tx);
        let got: Vec<Message> = rx.iter().collect();
        assert_eq!(got.len(), 2, "{got:?}");
        assert!(matches!(&got[0], Message::Response(r) if r.response_result.as_ref().is_ok_and(|v| v.is_null())));
        assert!(matches!(&got[1], Message::Notification(n) if n.method == "window/logMessage"));
    }
}
