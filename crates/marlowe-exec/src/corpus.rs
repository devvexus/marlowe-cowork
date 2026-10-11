//! The corpus fetcher: **fetch and read many pages at once, politely.** Restored from the old
//! harness (`fetch_and_extract`) onto today's marlowe-net, marlowe-extract and marlowe-research.
//!
//! - **Concurrency** is a knob ([`CorpusConfig::concurrency`], 0 = marlowe-net's derived width).
//!   Workers are std threads; each fetches and then extracts its own page, so CPU work on one page
//!   overlaps network waits on the others.
//! - **Politeness**: one request in flight per host, hosts served round-robin, and every request
//!   (each redirect hop included) passes the [`HostGate`]: robots.txt (RFC 9309) for our token
//!   and a per-host spacing of at least 1 s or the site's `Crawl-delay`. A 429/503 with a short
//!   `Retry-After` is retried once after waiting. Every request names itself with [`USER_AGENT`].
//! - **Reading**: HTML goes through main-content selection (see `marlowe_extract::main_content`)
//!   and comes back as markdown with links, headings, tables and code; other formats through
//!   `marlowe_extract::extract`. A thin client-rendered page is rendered through the system
//!   Microsoft Edge when a [`Renderer`] is configured (see [`crate::render`]).
//!
//! Every URL handed in must already be permitted by the caller's egress policy; this module only
//! adds politeness. Everything returned is untrusted content and must be tainted by the caller.

use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use marlowe_extract::main_content::{Candidate, ChoiceReason, PageType, Selector};
use marlowe_extract::{Document, Input, Link};
use marlowe_model::CancelToken;
use marlowe_net::Target;
use marlowe_research::politeness::split_url;
use marlowe_research::{DocRef, HostGate, HttpGet, Refusal, USER_AGENT};

pub use crate::render::{RenderError, Rendered, Renderer};

/// Redirect hops followed per URL; each hop is admitted through the gate again.
pub const MAX_REDIRECTS: usize = 5;
/// A `Retry-After` longer than this is not waited for.
pub const MAX_RETRY_AFTER: Duration = Duration::from_secs(30);
/// What page fetches accept.
pub const ACCEPT: &str = "text/html,application/xhtml+xml,application/pdf;q=0.9,text/plain;q=0.8,*/*;q=0.5";

/// One HTTP exchange, redirects not followed.
#[derive(Debug, Clone, Default)]
pub struct RawResponse {
    pub status: u16,
    pub content_type: Option<String>,
    pub bytes: Vec<u8>,
    /// The `Location` header of a 3xx, verbatim.
    pub location: Option<String>,
    pub retry_after: Option<Duration>,
    pub wire_bytes: usize,
}

/// The network as the corpus sees it: one GET, no redirect following. Tests inject recordings.
pub trait PageFetch: Send + Sync {
    fn fetch(&self, url: &str, cancel: &CancelToken) -> Result<RawResponse, String>;
}

/// marlowe-net's pooled client with [`USER_AGENT`] and [`ACCEPT`].
#[derive(Debug, Clone, Copy, Default)]
pub struct NetPages;

impl PageFetch for NetPages {
    fn fetch(&self, url: &str, cancel: &CancelToken) -> Result<RawResponse, String> {
        let target = Target::parse(url).map_err(|e| e.to_string())?;
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("marlowe-corpus-get".into())
            .spawn(move || {
                let h = marlowe_net::Headers { user_agent: USER_AGENT, accept: ACCEPT };
                let _ = tx.send(marlowe_net::fetch_with(&target, &h));
            })
            .map_err(|e| format!("could not start the fetch: {e}"))?;
        match crate::web::wait_for(&rx, cancel, Instant::now() + crate::web::FETCH_DEADLINE) {
            Ok(Ok(f)) => Ok(RawResponse {
                status: f.status,
                content_type: f.content_type,
                wire_bytes: f.wire_bytes,
                bytes: f.bytes,
                location: f.redirect_to,
                retry_after: marlowe_research::net::retry_after_secs(f.retry_after.as_deref()),
            }),
            Ok(Err(e)) => Err(e.to_string()),
            Err(crate::web::Stopped::Cancelled) => Err("cancelled".into()),
            Err(crate::web::Stopped::Deadline) => Err("no complete response before the deadline".into()),
            Err(crate::web::Stopped::WorkerLost) => Err("the fetch failed unexpectedly".into()),
        }
    }
}

