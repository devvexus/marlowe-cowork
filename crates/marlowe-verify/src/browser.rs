//! Browser checks: load a page in a real (headless) browser, drive it with a few steps, and
//! report what a developer would look at: the page's text, every console error and uncaught
//! exception, failed requests, and a screenshot.
//!
//! This crate defines the request, the report and the small step language ([`Step::parse`]); the
//! driver is a [`Browser`] the host supplies (`marlowe-exec` drives the installed Microsoft Edge
//! over DevTools; tests use a fake). A page on this machine runs with the browser kept on this
//! machine: requests to other hosts fail and are reported as such, because verifying your own
//! app must not turn into an unreviewed visit to the internet.
//!
//! Steps, one per string (`fill` and `select` split at the first ` = `):
//!
//! | Step | Does |
//! |---|---|
//! | `click <selector>` / `hover <selector>` | mouse on the element (scrolled into view) |
//! | `fill <selector> = <text>` | sets the value and fires `input` and `change` |
//! | `select <selector> = <value>` | picks an `<option>` |
//! | `type <text>` / `press <Key>` | keyboard input to the focused element (`Enter`, `Tab`, `Escape`) |
//! | `wait <selector>` / `wait_gone <selector>` / `wait_text <text>` | until it appears, vanishes, shows |
//! | `eval <js>` | evaluates an expression (promises awaited) and records the value |
//! | `goto <url or /path>` | navigates (a local page stays on this machine; a remote page on the hosts the call was approved for) |
//! | `scroll <selector\|top\|bottom>` / `sleep <ms>` | |
//!
//! What a page may reach is a [`Reach`], built from the call's plan and enforced by the driver on
//! every request the page makes (the browser's own request interception, so `eval`, iframes and
//! redirects are covered, not just `goto`): the hosts the call was judged for, loopback for a page
//! on this machine, files only below the workspace.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use marlowe_model::{CancelToken, ImageRef};

use crate::http::Url;

/// A browser-driving step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Click(String),
    Hover(String),
    Fill { selector: String, text: String },
    Select { selector: String, value: String },
    Type(String),
    Press(String),
    Wait(String),
    WaitGone(String),
    WaitText(String),
    Eval(String),
    Goto(String),
    Scroll(String),
    Sleep(u64),
}

impl Step {
    /// Parses one step string.
    pub fn parse(raw: &str) -> Result<Step, String> {
        let raw = raw.trim();
        let (verb, rest) = raw.split_once(char::is_whitespace).map(|(v, r)| (v, r.trim())).unwrap_or((raw, ""));
        let need = |what: &str| -> Result<String, String> {
            if rest.is_empty() {
                Err(format!("`{verb}` needs {what} (e.g. `{verb} {}`)", example(verb)))
            } else {
                Ok(rest.to_string())
            }
        };
        let pair = |what: &str| -> Result<(String, String), String> {
            rest.split_once(" = ")
                .map(|(a, b)| (a.trim().to_string(), b.to_string()))
                .filter(|(a, _)| !a.is_empty())
                .ok_or_else(|| format!("`{verb}` needs `<selector> = <{what}>` (e.g. `{verb} {}`)", example(verb)))
        };
        Ok(match verb.to_ascii_lowercase().as_str() {
            "click" => Step::Click(need("a CSS selector")?),
            "hover" => Step::Hover(need("a CSS selector")?),
            "fill" => {
                let (selector, text) = pair("text")?;
                Step::Fill { selector, text }
            }
            "select" => {
                let (selector, value) = pair("value")?;
                Step::Select { selector, value }
            }
            "type" => Step::Type(need("text")?),
            "press" => Step::Press(need("a key")?),
            "wait" => Step::Wait(need("a CSS selector")?),
            "wait_gone" => Step::WaitGone(need("a CSS selector")?),
            "wait_text" => Step::WaitText(need("text")?),
            "eval" => Step::Eval(need("a JavaScript expression")?),
            "goto" => Step::Goto(need("a URL or /path")?),
            "scroll" => Step::Scroll(need("a selector, top or bottom")?),
            "sleep" => Step::Sleep(rest.trim_end_matches("ms").trim().parse::<u64>().map_err(|_| "`sleep` needs milliseconds (e.g. `sleep 500`)".to_string())?.min(10_000)),
            other => return Err(format!("unknown step `{other}`; steps are click, hover, fill, select, type, press, wait, wait_gone, wait_text, eval, goto, scroll, sleep")),
        })
    }

