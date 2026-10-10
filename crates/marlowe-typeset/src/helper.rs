//! **The typesetter as a separate process**: `marlowe-typeset.exe`, shipped beside `marlowe.exe`.
//!
//! Typst is most of a typesetting system (about 36 MB of release binary), so it lives in its own
//! executable and `marlowe.exe` never links it. The image worker starts the helper on the first
//! formula it is asked for, and only once the terminal has said it can show Sixel. Each formula is
//! one JSON line out and one back ([`crate::proto`]).
//!
//! Failure is always the character grid, never a stuck placeholder:
//!
//! - A formula Typst refuses gets an error reply; the helper stays up.
//! - A helper that exits, writes something that is not a reply, or does not answer in time is
//!   killed, and that formula stays on the grid. The next formula starts a fresh helper, once.
//!   After a second death every formula stays on the grid for the rest of the session.
//! - A missing helper is a failure to start, and is counted the same way.
//!
//! The helper's environment is cleared (no API key reaches it) and its stderr is discarded, so
//! nothing it prints can land on the TUI.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::thread;
use std::time::Duration;

use crossbeam_channel::{unbounded, Receiver, RecvTimeoutError};

use crate::proto::{Request, Response};
use crate::service::{Caps, Image, Job, Renderer};

/// The helper's file name.
pub const HELPER_NAME: &str = if cfg!(windows) { "marlowe-typeset.exe" } else { "marlowe-typeset" };

/// Starts allowed per session: the first, and one restart.
pub const MAX_STARTS: u32 = 2;

/// The helper to run: `MARLOWE_TYPESET_HELPER` if set, else `marlowe-typeset.exe` beside the
/// running executable. `None` when there is none, and then images are off.
pub fn locate() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("MARLOWE_TYPESET_HELPER").filter(|p| !p.is_empty()) {
        let p = PathBuf::from(p);
        return p.is_file().then_some(p);
    }
    let exe = std::env::current_exe().ok()?;
    let p = exe.parent()?.join(HELPER_NAME);
    p.is_file().then_some(p)
}

/// A [`Renderer`] backed by the helper at `path`, started lazily on the first job.
pub fn renderer(path: PathBuf) -> Renderer {
    let mut h = Helper::new(path);
    Box::new(move |job, caps| h.render(job, caps))
}

struct Proc {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
    answered: u64,
}

/// The client end of one helper (and its one restart).
pub struct Helper {
    path: PathBuf,
    proc: Option<Proc>,
    starts: u32,
    next_id: u64,
    /// How long the first formula may take: the helper's start, Typst's library and the fonts.
    pub cold: Duration,
    /// How long any later formula may take.
    pub warm: Duration,
}

impl Helper {
    pub fn new(path: PathBuf) -> Helper {
        Helper {
            path,
            proc: None,
            starts: 0,
            next_id: 1,
            cold: Duration::from_secs(20),
            warm: Duration::from_secs(10),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// How many times a helper has been started.
    pub fn starts(&self) -> u32 {
        self.starts
    }

    /// The running helper's process id, if one is running.
    pub fn pid(&self) -> Option<u32> {
        self.proc.as_ref().map(|p| p.child.id())
    }

    /// Whether another formula could still be rendered.
    pub fn usable(&self) -> bool {
        self.proc.is_some() || self.starts < MAX_STARTS
    }

    /// Kills the running helper, as a death would. For tests of the restart.
    pub fn kill(&mut self) {
        if let Some(mut p) = self.proc.take() {
            let _ = p.child.kill();
            let _ = p.child.wait();
        }
    }

    /// One formula, rendered by the helper into its box.
    pub fn render(&mut self, job: &Job, caps: Caps) -> Result<Image, String> {
        if self.proc.is_none() {
            if self.starts >= MAX_STARTS {
                return Err("the typeset helper has stopped for this session".into());
            }
            self.starts += 1;
            self.proc = Some(spawn(&self.path)?);
        }
        let id = self.next_id;
        self.next_id += 1;
        let line = serde_json::to_string(&Request::new(id, job, caps)).map_err(|e| e.to_string())?;
        let p = self.proc.as_mut().expect("started above");
        let wait = if p.answered == 0 { self.cold } else { self.warm };
        match exchange(p, &line, id, wait) {
            Ok(resp) => {
                p.answered += 1;
                resp.image(job.rows)
            }
            Err(e) => {
                self.kill();
                Err(e)
            }
        }
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        self.kill();
    }
}

fn spawn(path: &Path) -> Result<Proc, String> {
    let mut cmd = Command::new(path);
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    // Nothing from this process's environment: no credential, and nothing it needs.
    cmd.env_clear();
    for keep in ["SystemRoot", "windir"] {
        if let Some(v) = std::env::var_os(keep) {
            cmd.env(keep, v);
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = cmd.spawn().map_err(|e| format!("cannot start {}: {e}", path.display()))?;
    let stdin = child.stdin.take().ok_or("no stdin")?;
    let stdout = child.stdout.take().ok_or("no stdout")?;
    let (tx, lines) = unbounded();
    thread::Builder::new()
        .name("marlowe-typeset-reader".into())
        .spawn(move || {
            let mut r = BufReader::new(stdout);
            loop {
                let mut s = String::new();
                match r.read_line(&mut s) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if tx.send(s).is_err() {
                            break;
                        }
                    }
                }
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(Proc { child, stdin, lines, answered: 0 })
}

fn exchange(p: &mut Proc, line: &str, id: u64, wait: Duration) -> Result<Response, String> {
    p.stdin
        .write_all(line.as_bytes())
        .and_then(|_| p.stdin.write_all(b"\n"))
        .and_then(|_| p.stdin.flush())
        .map_err(|e| format!("the helper is gone: {e}"))?;
    let got = match p.lines.recv_timeout(wait) {
        Ok(s) => s,
        Err(RecvTimeoutError::Timeout) => return Err(format!("no answer in {wait:?}")),
        Err(RecvTimeoutError::Disconnected) => return Err("the helper exited".into()),
    };
    let resp: Response = serde_json::from_str(got.trim_end()).map_err(|_| "not a reply".to_string())?;
    if resp.id != id {
        return Err(format!("reply {} to request {id}", resp.id));
    }
    Ok(resp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_helper_fails_twice_and_then_stops_trying() {
        let mut h = Helper::new(PathBuf::from("definitely-not-a-marlowe-typeset-helper.exe"));
        let job = Job { markup: "x".into(), fg: [1, 1, 1], cols: 2, rows: 1 };
        let caps = Caps { cell_w: 10, cell_h: 20, fg: [1, 1, 1], bg: [0, 0, 0] };
        assert!(h.render(&job, caps).is_err());
        assert!(h.usable(), "one restart is allowed");
        assert!(h.render(&job, caps).is_err());
        assert!(!h.usable());
        assert_eq!(h.starts(), MAX_STARTS);
        assert!(h.render(&job, caps).unwrap_err().contains("stopped"));
        assert_eq!(h.starts(), MAX_STARTS, "no third start");
    }
}
