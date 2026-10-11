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
//! | `goto <url or /path>` | navigates (same host only for a local page) |
//! | `scroll <selector\|top\|bottom>` / `sleep <ms>` | |

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use marlowe_model::{CancelToken, ImageRef};

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

/// What to load and collect.
#[derive(Debug, Clone)]
pub struct BrowseRequest {
    pub url: String,
    /// Keep the browser on this machine: every request to another host fails.
    pub strict_local: bool,
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
        let req = BrowseRequest { url: "http://127.0.0.1:5000/".into(), strict_local: true, viewport: (1280, 800), steps: Vec::new(), wait_for: None, snapshot: true, screenshot: false, full_page: false, timeout: Duration::from_secs(30) };
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