    pub fn parse_all(raw: &[String]) -> Result<Vec<Step>, String> {
        raw.iter().enumerate().map(|(i, s)| Step::parse(s).map_err(|e| format!("steps[{i}]: {e}"))).collect()
    }
}

impl std::fmt::Display for Step {
    /// The step as it is typed.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Step::Click(a) => write!(f, "click {a}"),
            Step::Hover(a) => write!(f, "hover {a}"),
            Step::Fill { selector, text } => write!(f, "fill {selector} = {text}"),
            Step::Select { selector, value } => write!(f, "select {selector} = {value}"),
            Step::Type(a) => write!(f, "type {a}"),
            Step::Press(a) => write!(f, "press {a}"),
            Step::Wait(a) => write!(f, "wait {a}"),
            Step::WaitGone(a) => write!(f, "wait_gone {a}"),
            Step::WaitText(a) => write!(f, "wait_text {a}"),
            Step::Eval(a) => write!(f, "eval {a}"),
            Step::Goto(a) => write!(f, "goto {a}"),
            Step::Scroll(a) => write!(f, "scroll {a}"),
            Step::Sleep(ms) => write!(f, "sleep {ms}"),
        }
    }
}

impl Step {
    /// A step that can change state on a server (a click may submit a form, `eval` runs anything,
    /// `goto` is a request); waits, hovers and scrolls only look.
    pub fn acts(&self) -> bool {
        matches!(self, Step::Click(_) | Step::Fill { .. } | Step::Select { .. } | Step::Type(_) | Step::Press(_) | Step::Eval(_) | Step::Goto(_))
    }
}

fn example(verb: &str) -> &'static str {
    match verb {
        "fill" => "#email = ada@example.org",
        "select" => "#country = DE",
        "click" | "hover" | "wait" | "wait_gone" => "button.save",
        "press" => "Enter",
        "goto" => "/about",
        _ => "...",
    }
}

/// What a page may reach, decided from the call before the browser starts and enforced per
/// request while it runs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reach {
    /// Remote hosts (and their subdomains) the call was judged for. Never loopback.
    pub hosts: Vec<String>,
    /// A page on this machine: loopback may be read.
    pub loopback: bool,
    /// Loopback ports a request that changes state may go to: the page's own server and the
    /// services this session started. Any other local port is read-only.
    pub write_ports: Vec<u16>,
    /// A page from the workspace: files may be opened below this directory, and nowhere else.
    pub file_root: Option<PathBuf>,
}

fn reads(method: &str) -> bool {
    matches!(method.to_ascii_uppercase().as_str(), "GET" | "HEAD" | "OPTIONS")
}

impl Reach {
    /// Whether the page may make this request, or why not.
    pub fn check(&self, method: &str, url: &str) -> Result<(), String> {
        let url = url.trim();
        let scheme = url.split_once(':').map(|(s, _)| s.to_ascii_lowercase()).unwrap_or_default();
        match scheme.as_str() {
            "data" | "blob" | "about" | "chrome-error" => Ok(()),
            "file" => self.check_file(url),
            "http" | "https" | "ws" | "wss" => self.check_net(method, url, &scheme),
            "" => Err(format!("`{url}` is not a URL")),
            other => Err(format!("{other}: URLs are not opened by a check")),
        }
    }