/// How pages are read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtractMode {
    /// html.rs only (the pre-M4.5 reading).
    Baseline,
    /// Main-content selection across the compiled candidates.
    MainContent,
}

/// Settings for one corpus pass.
#[derive(Clone)]
pub struct CorpusConfig {
    /// Worker threads; 0 = `marlowe_net::io_concurrency()`. Never more than there are URLs.
    pub concurrency: usize,
    pub mode: ExtractMode,
    pub gate: HostGate,
    /// Fetches robots.txt (redirects followed).
    pub robots_http: Arc<dyn HttpGet>,
    /// Renders thin client-rendered pages; `None` reports them as read with a warning.
    pub renderer: Option<Arc<dyn Renderer>>,
}

impl Default for CorpusConfig {
    fn default() -> Self {
        Self {
            concurrency: 0,
            mode: ExtractMode::MainContent,
            gate: shared_gate(),
            robots_http: Arc::new(marlowe_research::NetHttp::new()),
            renderer: crate::render::EdgeRenderer::find().map(|r| Arc::new(r) as Arc<dyn Renderer>),
        }
    }
}

/// A ready corpus fetcher: a configuration and the network it fetches through.
#[derive(Clone)]
pub struct Corpus {
    pub config: CorpusConfig,
    pub net: Arc<dyn PageFetch>,
}

impl Corpus {
    /// The product setup: the process-wide gate, marlowe-net, main-content reading, and the
    /// system Edge for client-rendered pages when it is installed.
    pub fn product() -> Self {
        Self { config: CorpusConfig::default(), net: Arc::new(NetPages) }
    }

    /// [`fetch_and_extract`] with `concurrency` workers (0 = the configured width).
    pub fn fetch(&self, urls: &[String], concurrency: usize, cancel: &CancelToken) -> Vec<Outcome> {
        let mut cfg = self.config.clone();
        if concurrency > 0 {
            cfg.concurrency = concurrency;
        }
        fetch_and_extract(urls, &cfg, self.net.as_ref(), cancel)
    }
}

/// The process-wide gate: every page fetch in this process shares robots and host spacing.
pub fn shared_gate() -> HostGate {
    static G: std::sync::OnceLock<HostGate> = std::sync::OnceLock::new();
    G.get_or_init(HostGate::new).clone()
}

/// How the main content was chosen, for the record.
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    pub candidate: Candidate,
    pub reason: ChoiceReason,
    pub page_type: PageType,
    pub confidence: Option<f32>,
    /// Share of the chosen text mapped onto structured blocks.
    pub coverage: f32,
}

/// One page read.
#[derive(Debug, Clone)]
pub struct Page {
    pub url: String,
    /// After redirects.
    pub final_url: String,
    pub status: u16,
    pub content_type: Option<String>,
    /// `text` is the main content as plain text; `links` every link on the page.
    pub document: Document,
    /// The main content as markdown (links inline). For non-HTML formats, the document text.
    pub markdown: String,
    /// Content address of the bytes read (rendered HTML when rendered).
    pub doc: DocRef,
    pub reading: Option<Reading>,
    /// Read through the browser.
    pub rendered: bool,
    pub wire_bytes: usize,
}

impl Page {
    pub fn links(&self) -> &[Link] {
        &self.document.links
    }
}

/// What happened to one URL.
#[derive(Debug, Clone)]
pub enum Outcome {
    Read(Box<Page>),
    /// robots.txt or a crawl delay above the ceiling said no; nothing was fetched from the page.
    Refused {
        url: String,
        reason: Refusal,
    },
    /// The server sent a redirect that was not followed (not https, or too many hops).
    Redirect {
        url: String,
        status: u16,
        location: RedirectTo,
    },
    /// HTTP status >= 400.
    Status {
        url: String,
        status: u16,
    },
    Unreachable {
        url: String,
        detail: String,
    },
    /// Bytes arrived and could not be turned into text. `kind` is a harness constant.
    Unreadable {
        url: String,
        status: u16,
        kind: &'static str,
        detail: String,
    },
}

