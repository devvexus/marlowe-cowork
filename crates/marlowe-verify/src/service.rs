//! Long-running services (dev servers, API processes) the model starts to verify against.
//!
//! A service is a background process on the host's workspace port, so it starts through
//! `marlowe_os::child` (scrubbed environment, its own kill-on-close Job Object) and dies with
//! the table: [`ServiceTable`] kills everything it holds when it is dropped, which is when the
//! host (session or crew run) ends. A start returns only when the service is ready:
//!
//! - `ready_log`: a regex over its output (named groups `port` and `url` are captured);
//! - `ready_port`: something accepts connections on that port of this machine;
//! - `ready_url`: a loopback URL answers with a status below 500 (or `ready_status`); `{port}`
//!   is the `port` group of `ready_log`.
//!
//! Every readiness condition given must hold. With none, the service must stay alive for a
//! moment, and the result says it was not checked. A service that exits first, or is not ready
//! in time, is killed and its output returned. A port that already answers before the start is
//! an error: it would make the check pass against someone else's server.

use std::collections::BTreeMap;
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use marlowe_os::workspace::ExecHandle;
use regex::Regex;

use crate::cmd::Cmd;
use crate::env::Env;
use crate::http::{self, StatusSpec, Url};

/// Default wait for readiness.
pub const DEFAULT_READY_TIMEOUT: Duration = Duration::from_secs(60);
const KEPT_OUTPUT: usize = 1 << 20;

/// What makes a service ready.
#[derive(Debug, Clone, Default)]
pub struct Ready {
    pub url: Option<String>,
    pub port: Option<u16>,
    pub log: Option<String>,
    pub status: Option<StatusSpec>,
    pub timeout: Option<Duration>,
}

/// Everything a start needs.
#[derive(Debug, Clone)]
pub struct StartSpec {
    pub name: String,
    pub cmd: Cmd,
    pub cwd: PathBuf,
    pub ready: Ready,
    pub env_allow: Vec<String>,
    pub env: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ServiceError {
    /// The request was wrong; nothing was started.
    #[error("{0}")]
    Refused(String),
    /// The service did not come up (its output is in the message).
    #[error("{0}")]
    Failed(String),
}

/// A started service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Started {
    pub name: String,
    pub pid: Option<u32>,
    /// Where it answers, when known.
    pub url: Option<String>,
    /// What was checked.
    pub ready_by: Vec<String>,
    pub waited: Duration,
}

/// A service's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub name: String,
    pub pid: Option<u32>,
    pub running: bool,
    pub exit: Option<i32>,
    pub uptime: Duration,
    pub command: String,
    pub url: Option<String>,
}

impl Status {
    pub fn render(&self) -> String {
        let state = match (self.running, self.exit) {
            (true, _) => "running".to_string(),
            (false, Some(c)) => format!("exited {c}"),
            _ => "stopped".to_string(),
        };
        format!(
            "{} {state}{} (up {} s){}   `{}`",
            self.name,
            self.pid.map(|p| format!(" pid {p}")).unwrap_or_default(),
            self.uptime.as_secs(),
            self.url.as_ref().map(|u| format!(" at {u}")).unwrap_or_default(),
            self.command
        )
    }
}

struct Svc {
    handle: Box<dyn ExecHandle>,
    command: String,
    started: Instant,
    url: Option<String>,
    /// Ports this service is known to answer on.
    ports: Vec<u16>,
}

/// The services of one host.
#[derive(Default)]
pub struct ServiceTable {
    map: Mutex<BTreeMap<String, Svc>>,
}

impl std::fmt::Debug for ServiceTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServiceTable").field("services", &self.names()).finish()
    }
}

fn valid_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 40 && n.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// A port nothing is listening on right now.
pub fn free_port() -> Option<u16> {
    let l = std::net::TcpListener::bind("127.0.0.1:0").ok()?;
    l.local_addr().ok().map(|a| a.port())
}