    fn check_net(&self, method: &str, url: &str, scheme: &str) -> Result<(), String> {
        let as_http = match scheme {
            "ws" => url.replacen("ws:", "http:", 1),
            "wss" => url.replacen("wss:", "https:", 1),
            _ => url.to_string(),
        };
        let u = Url::parse(&as_http).map_err(|e| e.to_string())?;
        let host = u.host.to_ascii_lowercase();
        if u.is_loopback() {
            if !self.loopback {
                return Err(format!("{} is on this machine; a page from another host cannot reach it", u.authority()));
            }
            if reads(method) || self.write_ports.contains(&u.port) {
                return Ok(());
            }
            return Err(format!("a {method} to {} changes state on a local service this session did not start", u.authority()));
        }
        if self.hosts.iter().any(|a| host == *a || host.ends_with(&format!(".{a}"))) {
            return Ok(());
        }
        if self.loopback && self.hosts.is_empty() {
            Err(format!("{host} is not on this machine: a check of a page on this machine stays on this machine"))
        } else {
            Err(format!(
                "{host} is not a host this call was approved to reach (approved: {})",
                if self.hosts.is_empty() { "none".to_string() } else { self.hosts.join(", ") }
            ))
        }
    }

    fn check_file(&self, url: &str) -> Result<(), String> {
        let Some(root) = &self.file_root else {
            return Err("a page that is not a file of the workspace cannot open local files".into());
        };
        let rest = url.get("file:".len()..).unwrap_or("").strip_prefix("//").ok_or("not a file:// URL")?;
        let at = rest.find('/').unwrap_or(rest.len());
        let (authority, path) = rest.split_at(at);
        if !(authority.is_empty() || authority.eq_ignore_ascii_case("localhost")) {
            return Err(format!("file://{authority}/... is a network share"));
        }
        let path = percent_decode(path.split(['?', '#']).next().unwrap_or(""));
        // `/C:/dir/x` is a drive path.
        let b = path.as_bytes();
        let path = if b.len() > 2 && b[0] == b'/' && b[2] == b':' { path[1..].to_string() } else { path };
        let abs = crate::env::resolve(root, &path);
        if crate::env::inside(root, &abs) {
            Ok(())
        } else {
            Err(format!("{} is outside the workspace; a page can open only files below {}", abs.display(), root.display()))
        }
    }

    /// Edge's `--host-resolver-rules`: names that are not allowed do not resolve, a second line
    /// of defence behind [`Reach::check`].
    pub fn resolver_rules(&self) -> String {
        let mut r = String::from("MAP * ~NOTFOUND");
        if self.loopback {
            r.push_str(" , EXCLUDE localhost , EXCLUDE 127.0.0.1");
        }
        for h in &self.hosts {
            r.push_str(&format!(" , EXCLUDE {h} , EXCLUDE *.{h}"));
        }
        r
    }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Some(v) = s.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// What to load and collect.
#[derive(Debug, Clone)]
pub struct BrowseRequest {
    pub url: String,
    /// The page is on this machine (loopback or a workspace file); it changes how a blocked
    /// request is explained.
    pub strict_local: bool,
    /// What the page may reach.
    pub reach: Reach,
    pub viewport: (u32, u32),
    pub steps: Vec<Step>,
    /// A CSS selector, or `text:<words>`, to wait for after load (otherwise the page settles).
    pub wait_for: Option<String>,
    pub snapshot: bool,
    pub screenshot: bool,
    pub full_page: bool,
    pub timeout: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Level {
    Error,
    Warning,
    Info,
    Log,
    Debug,
}

impl Level {
    pub fn name(self) -> &'static str {
        match self {
            Level::Error => "error",
            Level::Warning => "warning",
            Level::Info => "info",
            Level::Log => "log",
            Level::Debug => "debug",
        }
    }
}

/// One console message, uncaught exception or browser log entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsoleEntry {
    pub level: Level,
    pub text: String,
    /// `url:line:col` where known.
    pub location: Option<String>,
}

/// A request that failed or answered 4xx/5xx.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedRequest {
    pub url: String,
    pub reason: String,
    pub status: Option<u16>,
}

/// How one step went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepResult {
    pub raw: String,
    pub ok: bool,
    /// An `eval` value, or why the step failed.
    pub detail: String,
}

