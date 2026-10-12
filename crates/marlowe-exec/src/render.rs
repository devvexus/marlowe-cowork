//! Renders a JavaScript-assembled page through the **system Microsoft Edge** over the Chrome
//! DevTools Protocol (`headless_chrome`, synchronous, no tokio). Used only when static extraction
//! is thin and the page looks like an app shell (see `marlowe_extract::main_content`).
//!
//! - No bundled browser: if Edge is not installed the page is reported as unreadable.
//! - Edge runs headless in a fresh temporary profile, inside a kill-on-close Job Object, so a
//!   hung render or a crash never leaves processes behind; the profile is deleted afterwards.
//! - A hard timeout covers launch, navigation and settling. Downloads are denied.
//! - The browser identifies itself with our User-Agent. No stealth or fingerprint changes.
//! - Edge gets a cleared environment (no API key reaches it or its children).
//! - Every request the page makes is paused (CDP `Fetch`) and continued only to https hosts that
//!   resolve to public addresses; a navigation to another document also passes robots.txt and
//!   host spacing. The URL the render ends on is checked the same way. As on the static path, a
//!   page cannot make Marlowe read loopback or LAN services.
//!
//! [`EdgeBrowser`] (in `browse`) is the same Edge driven for the `browser` verification tool: console
//! messages, steps, snapshots and screenshots of the user's own pages.

use std::collections::HashMap;
use std::ffi::OsString;
use std::net::{IpAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use marlowe_model::CancelToken;
use marlowe_research::{HostGate, HttpGet, USER_AGENT};

mod browse;

pub use browse::EdgeBrowser;

/// Whole-render budget: launch, navigate, settle.
pub const RENDER_TIMEOUT: Duration = Duration::from_secs(30);
/// After navigation, how long the DOM may keep changing before it is read anyway.
pub const SETTLE: Duration = Duration::from_secs(4);
/// Browsers alive at once across the process.
pub const MAX_CONCURRENT: usize = 2;
/// Rendered HTML above this is cut (it is extracted, not stored whole).
const MAX_HTML_BYTES: usize = 16 * 1024 * 1024;

/// What the browser saw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    pub html: String,
    pub final_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RenderError {
    #[error("Microsoft Edge is not installed")]
    NoBrowser,
    #[error("the browser did not start: {0}")]
    Launch(String),
    #[error("rendering did not finish within {0:?}")]
    Timeout(Duration),
    #[error("cancelled")]
    Cancelled,
    #[error("navigation failed: {0}")]
    Navigation(String),
    /// The page ended on a URL the static path would not fetch (not https, or not public).
    #[error("the render ended on a URL outside the public web")]
    Refused,
}

impl RenderError {
    /// A constant naming the failure, safe to show a model.
    pub fn kind(&self) -> &'static str {
        match self {
            RenderError::NoBrowser => "no browser",
            RenderError::Launch(_) => "launch failed",
            RenderError::Timeout(_) => "timed out",
            RenderError::Cancelled => "cancelled",
            RenderError::Navigation(_) => "navigation failed",
            RenderError::Refused => "refused",
        }
    }
}

/// Renders a URL to HTML. The corpus holds one; tests substitute their own.
pub trait Renderer: Send + Sync {
    fn render(&self, url: &str, cancel: &CancelToken) -> Result<Rendered, RenderError>;
}

/// The installed Microsoft Edge, if any: `MARLOWE_EDGE` overrides the standard locations.
pub fn find_edge() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(p) = std::env::var_os("MARLOWE_EDGE") {
        candidates.push(PathBuf::from(p));
    }
    for var in ["ProgramFiles(x86)", "ProgramFiles", "LOCALAPPDATA"] {
        if let Some(base) = std::env::var_os(var) {
            candidates.push(Path::new(&base).join("Microsoft").join("Edge").join("Application").join("msedge.exe"));
        }
    }
    candidates.into_iter().find(|p| p.is_file())
}

/// Edge through CDP.
#[derive(Clone)]
pub struct EdgeRenderer {
    pub edge: PathBuf,
    pub timeout: Duration,
    pub settle: Duration,
    /// robots.txt and host spacing for documents the page navigates to.
    pub gate: HostGate,
    pub robots_http: Arc<dyn HttpGet>,
}

impl std::fmt::Debug for EdgeRenderer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EdgeRenderer").field("edge", &self.edge).field("timeout", &self.timeout).finish()
    }
}