/// Whether something accepts connections on `port` of this machine (IPv4 or IPv6 loopback).
pub fn port_open(port: u16) -> bool {
    let v4: SocketAddr = ([127, 0, 0, 1], port).into();
    let v6: SocketAddr = (std::net::Ipv6Addr::LOCALHOST, port).into();
    [v4, v6].iter().any(|a| TcpStream::connect_timeout(a, Duration::from_millis(300)).is_ok())
}

fn tail(text: &str, lines: usize, max: usize) -> String {
    let all: Vec<&str> = text.lines().collect();
    let from = all.len().saturating_sub(lines);
    let mut s = all[from..].join("\n");
    if s.len() > max {
        let mut cut = s.len() - max;
        while !s.is_char_boundary(cut) {
            cut += 1;
        }
        s = format!("...{}", &s[cut..]);
    }
    s
}

impl ServiceTable {
    pub fn names(&self) -> Vec<String> {
        self.map.lock().unwrap_or_else(|p| p.into_inner()).keys().cloned().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.map.lock().unwrap_or_else(|p| p.into_inner()).is_empty()
    }

    /// Starts `spec` and waits until it is ready.
    pub fn start(&self, env: &Env<'_>, mut spec: StartSpec) -> Result<Started, ServiceError> {
        if !valid_name(&spec.name) {
            return Err(ServiceError::Refused(format!("`{}` is not a service name (letters, digits, . _ -)", spec.name)));
        }
        if self.running(&spec.name) {
            return Err(ServiceError::Refused(format!("service `{}` is already running; stop it first or use restart", spec.name)));
        }
        // `{free_port}` is one port, picked now, for the command and the readiness URL alike.
        if spec.cmd.argv.iter().any(|a| a.contains("{free_port}")) || spec.ready.url.as_deref().is_some_and(|u| u.contains("{free_port}")) {
            let p = free_port().ok_or_else(|| ServiceError::Failed("could not find a free port".into()))?.to_string();
            spec.cmd.argv.iter_mut().for_each(|a| *a = a.replace("{free_port}", &p));
            spec.ready.url = spec.ready.url.map(|u| u.replace("{free_port}", &p));
        }
        let log_re = match &spec.ready.log {
            Some(p) => Some(Regex::new(p).map_err(|e| ServiceError::Refused(format!("ready_log is not a regex: {e}")))?),
            None => None,
        };
        let url_template = spec.ready.url.clone();
        let static_url = match url_template.as_deref().filter(|u| !u.contains("{port}")) {
            Some(u) => Some(Url::parse(u).map_err(|e| ServiceError::Refused(e.to_string()))?),
            None => None,
        };
        if let Some(u) = &static_url {
            if !u.is_loopback() || u.https {
                return Err(ServiceError::Refused(format!("ready_url {} must be an http://localhost URL", u.display())));
            }
        }
        let mut ports: Vec<u16> = spec.ready.port.into_iter().chain(static_url.as_ref().map(|u| u.port)).collect();
        ports.dedup();
        if let Some(p) = ports.into_iter().find(|p| port_open(*p)) {
            return Err(ServiceError::Refused(format!(
                "port {p} already answers before this service started; something else is using it, so a readiness check would pass against the wrong server. Stop that process or pick another port ({{free_port}} in the command and ready_url picks one)"
            )));
        }
        let mut req = env.request(&spec.cmd, &spec.cwd, Duration::from_secs(0), &spec.env_allow, &spec.env).map_err(ServiceError::Refused)?;
        req.max_output_bytes = KEPT_OUTPUT;
        let handle = env.ws.spawn(&req).map_err(|e| ServiceError::Failed(format!("could not start `{}`: {e}", spec.cmd.display())))?;
        self.await_ready(env, spec, handle, log_re, url_template, static_url)
    }