/// What the browser saw.
#[derive(Debug, Clone, Default)]
pub struct BrowseReport {
    pub final_url: String,
    pub title: String,
    pub snapshot: Option<String>,
    pub console: Vec<ConsoleEntry>,
    pub failed_requests: Vec<FailedRequest>,
    pub screenshot: Option<Vec<u8>>,
    pub steps: Vec<StepResult>,
    pub load_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BrowseError {
    #[error("Microsoft Edge is not installed (set MARLOWE_EDGE to msedge.exe to use another location)")]
    NoBrowser,
    #[error("the browser did not start: {0}")]
    Launch(String),
    #[error("the browser did not finish within {0:?}")]
    Timeout(Duration),
    #[error("cancelled")]
    Cancelled,
    #[error("navigation failed: {0}")]
    Navigation(String),
}

/// A headless browser the host provides.
pub trait Browser: Send + Sync {
    fn name(&self) -> String;
    fn available(&self) -> bool;
    fn run(&self, req: &BrowseRequest, cancel: &CancelToken) -> Result<BrowseReport, BrowseError>;
}

impl BrowseReport {
    pub fn errors(&self) -> impl Iterator<Item = &ConsoleEntry> {
        self.console.iter().filter(|c| c.level == Level::Error)
    }

    pub fn failed_step(&self) -> Option<&StepResult> {
        self.steps.iter().find(|s| !s.ok)
    }

    /// A page with no console errors, no failed requests (other than blocked ones) and no failed step.
    pub fn clean(&self) -> bool {
        self.errors().next().is_none() && self.failed_step().is_none() && self.failed_requests.iter().all(|r| is_blocked(r))
    }
}

fn is_blocked(r: &FailedRequest) -> bool {
    r.reason.contains("ERR_NAME_NOT_RESOLVED") || r.reason.contains("ERR_BLOCKED") || r.reason.contains("ERR_PROXY")
}

/// What the tool result should include.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Show {
    /// The page outline and text.
    Snapshot,
    /// Console and network only.
    Console,
    /// A screenshot (the image travels as a result part).
    Screenshot,
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}...", s.chars().take(max).collect::<String>())
    }
}