impl Outcome {
    pub fn url(&self) -> &str {
        match self {
            Outcome::Read(p) => &p.url,
            Outcome::Refused { url, .. }
            | Outcome::Redirect { url, .. }
            | Outcome::Status { url, .. }
            | Outcome::Unreachable { url, .. }
            | Outcome::Unreadable { url, .. } => url,
        }
    }

    pub fn page(&self) -> Option<&Page> {
        match self {
            Outcome::Read(p) => Some(p),
            _ => None,
        }
    }

    /// A one-line, model-safe reason for a page that was not read (no server-chosen text).
    pub fn why_not(&self) -> Option<String> {
        Some(match self {
            Outcome::Read(_) => return None,
            Outcome::Refused { reason, .. } => match reason {
                Refusal::Robots { host, .. } => format!("robots.txt on {host} disallows it"),
                Refusal::CrawlDelay { host, delay } => {
                    format!("{host} asks for {} s between requests", delay.as_secs())
                }
                Refusal::NotHttp(_) => "not an https URL".into(),
                Refusal::Cancelled => "cancelled".into(),
            },
            Outcome::Redirect { status, location, .. } => format!("HTTP {status} redirect {}", location.render()),
            Outcome::Status { status, .. } => format!("HTTP {status}"),
            Outcome::Unreachable { .. } => "unreachable".into(),
            Outcome::Unreadable { kind: CLIENT_RENDERED, detail, .. } => format!("unreadable: {detail}"),
            Outcome::Unreadable { kind, .. } => format!("unreadable ({kind})"),
        })
    }
}

// ---------------------------------------------------------------- redirects

/// A `Location` header after validation: only the parsed host and a path that cannot carry prose
/// are ever echoed (a redirect header is attacker-chosen text).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RedirectTo {
    Url(String),
    HostOnly(String),
    Unusable,
}

impl RedirectTo {
    pub fn render(&self) -> String {
        match self {
            RedirectTo::Url(u) => format!("to {u}"),
            RedirectTo::HostOnly(h) => format!("to a URL on {h} (path not echoed)"),
            RedirectTo::Unusable => "to a location that is not a valid https URL".to_string(),
        }
    }
}

const MAX_ECHOED_PATH: usize = 256;

/// Path, query and fragment characters only, no `+` (an encoded space), no `..`, and not shaped
/// like a sentence.
fn path_is_safe(path: &str) -> bool {
    path.len() <= MAX_ECHOED_PATH
        && !path.contains("..")
        && path.chars().all(|c| c.is_ascii_alphanumeric() || "-._~:/?#[]@$&=%".contains(c))
        && !looks_like_prose(path)
}

fn looks_like_prose(s: &str) -> bool {
    s.split(|c: char| !c.is_ascii_alphabetic()).filter(|run| run.len() >= 3).count() > 6
}

