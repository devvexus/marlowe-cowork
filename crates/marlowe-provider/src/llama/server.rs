//! Starting and watching the `llama-server` process. Behind a trait so the engine's tests run
//! against a fake process and a fake HTTP server.

use std::io;
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, Weak};

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
    proc: Arc<Mutex<ChildProc>>,
    id: u32,
    done: Option<i32>,
}

/// The servers this process started and has not stopped. Weak: dropping a [`Child`] still kills
/// its job.
static OWNED: Mutex<Vec<Weak<Mutex<ChildProc>>>> = Mutex::new(Vec::new());

fn locked<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Whether `pid` is a server Marlowe started or something that server started (its job holds the
/// whole tree), whatever the binary is called and wherever it lives: the GPU gates must not read
/// Marlowe's own engine as another program's compute.
pub fn owns_process(pid: u32) -> bool {
    let live: Vec<Arc<Mutex<ChildProc>>> = locked(&OWNED).iter().filter_map(Weak::upgrade).collect();
    live.iter().any(|p| {
        let p = locked(p);
        p.id() == pid || p.job().is_some_and(|j| j.contains(pid))
    })
}

impl Process for Child {
    fn id(&self) -> u32 {
        self.id
    }

    fn exited(&mut self) -> Option<i32> {
        if self.done.is_none() {
            if let Ok(Some(status)) = locked(&self.proc).child_mut().try_wait() {
                self.done = Some(status.code().unwrap_or(-1));
            }
        }
        self.done
    }

    fn kill(&mut self) {
        {
            let mut p = locked(&self.proc);
            p.kill_tree();
            let _ = p.child_mut().wait();
        }
        self.done.get_or_insert(-1);
        locked(&OWNED).retain(|w| w.upgrade().is_some_and(|a| !Arc::ptr_eq(&a, &self.proc)));
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
        if let Some(out) = log.and_then(|p| crate::server_log::open_own_log(p, crate::server_log::OWN_LOG_CAP).ok()) {
            if let Ok(err) = out.try_clone() {
                cmd.stdout(out).stderr(err);
            }
        }
        let spec = ChildSpec::new().with_containment(Containment::Job).with_env_remove(self.scrub.iter().cloned());
        let proc = marlowe_os::child::spawn(&mut cmd, &spec).map_err(|e| io::Error::other(e.to_string()))?;
        let id = proc.id();
        let proc = Arc::new(Mutex::new(proc));
        {
            let mut owned = locked(&OWNED);
            owned.retain(|w| w.strong_count() > 0);
            owned.push(Arc::downgrade(&proc));
        }
        Ok(Box::new(Child { proc, id, done: None }))
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
    use std::time::{Duration, Instant};

    #[test]
    fn the_offload_line_is_read_from_the_log() {
        let log = "load_tensors: loading model tensors\nload_tensors: offloaded 53/53 layers to GPU\nsrv  init: ok\n";
        assert_eq!(offloaded(log), Some(Offload { on_gpu: 53, total: 53 }));
        assert_eq!(offloaded("load_tensors: offloaded 0/53 layers to GPU").map(|o| o.on_gpu), Some(0));
        assert_eq!(offloaded("nothing here"), None);
    }

    /// A fake llama-server: a shell that starts a child and waits on it, like a server with its
    /// worker processes. `None` where there is no shell to use.
    #[cfg(windows)]
    fn fake_server() -> Option<crate::ollama_store::LaunchPlan> {
        Some(crate::ollama_store::LaunchPlan {
            binary: std::path::PathBuf::from("cmd.exe"),
            args: ["/d", "/c", "ping -n 60 127.0.0.1 >nul"].map(String::from).to_vec(),
            backend_dll: None,
            path_prefix: Vec::new(),
        })
    }

    #[cfg(not(windows))]
    fn fake_server() -> Option<crate::ollama_store::LaunchPlan> {
        None
    }

    /// The server is Marlowe's own while it lives, by process tree and not by name, and gone from
    /// the registry (and the machine) once it was killed: nothing stray is left after a shutdown.
    #[test]
    fn a_started_server_and_its_children_are_owned_until_it_is_killed() {
        let Some(plan) = fake_server() else { return };
        let spawner = SystemSpawner::new(vec!["MARLOWE_TEST_SECRET".into()]);
        let mut proc = spawner.start(&plan, None).unwrap();
        let pid = proc.id();
        assert!(owns_process(pid));
        assert!(!owns_process(std::process::id()), "the daemon itself is not the server");
        // Everything the server started stays in its job.
        let deadline = Instant::now() + Duration::from_secs(10);
        let count = || {
            let mut n = 0;
            for g in locked(&OWNED).iter().filter_map(Weak::upgrade) {
                n = n.max(locked(&g).job().and_then(|j| j.active_processes().ok()).unwrap_or(0));
            }
            n
        };
        while count() < 2 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(count() >= 2, "the shell and the process it started are in the job");
        proc.kill();
        assert!(proc.exited().is_some());
        assert!(!owns_process(pid), "no longer owned once stopped");
        let ours: Vec<u32> = locked(&OWNED).iter().filter_map(Weak::upgrade).map(|p| locked(&p).id()).collect();
        assert!(!ours.contains(&pid));
        // Dropping the process without a kill stops the tree as well (the Job Object).
        let proc = spawner.start(&plan, None).unwrap();
        let held = locked(&OWNED).iter().filter_map(Weak::upgrade).find(|p| locked(p).id() == proc.id()).unwrap();
        drop(proc);
        assert_eq!(Arc::strong_count(&held), 1, "only the registry's own upgrade is left");
        drop(held);
    }

    #[test]
    fn a_free_port_is_one_nothing_listens_on() {
        let p = SystemSpawner::new(Vec::new()).free_port().unwrap();
        assert!(TcpListener::bind(("127.0.0.1", p)).is_ok());
    }
}