/// The report as the model reads it.
pub fn render(req: &BrowseRequest, r: &BrowseReport, show: Show, shot: Option<&Shot>) -> String {
    let mut out = String::new();
    let title = if r.title.is_empty() { "(no title)".to_string() } else { format!("\"{}\"", clip(&r.title, 100)) };
    out.push_str(&format!("browser {} -> {title} (loaded in {} ms)\n", if r.final_url.is_empty() { &req.url } else { &r.final_url }, r.load_ms));
    for (i, s) in r.steps.iter().enumerate() {
        out.push_str(&format!("step {}: {} {}{}\n", i + 1, if s.ok { "ok  " } else { "FAIL" }, s.raw, if s.detail.is_empty() { String::new() } else { format!("  -> {}", clip(&s.detail, 400)) }));
    }
    // Console: errors and warnings first, identical messages folded.
    let mut groups: Vec<(&ConsoleEntry, usize)> = Vec::new();
    for c in &r.console {
        match groups.iter_mut().find(|(g, _)| g.level == c.level && g.text == c.text && g.location == c.location) {
            Some((_, n)) => *n += 1,
            None => groups.push((c, 1)),
        }
    }
    let count = |l: Level| r.console.iter().filter(|c| c.level == l).count();
    let (errors, warnings) = (count(Level::Error), count(Level::Warning));
    let others = r.console.len() - errors - warnings;
    out.push_str(&format!("console: {errors} error(s), {warnings} warning(s){}\n", if others > 0 { format!(", {others} other message(s)") } else { String::new() }));
    let limit = |l: Level| match (l, show) {
        (Level::Error, _) => 20,
        (Level::Warning, _) => 10,
        (_, Show::Console) => 30,
        _ => 0,
    };
    let mut shown = [0usize; 5];
    for (c, n) in &groups {
        let idx = c.level as usize;
        if shown[idx] >= limit(c.level) {
            continue;
        }
        shown[idx] += 1;
        out.push_str(&format!(
            "  {:<7} {}{}{}\n",
            c.level.name(),
            clip(c.text.lines().next().unwrap_or(""), 300),
            c.location.as_ref().map(|l| format!("   at {l}")).unwrap_or_default(),
            if *n > 1 { format!("   (x{n})") } else { String::new() }
        ));
    }
    if !r.failed_requests.is_empty() {
        out.push_str(&format!("failed requests: {}\n", r.failed_requests.len()));
        for f in r.failed_requests.iter().take(12) {
            let why = match f.status {
                Some(s) => format!("HTTP {s}"),
                None if f.reason.contains("ERR_BLOCKED_BY_CLIENT (") => format!("not fetched: {}", f.reason),
                None if is_blocked(f) && req.strict_local => format!("{} (not fetched: a check of a page on this machine keeps the browser on this machine)", f.reason),
                None => f.reason.clone(),
            };
            out.push_str(&format!("  {} -> {why}\n", clip(&f.url, 160)));
        }
    }
    if let Some(snap) = &r.snapshot {
        if show == Show::Snapshot || req.snapshot {
            out.push_str("page:\n");
            out.push_str(snap.trim_end());
            out.push('\n');
        }
    }
    if let Some(s) = shot {
        out.push_str(&format!("screenshot: {} ({}x{})\n", s.path.display().to_string().trim_start_matches(r"\\?\"), s.w, s.h));
    } else if req.screenshot {
        out.push_str("screenshot: not captured\n");
    }
    out.trim_end().to_string()
}

// ---- screenshots -------------------------------------------------------------------------------

/// A saved screenshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shot {
    pub path: PathBuf,
    pub w: u32,
    pub h: u32,
    /// Lower-case hex blake3 of the PNG, the key in the blob store.
    pub blob: Option<String>,
}

impl Shot {
    /// The image as a tool-result part (needs the blob store).
    pub fn image_ref(&self) -> Option<ImageRef> {
        Some(ImageRef { mime: "image/png".into(), blob: self.blob.clone()?, w: self.w, h: self.h })
    }
}

/// Screenshots on disk (the last [`ShotStore::KEEP`]) and in the content-addressed blob store.
#[derive(Debug, Default)]
pub struct ShotStore {
    dirs: Mutex<(Option<PathBuf>, Option<PathBuf>)>,
}

impl ShotStore {
    /// Screenshots kept on disk.
    pub const KEEP: usize = 3;

    pub fn set_dirs(&self, shots: PathBuf, blobs: Option<PathBuf>) {
        *self.dirs.lock().unwrap_or_else(|p| p.into_inner()) = (Some(shots), blobs);
    }

    /// Writes `png` and returns where it went.
    pub fn save(&self, png: &[u8]) -> Result<Shot, String> {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let (shots, blobs) = {
            let g = self.dirs.lock().unwrap_or_else(|p| p.into_inner());
            (g.0.clone().unwrap_or_else(|| std::env::temp_dir().join("marlowe-verify").join("shots")), g.1.clone())
        };
        let (w, h) = png_size(png).ok_or("the browser returned an image that is not a PNG")?;
        std::fs::create_dir_all(&shots).map_err(|e| format!("cannot create {}: {e}", shots.display()))?;
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
        let path = shots.join(format!("shot-{stamp}-{}.png", SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
        std::fs::write(&path, png).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        crate::run::prune(&shots, Self::KEEP);
        let blob = blobs.and_then(|dir| store_blob(&dir, png).ok());
        Ok(Shot { path, w, h, blob })
    }
}

/// Writes `bytes` as `<dir>/<blake3 hex>` (once) and returns the hex.
pub fn store_blob(dir: &Path, bytes: &[u8]) -> std::io::Result<String> {
    let hex = blake3::hash(bytes).to_hex().to_string();
    let path = dir.join(&hex);
    if !path.is_file() {
        std::fs::create_dir_all(dir)?;
        let tmp = dir.join(format!(".{hex}.{}.tmp", std::process::id()));
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, &path).inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })?;
    }
    Ok(hex)
}

/// Width and height from a PNG's IHDR.
pub fn png_size(png: &[u8]) -> Option<(u32, u32)> {
    if png.len() < 24 || &png[..8] != b"\x89PNG\r\n\x1a\n" || &png[12..16] != b"IHDR" {
        return None;
    }
    let be = |i: usize| u32::from_be_bytes([png[i], png[i + 1], png[i + 2], png[i + 3]]);
    Some((be(16), be(20)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_parse_and_say_what_is_missing() {
        assert_eq!(Step::parse("click button.save").unwrap(), Step::Click("button.save".into()));
        assert_eq!(Step::parse("fill input[name=q] = hello world").unwrap(), Step::Fill { selector: "input[name=q]".into(), text: "hello world".into() });
        assert_eq!(Step::parse("press Enter").unwrap(), Step::Press("Enter".into()));
        assert_eq!(Step::parse("sleep 250ms").unwrap(), Step::Sleep(250));
        assert_eq!(Step::parse("sleep 999999").unwrap(), Step::Sleep(10_000));
        assert_eq!(Step::parse("eval document.title").unwrap(), Step::Eval("document.title".into()));
        assert!(Step::parse("fill #a").unwrap_err().contains("<selector> = <text>"));
        assert!(Step::parse("click").unwrap_err().contains("CSS selector"));
        assert!(Step::parse("dance").unwrap_err().contains("unknown step"));
        let err = Step::parse_all(&["click a".into(), "nope".into()]).unwrap_err();
        assert!(err.starts_with("steps[1]:"), "{err}");
    }

    fn remote() -> Reach {
        Reach { hosts: vec!["github.com".into()], loopback: false, write_ports: vec![], file_root: None }
    }

    fn local(root: &Path) -> Reach {
        Reach { hosts: vec![], loopback: true, write_ports: vec![5173], file_root: Some(root.to_path_buf()) }
    }

    #[test]
    fn a_remote_page_reaches_only_the_hosts_it_was_judged_for() {
        let r = remote();
        assert!(r.check("GET", "https://github.com/a").is_ok());
        assert!(r.check("GET", "https://api.github.com/a").is_ok(), "a subdomain of an approved host");
        assert!(r.check("GET", "wss://github.com/socket").is_ok());
        let e = r.check("GET", "https://evil.example/?d=1").unwrap_err();
        assert!(e.contains("not a host this call was approved to reach") && e.contains("github.com"), "{e}");
        assert!(r.check("GET", "https://notgithub.com/").is_err(), "a suffix of the name is not a subdomain");
        assert!(r.check("GET", "http://127.0.0.1:11434/api").unwrap_err().contains("on this machine"));
        assert!(r.check("GET", "http://localhost:8080/").is_err());
        assert!(r.check("GET", "file:///C:/Users/me/.ssh/id_ed25519").unwrap_err().contains("cannot open local files"));
        assert!(r.check("GET", "ftp://github.com/x").is_err());
        assert!(r.check("GET", "data:text/html,hi").is_ok());
        assert!(r.resolver_rules().starts_with("MAP * ~NOTFOUND") && r.resolver_rules().contains("EXCLUDE *.github.com") && !r.resolver_rules().contains("localhost"));
    }

    #[test]
    fn a_page_on_this_machine_reads_loopback_writes_only_to_its_own_ports_and_opens_only_workspace_files() {
        let root = std::env::temp_dir().join(format!("marlowe-verify-reach-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("site")).unwrap();
        let root = root.canonicalize().unwrap();
        let r = local(&root);
        assert!(r.check("GET", "http://localhost:11434/api/tags").is_ok(), "reading any local port is free");
        assert!(r.check("POST", "http://localhost:5173/api").is_ok());
        let e = r.check("POST", "http://localhost:11434/api/generate").unwrap_err();
        assert!(e.contains("did not start"), "{e}");
        assert!(r.check("DELETE", "http://127.0.0.1:8080/x").is_err());
        assert!(r.check("GET", "https://example.org/").unwrap_err().contains("stays on this machine"));
        let url = |p: &Path| {
            let s = p.to_string_lossy().replace('\\', "/");
            format!("file:///{}", s.trim_start_matches("//?/").trim_start_matches('/'))
        };
        assert!(r.check("GET", &url(&root.join("site").join("index.html"))).is_ok());
        let outside = root.parent().unwrap().join("secret.txt");
        let e = r.check("GET", &url(&outside)).unwrap_err();
        assert!(e.contains("outside the workspace"), "{e}");
        assert!(r.check("GET", &format!("{}/../secret.txt", url(&root))).is_err(), "dot-dot does not climb out");
        assert!(r.check("GET", &format!("{}/site/%2e%2e/%2e%2e/secret.txt", url(&root))).is_err(), "nor does an encoded one");
        assert!(r.check("GET", "file://server/share/x").unwrap_err().contains("network share"));
        assert_eq!(r.resolver_rules(), "MAP * ~NOTFOUND , EXCLUDE localhost , EXCLUDE 127.0.0.1");
        let _ = std::fs::remove_dir_all(&root);
    }

    fn report() -> BrowseReport {
        BrowseReport {
            final_url: "http://127.0.0.1:5000/".into(),
            title: "Fixture".into(),
            snapshot: Some("h1: Fixture\nbutton: Save".into()),
            console: vec![
                ConsoleEntry { level: Level::Error, text: "Uncaught TypeError: x is undefined".into(), location: Some("app.js:3:9".into()) },
                ConsoleEntry { level: Level::Error, text: "Uncaught TypeError: x is undefined".into(), location: Some("app.js:3:9".into()) },
                ConsoleEntry { level: Level::Warning, text: "deprecated".into(), location: None },
                ConsoleEntry { level: Level::Log, text: "hello".into(), location: None },
            ],
            failed_requests: vec![
                FailedRequest { url: "http://127.0.0.1:5000/missing.png".into(), reason: "HTTP 404".into(), status: Some(404) },
                FailedRequest { url: "https://cdn.example/x.js".into(), reason: "net::ERR_NAME_NOT_RESOLVED".into(), status: None },
            ],
            screenshot: None,
            steps: vec![StepResult { raw: "eval 1+1".into(), ok: true, detail: "2".into() }],
            load_ms: 412,
        }
    }

    #[test]
    fn the_report_folds_repeats_and_explains_blocked_hosts() {
        let req = BrowseRequest { url: "http://127.0.0.1:5000/".into(), strict_local: true, reach: Reach::default(), viewport: (1280, 800), steps: Vec::new(), wait_for: None, snapshot: true, screenshot: false, full_page: false, timeout: Duration::from_secs(30) };
        let r = report();
        let t = render(&req, &r, Show::Snapshot, None);
        assert!(t.contains("console: 2 error(s), 1 warning(s), 1 other message(s)"), "{t}");
        assert!(t.contains("Uncaught TypeError: x is undefined   at app.js:3:9   (x2)"), "{t}");
        assert!(t.contains("HTTP 404") && t.contains("not fetched: a check of a page on this machine"), "{t}");
        assert!(t.contains("step 1: ok   eval 1+1  -> 2") && t.contains("page:\nh1: Fixture"), "{t}");
        assert!(!t.contains("hello"), "plain log lines stay out unless console is asked for");
        let t = render(&req, &r, Show::Console, None);
        assert!(t.contains("log     hello"), "{t}");
        assert!(!r.clean());
    }

    #[test]
    fn screenshots_are_kept_three_deep_and_content_addressed() {
        let dir = std::env::temp_dir().join(format!("marlowe-verify-shots-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = ShotStore::default();
        store.set_dirs(dir.join("shots"), Some(dir.join("blobs")));
        // A minimal valid PNG header: 1x1.
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        png.extend_from_slice(&1u32.to_be_bytes());
        png.extend_from_slice(&1u32.to_be_bytes());
        png.extend_from_slice(&[8, 2, 0, 0, 0, 0, 0, 0, 0]);
        let mut shots = Vec::new();
        for i in 0..5u8 {
            let mut p = png.clone();
            p.push(i);
            std::thread::sleep(Duration::from_millis(15));
            shots.push(store.save(&p).unwrap());
        }
        let on_disk = std::fs::read_dir(dir.join("shots")).unwrap().count();
        assert_eq!(on_disk, ShotStore::KEEP);
        let last = shots.last().unwrap();
        assert_eq!((last.w, last.h), (1, 1));
        let r = last.image_ref().unwrap();
        assert_eq!(r.blob.len(), 64);
        assert!(dir.join("blobs").join(&r.blob).is_file(), "the blob store holds the image under its hash");
        assert!(store.save(b"not a png at all, not at all").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