/// The DNS rule: LDH labels, 63 per label, 253 total, ASCII only.
fn host_is_safe(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && host
            .split('.')
            .all(|l| !l.is_empty() && l.len() <= 63 && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
        && !looks_like_prose(host)
}

pub fn sanitize_location(raw: &str) -> RedirectTo {
    match Target::parse(raw) {
        Ok(t) if host_is_safe(&t.host) => {
            if path_is_safe(&t.path) {
                let port = if t.port == 443 { String::new() } else { format!(":{}", t.port) };
                RedirectTo::Url(format!("https://{}{}{}", t.host, port, t.path))
            } else {
                RedirectTo::HostOnly(t.host)
            }
        }
        _ => RedirectTo::Unusable,
    }
}

/// `location` resolved against `current`; https only.
fn next_hop(current: &str, location: &str) -> Option<String> {
    let loc = location.trim();
    if loc.starts_with("https://") {
        return Some(loc.to_string());
    }
    let (origin, _) = split_url(current)?;
    if !origin.starts_with("https://") {
        return None;
    }
    if loc.starts_with('/') && !loc.starts_with("//") {
        return Some(format!("{origin}{loc}"));
    }
    if let Some(rest) = loc.strip_prefix("//") {
        return Some(format!("https://{rest}"));
    }
    None
}

// ---------------------------------------------------------------- scheduling

/// Per-host queues; a host has at most one URL in flight and hosts are served round-robin.
struct Scheduler {
    state: Mutex<SchedState>,
    cv: Condvar,
}

struct SchedState {
    hosts: Vec<(String, VecDeque<usize>)>,
    busy: HashSet<String>,
    cursor: usize,
    left: usize,
}

impl Scheduler {
    fn new(urls: &[String]) -> Self {
        let mut hosts: Vec<(String, VecDeque<usize>)> = Vec::new();
        for (i, u) in urls.iter().enumerate() {
            let origin = split_url(u).map(|(o, _)| o).unwrap_or_default();
            match hosts.iter_mut().find(|(o, _)| *o == origin) {
                Some((_, q)) => q.push_back(i),
                None => hosts.push((origin, VecDeque::from([i]))),
            }
        }
        let left = urls.len();
        Self { state: Mutex::new(SchedState { hosts, busy: HashSet::new(), cursor: 0, left }), cv: Condvar::new() }
    }

    /// The next URL whose host is idle, or None when all are handed out.
    fn take(&self, cancel: &CancelToken) -> Option<(usize, String)> {
        let mut s = self.state.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            if s.left == 0 {
                return None;
            }
            let n = s.hosts.len();
            for k in 0..n {
                let h = (s.cursor + k) % n;
                let origin = s.hosts[h].0.clone();
                // URLs without a host (unparseable) are not rate limited: they fail at once.
                if (!origin.is_empty() && s.busy.contains(&origin)) || s.hosts[h].1.is_empty() {
                    continue;
                }
                let i = s.hosts[h].1.pop_front().expect("non-empty");
                if !origin.is_empty() {
                    s.busy.insert(origin.clone());
                }
                s.cursor = (h + 1) % n;
                s.left -= 1;
                return Some((i, origin));
            }
            if cancel.is_cancelled() {
                // Hand out the rest so each gets a cancelled outcome.
                let h = s.hosts.iter().position(|(_, q)| !q.is_empty())?;
                let i = s.hosts[h].1.pop_front().expect("non-empty");
                s.left -= 1;
                return Some((i, String::new()));
            }
            s = self.cv.wait_timeout(s, Duration::from_millis(100)).unwrap_or_else(|p| p.into_inner()).0;
        }
    }

    fn done(&self, origin: &str) {
        let mut s = self.state.lock().unwrap_or_else(|p| p.into_inner());
        s.busy.remove(origin);
        self.cv.notify_all();
    }
}

// ---------------------------------------------------------------- the pass

/// Fetch and read many URLs concurrently. **Results are in input order**, and each URL's failure
/// is its own.
pub fn fetch_and_extract(
    urls: &[String],
    cfg: &CorpusConfig,
    net: &dyn PageFetch,
    cancel: &CancelToken,
) -> Vec<Outcome> {
    let n = urls.len();
    if n == 0 {
        return Vec::new();
    }
    let width = if cfg.concurrency == 0 { marlowe_net::io_concurrency() } else { cfg.concurrency };
    let workers = width.clamp(1, n);
    let slots: Vec<Mutex<Option<Outcome>>> = (0..n).map(|_| Mutex::new(None)).collect();
    let sched = Scheduler::new(urls);
    let selector = match cfg.mode {
        ExtractMode::MainContent => Selector::with_defaults(),
        ExtractMode::Baseline => Selector::baseline_only(),
    };
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                while let Some((i, origin)) = sched.take(cancel) {
                    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        read_one(&urls[i], cfg, net, &selector, cancel)
                    }))
                    .unwrap_or_else(|_| Outcome::Unreadable {
                        url: urls[i].clone(),
                        status: 0,
                        kind: "panic",
                        detail: "reading this page panicked (recovered)".into(),
                    });
                    *slots[i].lock().unwrap_or_else(|p| p.into_inner()) = Some(out);
                    sched.done(&origin);
                }
            });
        }
    });
    slots
        .into_iter()
        .zip(urls)
        .map(|(s, u)| {
            s.into_inner()
                .unwrap_or_else(|p| p.into_inner())
                .unwrap_or_else(|| Outcome::Unreachable { url: u.clone(), detail: "not attempted".into() })
        })
        .collect()
}