    fn await_ready(
        &self,
        env: &Env<'_>,
        spec: StartSpec,
        mut handle: Box<dyn ExecHandle>,
        log_re: Option<Regex>,
        url_template: Option<String>,
        static_url: Option<Url>,
    ) -> Result<Started, ServiceError> {
        let pid = handle.pid();
        let started = Instant::now();
        let timeout = spec.ready.timeout.unwrap_or(DEFAULT_READY_TIMEOUT);
        let mut captured: BTreeMap<String, String> = BTreeMap::new();
        let mut log_ok = log_re.is_none();
        let mut ready_by: Vec<String> = Vec::new();
        let no_check = log_re.is_none() && spec.ready.port.is_none() && url_template.is_none();
        let fail = |mut handle: Box<dyn ExecHandle>, why: String| -> ServiceError {
            let (bytes, _) = handle.read_since(0);
            let _ = handle.kill_tree();
            let out = String::from_utf8_lossy(&bytes).into_owned();
            ServiceError::Failed(format!("{why}\n--- output (last lines) ---\n{}", tail(&out, 40, 6000)))
        };
        let url = loop {
            if env.cancel.is_cancelled() {
                return Err(fail(handle, "cancelled while waiting for the service".into()));
            }
            if let Ok(Some(code)) = handle.try_wait() {
                return Err(fail(handle, format!("service `{}` exited with code {code} before it was ready", spec.name)));
            }
            if started.elapsed() >= timeout {
                let want = [
                    spec.ready.log.as_ref().map(|l| format!("log /{l}/")),
                    spec.ready.port.map(|p| format!("port {p}")),
                    url_template.as_ref().map(|u| format!("url {u}")),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" + ");
                return Err(fail(handle, format!("service `{}` was not ready within {} s (waiting for {want}); it was stopped", spec.name, timeout.as_secs())));
            }
            if no_check {
                if started.elapsed() >= Duration::from_millis(1500) {
                    ready_by.push("no readiness check given: it stayed alive for 1.5 s".into());
                    break None;
                }
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            if !log_ok {
                if let Some(re) = &log_re {
                    let (bytes, _) = handle.read_since(0);
                    let text = String::from_utf8_lossy(&bytes);
                    if let Some(c) = re.captures(&text) {
                        for name in re.capture_names().flatten() {
                            if let Some(m) = c.name(name) {
                                captured.insert(name.to_string(), m.as_str().to_string());
                            }
                        }
                        log_ok = true;
                    }
                }
            }
            if log_ok {
                let mut all = spec.ready.port.is_none_or(port_open);
                let url_now: Option<String> = match (&url_template, &static_url) {
                    (_, Some(u)) => Some(u.display()),
                    (Some(t), None) => captured.get("port").map(|p| t.replace("{port}", p)),
                    _ => None,
                };
                if all {
                    if let Some(u) = &url_now {
                        all = match http::Request::get(u) {
                            Ok(r) => match http::send(&http::Request { timeout: Duration::from_secs(2), ..r }, &|| false) {
                                Ok(resp) => spec.ready.status.as_ref().map_or(resp.status < 500, |s| s.accepts(resp.status)),
                                Err(_) => false,
                            },
                            Err(_) => false,
                        };
                    } else if url_template.is_some() {
                        all = false;
                    }
                }
                if all {
                    if let Some(l) = &spec.ready.log {
                        ready_by.push(format!("log matched /{l}/"));
                    }
                    if let Some(p) = spec.ready.port {
                        ready_by.push(format!("port {p} accepts connections"));
                    }
                    if let Some(u) = &url_now {
                        ready_by.push(format!("{u} answered"));
                    }
                    break url_now;
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        let url = url.or_else(|| captured.get("url").cloned());
        let waited = started.elapsed();
        let mut ports: Vec<u16> = spec.ready.port.into_iter().chain(captured.get("port").and_then(|p| p.parse().ok())).collect();
        ports.extend(url.as_deref().and_then(|u| Url::parse(u).ok()).map(|u| u.port));
        ports.sort_unstable();
        ports.dedup();
        self.map.lock().unwrap_or_else(|p| p.into_inner()).insert(
            spec.name.clone(),
            Svc { handle, command: spec.cmd.display(), started, url: url.clone(), ports },
        );
        Ok(Started { name: spec.name, pid, url, ready_by, waited })
    }

    fn running(&self, name: &str) -> bool {
        let mut g = self.map.lock().unwrap_or_else(|p| p.into_inner());
        match g.get_mut(name) {
            Some(s) => matches!(s.handle.try_wait(), Ok(None)),
            None => false,
        }
    }

    /// Whether a running service of this table answers on `port` (a request that changes state
    /// may go to it without asking; to any other port on this machine it asks).
    pub fn owns_port(&self, port: u16) -> bool {
        self.map.lock().unwrap_or_else(|p| p.into_inner()).values().any(|s| s.ports.contains(&port))
    }

    /// Kills `name` and forgets it. Returns the tail of its output, or `None` if it was unknown.
    pub fn stop(&self, name: &str) -> Option<String> {
        let mut svc = self.map.lock().unwrap_or_else(|p| p.into_inner()).remove(name)?;
        let (bytes, _) = svc.handle.read_since(0);
        let _ = svc.handle.kill_tree();
        Some(tail(&String::from_utf8_lossy(&bytes), 15, 2000))
    }

    /// Kills every service; returns their names.
    pub fn stop_all(&self) -> Vec<String> {
        let names = self.names();
        for n in &names {
            self.stop(n);
        }
        names
    }

    pub fn status(&self) -> Vec<Status> {
        let mut g = self.map.lock().unwrap_or_else(|p| p.into_inner());
        g.iter_mut()
            .map(|(name, s)| {
                let exit = s.handle.try_wait().ok().flatten();
                Status {
                    name: name.clone(),
                    pid: s.handle.pid(),
                    running: exit.is_none(),
                    exit,
                    uptime: s.started.elapsed(),
                    command: s.command.clone(),
                    url: s.url.clone(),
                }
            })
            .collect()
    }

    /// The last `lines` lines of `name`'s output.
    pub fn logs(&self, name: &str, lines: usize) -> Option<String> {
        let g = self.map.lock().unwrap_or_else(|p| p.into_inner());
        let s = g.get(name)?;
        let (bytes, _) = s.handle.read_since(0);
        Some(tail(&String::from_utf8_lossy(&bytes), lines.clamp(1, 500), 16_000))
    }
}

impl Drop for ServiceTable {
    fn drop(&mut self) {
        self.stop_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use marlowe_model::CancelToken;
    use marlowe_os::workspace::LocalFs;

    fn python() -> Option<String> {
        crate::cmd::resolve_program("python", None).map(|p| p.to_string_lossy().into_owned())
    }

    fn spec(py: &str, name: &str, code: &str, ready: Ready) -> StartSpec {
        StartSpec {
            name: name.into(),
            cmd: Cmd { argv: vec![py.to_string(), "-u".into(), "-c".into(), code.into()] },
            cwd: std::env::temp_dir(),
            ready,
            env_allow: Vec::new(),
            env: Vec::new(),
        }
    }

    const SERVER: &str = "import http.server, socketserver, sys\n\
        class H(http.server.BaseHTTPRequestHandler):\n\
        \x20   def do_GET(self):\n\
        \x20       self.send_response(200); self.end_headers(); self.wfile.write(b'hello')\n\
        \x20   def log_message(self, *a): pass\n\
        s = socketserver.TCPServer(('127.0.0.1', int(sys.argv[1]) if len(sys.argv) > 1 else 0), H)\n\
        print('listening on http://127.0.0.1:%d' % s.server_address[1], flush=True)\n\
        s.serve_forever()";

    #[test]
    fn a_service_is_ready_by_log_and_url_and_dies_with_the_table() {
        let Some(py) = python() else { return };
        let root = std::env::temp_dir();
        let (ws, cancel) = (LocalFs, CancelToken::new());
        let env = Env { ws: &ws, root: &root, pin: None, scrub: &[], child_env: &[], cancel: &cancel };
        let table = ServiceTable::default();
        let ready = Ready {
            log: Some(r"listening on (?P<url>http://127\.0\.0\.1:(?P<port>\d+))".into()),
            url: Some("http://127.0.0.1:{port}/".into()),
            timeout: Some(Duration::from_secs(30)),
            ..Ready::default()
        };
        let started = table.start(&env, spec(&py, "web", SERVER, ready)).unwrap();
        assert!(started.url.as_deref().unwrap().starts_with("http://127.0.0.1:"), "{started:?}");
        assert!(started.ready_by.iter().any(|r| r.contains("answered")), "{started:?}");
        let url = started.url.clone().unwrap();
        let r = http::send(&http::Request::get(&url).unwrap(), &|| false).unwrap();
        assert_eq!(r.text(), "hello");
        let st = table.status();
        assert!(st.len() == 1 && st[0].running && st[0].name == "web", "{st:?}");
        assert!(table.logs("web", 5).unwrap().contains("listening on"));
        assert!(matches!(table.start(&env, spec(&py, "web", SERVER, Ready::default())), Err(ServiceError::Refused(m)) if m.contains("already running")));
        let port = Url::parse(&url).unwrap().port;
        assert!(port_open(port));
        assert!(table.owns_port(port) && !table.owns_port(port.wrapping_add(1)), "a started service owns its port");
        drop(table);
        std::thread::sleep(Duration::from_millis(500));
        assert!(!port_open(port), "dropping the table killed the server");
    }

    #[test]
    fn a_busy_port_an_early_exit_and_a_timeout_are_reported_not_trusted() {
        let Some(py) = python() else { return };
        let root = std::env::temp_dir();
        let (ws, cancel) = (LocalFs, CancelToken::new());
        let env = Env { ws: &ws, root: &root, pin: None, scrub: &[], child_env: &[], cancel: &cancel };
        let table = ServiceTable::default();
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let busy = l.local_addr().unwrap().port();
        let err = table.start(&env, spec(&py, "x", "print(1)", Ready { port: Some(busy), ..Ready::default() })).unwrap_err();
        assert!(err.to_string().contains("already answers"), "{err}");
        drop(l);
        let err = table
            .start(&env, spec(&py, "boom", "print('starting'); raise SystemExit(3)", Ready { port: free_port(), timeout: Some(Duration::from_secs(20)), ..Ready::default() }))
            .unwrap_err();
        assert!(err.to_string().contains("exited with code 3") && err.to_string().contains("starting"), "{err}");
        let err = table
            .start(&env, spec(&py, "slow", "import time; time.sleep(60)", Ready { log: Some("never".into()), timeout: Some(Duration::from_secs(1)), ..Ready::default() }))
            .unwrap_err();
        assert!(err.to_string().contains("not ready within") && err.to_string().contains("stopped"), "{err}");
        assert!(table.is_empty(), "failed starts leave nothing behind");
        let none = table.start(&env, spec(&py, "idle", "import time; time.sleep(60)", Ready::default())).unwrap();
        assert!(none.ready_by[0].contains("no readiness check"));
        assert_eq!(table.stop_all(), ["idle"]);
    }

    #[test]
    fn free_port_in_the_command_and_url_is_one_port() {
        let Some(py) = python() else { return };
        let root = std::env::temp_dir();
        let (ws, cancel) = (LocalFs, CancelToken::new());
        let env = Env { ws: &ws, root: &root, pin: None, scrub: &[], child_env: &[], cancel: &cancel };
        let table = ServiceTable::default();
        let mut s = spec(&py, "web", SERVER, Ready { url: Some("http://127.0.0.1:{free_port}/".into()), timeout: Some(Duration::from_secs(30)), ..Ready::default() });
        s.cmd.argv.push("{free_port}".into());
        let started = table.start(&env, s).unwrap();
        assert!(started.url.unwrap().starts_with("http://127.0.0.1:"));
    }
}