impl EdgeRenderer {
    /// Edge at `edge` with default budgets and the process-wide host gate.
    pub fn at(edge: PathBuf) -> Self {
        Self {
            edge,
            timeout: RENDER_TIMEOUT,
            settle: SETTLE,
            gate: crate::corpus::shared_gate(),
            robots_http: Arc::new(marlowe_research::NetHttp::new()),
        }
    }

    /// The installed Edge with default budgets, or None.
    pub fn find() -> Option<Self> {
        find_edge().map(Self::at)
    }
}

/// Environment variables Edge keeps: where Windows and the profile live, nothing else. Every
/// other variable (API keys among them) is cleared.
const EDGE_ENV: [&str; 20] = [
    "SystemRoot",
    "SystemDrive",
    "windir",
    "TEMP",
    "TMP",
    "LOCALAPPDATA",
    "APPDATA",
    "USERPROFILE",
    "HOMEDRIVE",
    "HOMEPATH",
    "ProgramData",
    "ProgramFiles",
    "ProgramFiles(x86)",
    "ProgramW6432",
    "CommonProgramFiles",
    "CommonProgramFiles(x86)",
    "PATH",
    "PATHEXT",
    "NUMBER_OF_PROCESSORS",
    "PROCESSOR_ARCHITECTURE",
];

/// The variables of `env` Edge may see: [`EDGE_ENV`] only, and never a scrubbed key.
pub fn edge_env(env: impl Iterator<Item = (OsString, OsString)>) -> Vec<(OsString, OsString)> {
    let scrubbed = marlowe_brains::scrubbed_env();
    env.filter(|(k, _)| {
        let k = k.to_string_lossy();
        EDGE_ENV.iter().any(|e| e.eq_ignore_ascii_case(&k)) && !scrubbed.iter().any(|s| s.eq_ignore_ascii_case(&k))
    })
    .collect()
}

/// Whether the page may make a request to `url`: https to a host whose every address is public
/// (`resolve` gives them). `data:`, `blob:` and `about:` never leave the browser.
pub fn egress_allowed(url: &str, resolve: &dyn Fn(&str, u16) -> Vec<IpAddr>) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    if ["data:", "blob:", "about:"].iter().any(|p| lower.starts_with(p)) {
        return true;
    }
    let Ok(t) = marlowe_net::Target::parse(url) else { return false };
    let host = t.host.trim_start_matches('[').trim_end_matches(']');
    let addrs = match host.parse::<IpAddr>() {
        Ok(ip) => vec![ip],
        Err(_) => resolve(host, t.port),
    };
    !addrs.is_empty() && addrs.into_iter().all(marlowe_net::is_public)
}

/// The system resolver, for [`egress_allowed`].
fn resolve_system(host: &str, port: u16) -> Vec<IpAddr> {
    (host, port).to_socket_addrs().map(|a| a.map(|s| s.ip()).collect()).unwrap_or_default()
}

/// Decides the requests of one render: [`egress_allowed`] per host (cached), and robots.txt and
/// host spacing for every document other than the page itself.
struct Egress {
    page: String,
    gate: HostGate,
    robots_http: Arc<dyn HttpGet>,
    cancel: CancelToken,
    hosts: Mutex<HashMap<String, bool>>,
}

impl Egress {
    fn allow(&self, url: &str, document: bool) -> bool {
        // A local file the caller asked to render may load itself (the corpus only ever passes
        // https; a web page cannot reach file: in Edge either way).
        if is_file(&self.page) && is_file(url) && same_page(url, &self.page) {
            return true;
        }
        let host = marlowe_research::politeness::split_url(url).map(|(origin, _)| origin);
        let public = match &host {
            Some(h) => {
                let cached = self.hosts.lock().unwrap_or_else(|p| p.into_inner()).get(h).copied();
                cached.unwrap_or_else(|| {
                    let ok = egress_allowed(url, &resolve_system);
                    self.hosts.lock().unwrap_or_else(|p| p.into_inner()).insert(h.clone(), ok);
                    ok
                })
            }
            None => egress_allowed(url, &resolve_system),
        };
        if !public {
            return false;
        }
        // The page itself was admitted by the corpus; another document is a new fetch.
        !document || same_page(url, &self.page) || self.gate.admit(url, self.robots_http.as_ref(), &self.cancel).is_ok()
    }
}

fn is_file(url: &str) -> bool {
    url.get(..5).is_some_and(|p| p.eq_ignore_ascii_case("file:"))
}