/// Fetches one URL through the gate (each hop admitted) and reads it.
pub fn read_one(
    url: &str,
    cfg: &CorpusConfig,
    net: &dyn PageFetch,
    selector: &Selector,
    cancel: &CancelToken,
) -> Outcome {
    let unreachable = |detail: String| Outcome::Unreachable { url: url.to_string(), detail };
    if !url.trim_start().to_ascii_lowercase().starts_with("https://") {
        return Outcome::Refused { url: url.to_string(), reason: Refusal::NotHttp("https only".into()) };
    }
    if let Err(e) = Target::parse(url) {
        return unreachable(e.to_string());
    }
    let mut current = url.trim().to_string();
    let mut retried = false;
    let mut hops = 0;
    let res = loop {
        if cancel.is_cancelled() {
            return Outcome::Refused { url: url.to_string(), reason: Refusal::Cancelled };
        }
        if let Err(reason) = cfg.gate.admit(&current, cfg.robots_http.as_ref(), cancel) {
            return Outcome::Refused { url: url.to_string(), reason };
        }
        let res = match net.fetch(&current, cancel) {
            Ok(r) => r,
            // After a redirect the URL is the server's choice: report it sanitised, never verbatim.
            Err(_) if hops > 0 => {
                return unreachable(format!(
                    "redirected {}, which could not be reached",
                    sanitize_location(&current).render()
                ))
            }
            Err(e) => return unreachable(e),
        };
        if matches!(res.status, 429 | 503) && !retried {
            if let Some(wait) = res.retry_after.filter(|w| *w <= MAX_RETRY_AFTER) {
                retried = true;
                if !sleep_cancellable(wait, cancel) {
                    return Outcome::Refused { url: url.to_string(), reason: Refusal::Cancelled };
                }
                continue;
            }
        }
        if (300..400).contains(&res.status) {
            let Some(loc) = res.location.as_deref() else { break res };
            match next_hop(&current, loc) {
                Some(next) if hops < MAX_REDIRECTS => {
                    hops += 1;
                    current = next;
                    continue;
                }
                _ => {
                    return Outcome::Redirect {
                        url: url.to_string(),
                        status: res.status,
                        location: sanitize_location(loc),
                    }
                }
            }
        }
        break res;
    };
    if res.status >= 400 {
        return Outcome::Status { url: url.to_string(), status: res.status };
    }
    read_bytes(url, &current, res, cfg, selector, cancel)
}

fn sleep_cancellable(d: Duration, cancel: &CancelToken) -> bool {
    let until = Instant::now() + d;
    while Instant::now() < until {
        if cancel.is_cancelled() {
            return false;
        }
        std::thread::sleep((until - Instant::now()).min(Duration::from_millis(100)));
    }
    true
}

/// Reads fetched bytes: HTML through the selector (rendered when thin and client-side), anything
/// else through `marlowe_extract::extract`.
fn read_bytes(
    url: &str,
    final_url: &str,
    res: RawResponse,
    cfg: &CorpusConfig,
    selector: &Selector,
    cancel: &CancelToken,
) -> Outcome {
    let ct = res.content_type.as_deref();
    let format = marlowe_extract::sniff::detect(&res.bytes, ct, Some(final_url));
    let unreadable = |e: marlowe_extract::ExtractError| Outcome::Unreadable {
        url: url.to_string(),
        status: res.status,
        kind: e.kind(),
        detail: e.to_string(),
    };
    let mut page = Page {
        url: url.to_string(),
        final_url: final_url.to_string(),
        status: res.status,
        content_type: res.content_type.clone(),
        document: marlowe_extract::extract(&Input::new(b"")).expect("empty input reads"),
        markdown: String::new(),
        doc: DocRef::of(&res.bytes),
        reading: None,
        rendered: false,
        wire_bytes: res.wire_bytes,
    };
    if format != marlowe_extract::Format::Html {
        match marlowe_extract::extract(&Input::new(&res.bytes).content_type(ct).url(Some(final_url))) {
            Ok(d) => {
                page.markdown = d.text.clone();
                page.document = d;
                return Outcome::Read(Box::new(page));
            }
            Err(e) => return unreadable(e),
        }
    }
    let m = match selector.read(&res.bytes, ct, Some(final_url)) {
        Ok(m) => m,
        Err(e) => return unreadable(e),
    };
    if m.client_rendered {
        // The server HTML is an app shell: what a reader sees needs a browser, or it is unreadable.
        return match render_into(&mut page, final_url, cfg, selector, cancel) {
            Ok(()) => Outcome::Read(Box::new(page)),
            Err(why) => Outcome::Unreadable {
                url: url.to_string(),
                status: res.status,
                kind: CLIENT_RENDERED,
                detail: why.to_string(),
            },
        };
    }
    fill(&mut page, m);
    Outcome::Read(Box::new(page))
}

