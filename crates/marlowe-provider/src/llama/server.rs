//! Starting and watching the `llama-server` process. Behind a trait so the engine's tests run
//! against a fake process and a fake HTTP server.

use std::io;
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Stdio};

use marlowe_os::{ChildProc, ChildSpec, Containment};

use crate::ollama_store::LaunchPlan;

/// A started server process.
pub trait Process: Send {
    fn id(&self) -> u32;
    /// The exit code once the process has ended.
    fn exited(&mut self) -> Option<i32>;
    /// Kills the process and everything it started.
    fn kill(&mut self);
}

/// Starts server processes.
pub trait Spawner: Send + Sync {
    /// Starts `plan` with no window, its output appended to `log`.
    fn start(&self, plan: &LaunchPlan, log: Option<&Path>) -> io::Result<Box<dyn Process>>;
    /// A port nothing listens on.
    fn free_port(&self) -> io::Result<u16>;
}

/// The real machine: a hidden child in a kill-on-close Job Object, so the server dies with the
/// daemon whatever way the daemon ends, with the credential variables scrubbed from its
/// environment.
pub struct SystemSpawner {
    scrub: Vec<String>,
}

impl SystemSpawner {
    pub fn new(scrub: Vec<String>) -> Self {
        Self { scrub }
    }
}

struct Child {
    proc: ChildProc,
    done: Option<i32>,
}

impl Process for Child {
    fn id(&self) -> u32 {
        self.proc.id()
    }

    fn exited(&mut self) -> Option<i32> {
        if self.done.is_none() {
            if let Ok(Some(status)) = self.proc.child_mut().try_wait() {
                self.done = Some(status.code().unwrap_or(-1));
            }
        }
        self.done
    }

    fn kill(&mut self) {
        self.proc.kill_tree();
        let _ = self.proc.child_mut().wait();
        self.done.get_or_insert(-1);
    }
}

impl Spawner for SystemSpawner {
    fn start(&self, plan: &LaunchPlan, log: Option<&Path>) -> io::Result<Box<dyn Process>> {
        let mut cmd = Command::new(&plan.binary);
        cmd.args(&plan.args);
        // The directories the backend DLL's own imports live in, or ggml quietly runs on the CPU.
        cmd.env("PATH", plan.path_value());
        if let Some(dll) = &plan.backend_dll {
            cmd.env("GGML_BACKEND_PATH", dll);
        }
        cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        if let Some(out) = log.and_then(|p| crate::ollama_log::open_own_log(p, crate::ollama_log::OWN_LOG_CAP).ok()) {
            if let Ok(err) = out.try_clone() {
                cmd.stdout(out).stderr(err);
            }
        }
        let spec = ChildSpec::new().with_containment(Containment::Job).with_env_remove(self.scrub.iter().cloned());
        let proc = marlowe_os::child::spawn(&mut cmd, &spec).map_err(|e| io::Error::other(e.to_string()))?;
        Ok(Box::new(Child { proc, done: None }))
    }

    fn free_port(&self) -> io::Result<u16> {
        Ok(TcpListener::bind(("127.0.0.1", 0))?.local_addr()?.port())
    }
}

/// What a server's log says about where the model went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Offload {
    pub on_gpu: u32,
    pub total: u32,
}

/// The last `offloaded N/M layers to GPU` line of a llama-server log.
pub fn offloaded(log: &str) -> Option<Offload> {
    log.lines().rev().find_map(|l| {
        let rest = l.split("offloaded ").nth(1)?;
        let frac = rest.split_whitespace().next()?;
        let (n, m) = frac.split_once('/')?;
        Some(Offload { on_gpu: n.parse().ok()?, total: m.parse().ok()? })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_offload_line_is_read_from_the_log() {
        let log = "load_tensors: loading model tensors\nload_tensors: offloaded 53/53 layers to GPU\nsrv  init: ok\n";
        assert_eq!(offloaded(log), Some(Offload { on_gpu: 53, total: 53 }));
        assert_eq!(offloaded("load_tensors: offloaded 0/53 layers to GPU").map(|o| o.on_gpu), Some(0));
        assert_eq!(offloaded("nothing here"), None);
    }

    #[test]
    fn a_free_port_is_one_nothing_listens_on() {
        let p = SystemSpawner::new(Vec::new()).free_port().unwrap();
        assert!(TcpListener::bind(("127.0.0.1", p)).is_ok());
    }
}