fn same_page(a: &str, b: &str) -> bool {
    let strip = |u: &str| u.split('#').next().unwrap_or(u).trim_end_matches('/').to_ascii_lowercase();
    strip(a) == strip(b)
}

/// Bounds concurrent browsers process-wide.
struct Slots {
    used: Mutex<usize>,
    cv: Condvar,
}

static SLOTS: Slots = Slots { used: Mutex::new(0), cv: Condvar::new() };

struct SlotGuard;

impl Drop for SlotGuard {
    fn drop(&mut self) {
        let mut n = SLOTS.used.lock().unwrap_or_else(|p| p.into_inner());
        *n = n.saturating_sub(1);
        SLOTS.cv.notify_one();
    }
}

fn acquire(cancel: &CancelToken, deadline: Instant) -> Result<SlotGuard, RenderError> {
    let mut n = SLOTS.used.lock().unwrap_or_else(|p| p.into_inner());
    while *n >= MAX_CONCURRENT {
        if cancel.is_cancelled() {
            return Err(RenderError::Cancelled);
        }
        if Instant::now() >= deadline {
            return Err(RenderError::Timeout(Duration::ZERO));
        }
        n = SLOTS.cv.wait_timeout(n, Duration::from_millis(100)).unwrap_or_else(|p| p.into_inner()).0;
    }
    *n += 1;
    Ok(SlotGuard)
}

/// A fresh profile directory, removed on drop (retried: Edge releases files as it exits).
struct Profile(PathBuf);