fn fill(page: &mut Page, m: marlowe_extract::main_content::MainText) {
    page.reading = Some(Reading {
        candidate: m.choice.chosen.candidate,
        reason: m.choice.reason,
        page_type: m.choice.page_type,
        confidence: m.choice.chosen.confidence,
        coverage: m.coverage,
    });
    page.markdown = m.markdown;
    page.document = m.document;
}

/// `Outcome::Unreadable::kind` for an app shell that could not be rendered.
pub const CLIENT_RENDERED: &str = "client_rendered";

/// Why a client-rendered page was not rendered. Harness-written text only: nothing here quotes
/// the page or the browser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotRendered {
    NoBrowser,
    Refused,
    Browser(&'static str),
    StillThin,
    Unreadable,
}

impl std::fmt::Display for NotRendered {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the page is rendered client-side and ")?;
        match self {
            NotRendered::NoBrowser => f.write_str("no browser is available (Microsoft Edge not found)"),
            NotRendered::Refused => f.write_str("the host refused a second request, or the page left the public web"),
            NotRendered::Browser(kind) => write!(f, "the browser could not render it ({kind})"),
            NotRendered::StillThin => f.write_str("it was still thin after rendering"),
            NotRendered::Unreadable => f.write_str("the rendered HTML could not be read"),
        }
    }
}

/// Renders `url` in the browser and reads the result. Admitted through the gate again, so the
/// second request to the host is spaced like any other.
fn render_into(
    page: &mut Page,
    url: &str,
    cfg: &CorpusConfig,
    selector: &Selector,
    cancel: &CancelToken,
) -> Result<(), NotRendered> {
    let Some(r) = cfg.renderer.as_ref() else {
        return Err(NotRendered::NoBrowser);
    };
    cfg.gate.admit(url, cfg.robots_http.as_ref(), cancel).map_err(|_| NotRendered::Refused)?;
    let rendered = r.render(url, cancel).map_err(|e| match e {
        crate::render::RenderError::Refused => NotRendered::Refused,
        e => NotRendered::Browser(e.kind()),
    })?;
    let m = selector
        .read(rendered.html.as_bytes(), Some("text/html; charset=utf-8"), Some(&rendered.final_url))
        .map_err(|_| NotRendered::Unreadable)?;
    if m.choice.chosen.is_thin() {
        return Err(NotRendered::StillThin);
    }
    page.doc = DocRef::of(rendered.html.as_bytes());
    page.rendered = true;
    page.final_url = rendered.final_url;
    fill(page, m);
    Ok(())
}

/// One page for a reader: title, then warnings that change what the text means, then markdown.
pub fn render(page: &Page) -> String {
    let mut out = String::with_capacity(page.markdown.len() + 256);
    if let Some(t) = &page.document.title {
        out.push_str(&format!("# {t}\n\n"));
    }
    for w in &page.document.warnings {
        if !matches!(w, marlowe_extract::Warning::Recovered { .. }) {
            out.push_str(&format!("[note: {w}]\n"));
        }
    }
    if !out.is_empty() && !out.ends_with("\n\n") {
        out.push('\n');
    }
    out.push_str(&page.markdown);
    out
}

#[cfg(test)]
#[path = "corpus_tests.rs"]
mod tests;