impl Profile {
    fn new() -> Result<Self, RenderError> {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join("marlowe-render").join(format!(
            "{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).map_err(|e| RenderError::Launch(format!("profile directory: {e}")))?;
        Ok(Self(dir))
    }
}

impl Drop for Profile {
    fn drop(&mut self) {
        for _ in 0..20 {
            if std::fs::remove_dir_all(&self.0).is_ok() || !self.0.exists() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

impl Renderer for EdgeRenderer {
    fn render(&self, url: &str, cancel: &CancelToken) -> Result<Rendered, RenderError> {
        if !self.edge.is_file() {
            return Err(RenderError::NoBrowser);
        }
        let deadline = Instant::now() + self.timeout;
        let _slot = acquire(cancel, deadline)?;
        let profile = Profile::new()?;
        // Defence in depth beside the request interception: the names a page reaches local
        // services by do not resolve.
        let rules = "--host-resolver-rules=MAP localhost ~NOTFOUND, MAP *.localhost ~NOTFOUND, MAP *.local ~NOTFOUND".to_string();
        let mut child = spawn_edge(&self.edge, &profile, &[rules])?;
        let egress = Arc::new(Egress {
            page: url.to_string(),
            gate: self.gate.clone(),
            robots_http: self.robots_http.clone(),
            cancel: cancel.clone(),
            hosts: Mutex::new(HashMap::new()),
        });
        let result = drive(&profile.0, url, self.settle, deadline, cancel, egress);
        let _ = child.kill_tree();
        let _ = child.child_mut().wait();
        drop(profile);
        let r = result?;
        // Where the page ended is checked like any URL the static path would fetch.
        let stayed_local = is_file(url) && same_page(&r.final_url, url);
        if !stayed_local && !egress_allowed(&r.final_url, &resolve_system) {
            return Err(RenderError::Refused);
        }
        Ok(r)
    }
}

/// Starts headless Edge in `profile` (extra command-line flags in `extra`): scrubbed, windowless,
/// and in its own kill-on-close Job Object, never uncontained.
fn spawn_edge(edge: &Path, profile: &Profile, extra: &[String]) -> Result<marlowe_os::ChildProc, RenderError> {
    let mut cmd = std::process::Command::new(edge);
    cmd.args([
        "--headless=new",
        "--remote-debugging-port=0",
        "--no-first-run",
        "--no-default-browser-check",
        "--disable-extensions",
        "--disable-sync",
        "--disable-background-networking",
        "--disable-component-update",
        "--disable-default-apps",
        "--disable-features=msEdgeSidebarV2,msUndersideButton,Translate",
        "--mute-audio",
        "--disable-gpu",
        &format!("--user-agent={USER_AGENT}"),
    ])
    .args(extra)
    .arg(format!("--user-data-dir={}", profile.0.display()))
    .arg("about:blank")
    .stdin(std::process::Stdio::null())
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null());
    // A cleared environment: no API key reaches Edge or its children.
    cmd.env_clear().envs(edge_env(std::env::vars_os()));
    let spec = marlowe_os::ChildSpec::new().with_creation_flags(0).with_containment(marlowe_os::Containment::Job);
    marlowe_os::child::spawn(&mut cmd, &spec).map_err(|e| RenderError::Launch(e.to_string()))
}

/// Waits for Edge's DevToolsActivePort file and returns the browser websocket URL.
fn devtools_url(profile: &Path, deadline: Instant, cancel: &CancelToken) -> Result<String, RenderError> {
    let file = profile.join("DevToolsActivePort");
    loop {
        if let Ok(s) = std::fs::read_to_string(&file) {
            let mut lines = s.lines();
            if let (Some(port), Some(path)) = (lines.next(), lines.next()) {
                if port.trim().parse::<u16>().is_ok() && path.starts_with('/') {
                    return Ok(format!("ws://127.0.0.1:{}{}", port.trim(), path.trim()));
                }
            }
        }
        if cancel.is_cancelled() {
            return Err(RenderError::Cancelled);
        }
        if Instant::now() >= deadline {
            return Err(RenderError::Launch("no DevTools endpoint appeared".into()));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Connects, navigates and reads the DOM on a worker; the caller kills the browser on timeout.
fn drive(
    profile: &Path,
    url: &str,
    settle: Duration,
    deadline: Instant,
    cancel: &CancelToken,
    egress: Arc<Egress>,
) -> Result<Rendered, RenderError> {
    let ws = devtools_url(profile, deadline, cancel)?;
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let url = url.to_string();
    let budget = deadline.saturating_duration_since(Instant::now());
    std::thread::Builder::new()
        .name("marlowe-render".into())
        .spawn(move || {
            let _ = tx.send(navigate(&ws, &url, settle, budget, egress));
        })
        .map_err(|e| RenderError::Launch(e.to_string()))?;
    match crate::web::wait_for(&rx, cancel, deadline) {
        Ok(r) => r,
        Err(crate::web::Stopped::Cancelled) => Err(RenderError::Cancelled),
        Err(_) => Err(RenderError::Timeout(budget)),
    }
}

fn navigate(ws: &str, url: &str, settle: Duration, budget: Duration, egress: Arc<Egress>) -> Result<Rendered, RenderError> {
    use headless_chrome::browser::tab::RequestPausedDecision;
    use headless_chrome::browser::transport::{SessionId, Transport};
    use headless_chrome::protocol::cdp::Fetch::events::RequestPausedEvent;
    use headless_chrome::protocol::cdp::Fetch::FailRequest;
    use headless_chrome::protocol::cdp::Network::{ErrorReason, ResourceType};
    use headless_chrome::protocol::cdp::Page::{SetDownloadBehavior, SetDownloadBehaviorBehaviorOption};
    let browser = headless_chrome::Browser::connect_with_timeout(ws.to_string(), budget).map_err(nav)?;
    let tab = browser.new_tab().map_err(nav)?;
    let _ =
        tab.call_method(SetDownloadBehavior { behavior: SetDownloadBehaviorBehaviorOption::Deny, download_path: None });
    // Every request pauses here first; nothing reaches the network until `egress` allows it.
    let interceptor = move |_t: Arc<Transport>, _s: SessionId, ev: RequestPausedEvent| {
        let p = &ev.params;
        let document = matches!(p.resource_Type, ResourceType::Document);
        if egress.allow(&p.request.url, document) {
            RequestPausedDecision::Continue(None)
        } else {
            RequestPausedDecision::Fail(FailRequest {
                request_id: p.request_id.clone(),
                error_reason: ErrorReason::BlockedByClient,
            })
        }
    };
    tab.enable_request_interception(Arc::new(interceptor)).map_err(nav)?;
    tab.enable_fetch(None, None).map_err(nav)?;
    tab.set_default_timeout(budget);
    tab.navigate_to(url).map_err(nav)?.wait_until_navigated().map_err(nav)?;
    // Settle: read until two consecutive snapshots agree, or the settle time is spent.
    let start = Instant::now();
    let mut last = tab.get_content().map_err(nav)?;
    while start.elapsed() < settle {
        std::thread::sleep(Duration::from_millis(400));
        let now = tab.get_content().map_err(nav)?;
        if now.len() == last.len() && now == last {
            break;
        }
        last = now;
    }
    if last.len() > MAX_HTML_BYTES {
        last.truncate(crate::text::floor_boundary(&last, MAX_HTML_BYTES));
    }
    Ok(Rendered { html: last, final_url: tab.get_url() })
}

/// Browser error text is third-party; keep a line of it.
fn short(s: &str) -> String {
    let line = s.lines().next().unwrap_or("");
    line[..crate::text::floor_boundary(line, 200)].to_string()
}

fn nav<E: std::fmt::Display>(e: E) -> RenderError {
    RenderError::Navigation(short(&e.to_string()))
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::*;

    #[test]
    fn a_missing_browser_is_reported_not_launched() {
        let r = EdgeRenderer::at(PathBuf::from("Z:\\nope\\msedge.exe"));
        assert_eq!(r.render("https://example.org/", &CancelToken::new()), Err(RenderError::NoBrowser));
    }

    /// Found in review: Edge inherited the daemon's whole environment, API keys included.
    #[test]
    fn edge_sees_no_credential_and_only_the_system_variables() {
        let env = [
            ("OPENROUTER_API_KEY", "sk-or-secret"),
            ("ANTHROPIC_API_KEY", "sk-ant"),
            ("GITHUB_TOKEN", "ghp"),
            ("SystemRoot", "C:\\Windows"),
            ("LOCALAPPDATA", "C:\\Users\\m\\AppData\\Local"),
            ("Path", "C:\\Windows"),
        ];
        let kept = edge_env(env.iter().map(|(k, v)| (OsString::from(k), OsString::from(v))));
        let names: Vec<&OsStr> = kept.iter().map(|(k, _)| k.as_os_str()).collect();
        assert_eq!(names, [OsStr::new("SystemRoot"), OsStr::new("LOCALAPPDATA"), OsStr::new("Path")]);
        for k in marlowe_brains::scrubbed_env() {
            assert!(!EDGE_ENV.iter().any(|e| e.eq_ignore_ascii_case(k)), "{k} is never passed");
        }
    }

    /// Found in review: a rendered page's JavaScript could reach loopback and LAN services.
    #[test]
    fn page_requests_reach_only_public_https_hosts() {
        let public = |_: &str, _: u16| vec!["93.184.216.34".parse().unwrap()];
        let lan = |_: &str, _: u16| vec!["192.168.1.1".parse().unwrap()];
        let mixed = |_: &str, _: u16| vec!["93.184.216.34".parse().unwrap(), "10.0.0.5".parse().unwrap()];
        let nothing = |_: &str, _: u16| Vec::new();
        assert!(egress_allowed("https://example.org/app.js", &public));
        assert!(!egress_allowed("http://example.org/", &public), "plain http");
        assert!(!egress_allowed("https://127.0.0.1:11434/api/tags", &public), "loopback literal");
        assert!(!egress_allowed("https://[::1]/", &public));
        assert!(!egress_allowed("https://router.home/", &lan), "a name that resolves to the LAN");
        assert!(!egress_allowed("https://rebind.example/", &mixed), "every address must be public");
        assert!(!egress_allowed("https://nowhere.example/", &nothing));
        assert!(!egress_allowed("ws://example.org/socket", &public));
        assert!(egress_allowed("data:image/png;base64,AAAA", &nothing));
        assert!(same_page("https://e.org/app/#/route", "https://e.org/app"));
    }

    /// Renders the SPA-shell fixture from disk through the real Edge, when installed.
    #[test]
    fn edge_renders_a_client_side_page() {
        let Some(r) = EdgeRenderer::find() else {
            eprintln!("skipped: Microsoft Edge is not installed");
            return;
        };
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../marlowe-extract/tests/fixtures/spa_shell.html");
        let fixture = fixture.canonicalize().expect("fixture");
        let url = format!("file:///{}", fixture.display().to_string().trim_start_matches(r"\\?\").replace('\\', "/"));
        let out = r.render(&url, &CancelToken::new()).expect("render");
        assert!(out.html.contains("priority lanes"), "{}", &out.html[..out.html.len().min(400)]);
        let m = marlowe_extract::main_content::Selector::with_defaults()
            .read(out.html.as_bytes(), Some("text/html; charset=utf-8"), Some("https://lumen.example/docs"))
            .unwrap();
        assert!(!m.client_rendered, "rendered page still thin: {}", m.document.text);
        assert!(m.markdown.contains("Read the lanes guide](https://example.org/lumen/lanes)"), "{}", m.markdown);
    }
}
