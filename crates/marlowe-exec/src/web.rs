//! web_fetch (one page through the corpus reader: robots.txt, host spacing, main content as
//! markdown, a browser for client-rendered pages) and web_search (injected backend).
//!
//! T20: `http://` is upgraded to `https://`; a redirect to another host is returned, not
//! followed; a page read in the last 15 minutes comes from the cache; with `prompt` the question
//! is answered from the page by the session's [`WebDigest`] and the page is saved to a file
//! instead of entering the conversation (`raw: true` returns the page itself). T21:
//! `allowed_domains` / `blocked_domains` filter what a search may return.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use marlowe_agent::{CallCtx, DigestRequest, ToolOutcome, WebDigest};
use marlowe_extract::main_content::Selector;
use marlowe_model::CancelToken;
use serde_json::Value;

use crate::corpus::{CorpusConfig, Outcome, Page, PageFetch};
use crate::text::floor_boundary;
use crate::{int_arg, str_arg, BuiltinTools};

/// How long a fetched page is served from the cache (Claude Code's 15 minutes).
pub const FETCH_CACHE_TTL: Duration = Duration::from_secs(15 * 60);

/// Pages kept in the cache; the oldest goes first.
const FETCH_CACHE_MAX: usize = 64;

/// Characters of a page a digest model is given.
pub const DIGEST_PAGE_CHARS: usize = 60_000;

/// Characters of a fetched document returned per call.
pub const FETCH_WINDOW_CHARS: usize = 24_000;

/// Wall-clock limit on one web_fetch, redirects included. marlowe-net's timeout is per read.
pub const FETCH_DEADLINE: Duration = Duration::from_secs(60);

/// How often a waiting fetch checks for cancel.
const POLL: Duration = Duration::from_millis(50);

/// One search hit with its citation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// What a search backend returns: an optional synthesised answer plus cited results.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchResponse {
    pub answer: String,
    pub results: Vec<SearchResult>,
}

/// The search port. The bin wires the keyless web channel (SearXNG, DuckDuckGo lite); closures of the
/// right shape implement it.
pub trait SearchBackend: Send + Sync {
    fn search(&self, query: &str, cancel: &CancelToken) -> Result<SearchResponse, String>;

    /// A search limited to `domains`. The default narrows the query with `site:` terms; either
    /// way [`web_search`] filters the results again, so a backend cannot leak a blocked domain.
    fn search_in(&self, query: &str, domains: &DomainFilter, cancel: &CancelToken) -> Result<SearchResponse, String> {
        self.search(&domains.narrow(query), cancel)
    }
}

/// `web_search{allowed_domains, blocked_domains}` (T21). A domain matches itself and its
/// subdomains; `www.` and a scheme or path given with it are ignored.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DomainFilter {
    pub allowed: Vec<String>,
    pub blocked: Vec<String>,
}

impl DomainFilter {
    pub fn from_args(args: &Value) -> Self {
        let list = |key: &str| -> Vec<String> {
            let mut v: Vec<String> = match args.get(key) {
                Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).filter_map(normal_domain).collect(),
                Some(Value::String(s)) => s.split(',').filter_map(normal_domain).collect(),
                _ => Vec::new(),
            };
            v.dedup();
            v
        };
        Self { allowed: list("allowed_domains"), blocked: list("blocked_domains") }
    }

    pub fn is_empty(&self) -> bool {
        self.allowed.is_empty() && self.blocked.is_empty()
    }

    /// Whether a result URL may be returned.
    pub fn admits(&self, url: &str) -> bool {
        let Some(host) = url_host(url) else { return false };
        let within = |d: &String| host == *d || host.ends_with(&format!(".{d}"));
        (self.allowed.is_empty() || self.allowed.iter().any(within)) && !self.blocked.iter().any(within)
    }

    /// The query with `site:` terms for a search engine.
    pub fn narrow(&self, query: &str) -> String {
        let mut q = query.to_string();
        if !self.allowed.is_empty() {
            let sites: Vec<String> = self.allowed.iter().map(|d| format!("site:{d}")).collect();
            q.push_str(&format!(" ({})", sites.join(" OR ")));
        }
        for d in &self.blocked {
            q.push_str(&format!(" -site:{d}"));
        }
        q
    }
}

fn normal_domain(raw: &str) -> Option<String> {
    let d = raw.trim().to_ascii_lowercase();
    let d = d.split("://").last().unwrap_or("");
    let d = d.split(['/', '?', '#']).next().unwrap_or("").trim_start_matches("*.").trim_start_matches("www.").trim_matches('.');
    (!d.is_empty() && d.contains('.') && d.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')).then(|| d.to_string())
}

fn url_host(url: &str) -> Option<String> {
    let rest = url.trim().split_once("://")?.1;
    let host = rest.split(['/', '?', '#']).next()?.rsplit('@').next()?;
    let host = host.split(':').next()?.to_ascii_lowercase();
    Some(host.trim_start_matches("www.").to_string()).filter(|h| !h.is_empty())
}

impl<F> SearchBackend for F
where
    F: Fn(&str, &CancelToken) -> Result<SearchResponse, String> + Send + Sync,
{
    fn search(&self, query: &str, cancel: &CancelToken) -> Result<SearchResponse, String> {
        self(query, cancel)
    }
}

pub(crate) fn web_search(backend: &dyn SearchBackend, args: &Value, cancel: &CancelToken) -> ToolOutcome {
    let Some(query) = str_arg(args, "query").map(str::trim).filter(|q| !q.is_empty()) else {
        return ToolOutcome::error("`query` is required");
    };
    let filter = DomainFilter::from_args(args);
    let found = if filter.is_empty() { backend.search(query, cancel) } else { backend.search_in(query, &filter, cancel) };
    match found {
        Err(e) => ToolOutcome::error(format!("web search failed: {e}")),
        Ok(mut r) => {
            let before = r.results.len();
            r.results.retain(|s| filter.admits(&s.url));
            let mut out = String::new();
            if before > r.results.len() {
                // The summary may quote a source the filter removed, so only the sources are shown.
                out.push_str(&format!(
                    "[{} result(s) outside the domain filter were removed, and the search's summary with them.]\n\n",
                    before - r.results.len()
                ));
            } else if !r.answer.trim().is_empty() {
                out.push_str(r.answer.trim());
                out.push_str("\n\n");
            }
            if r.results.is_empty() {
                out.push_str("[no cited results]");
            } else {
                out.push_str("Sources:\n");
                for (i, s) in r.results.iter().enumerate() {
                    out.push_str(&format!("{}. {} — {}\n", i + 1, s.title, s.url));
                    if !s.snippet.is_empty() {
                        out.push_str(&format!("   {}\n", s.snippet.trim()));
                    }
                }
            }
            ToolOutcome::ok(out.trim_end().to_string(), format!("{} sources", r.results.len()))
        }
    }
}

/// Why waiting for a worker ended without a result.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Stopped {
    Cancelled,
    Deadline,
    WorkerLost,
}

/// Waits for `rx`, giving up on cancel or at `deadline`. An abandoned worker finishes detached.
pub(crate) fn wait_for<T>(rx: &Receiver<T>, cancel: &CancelToken, deadline: Instant) -> Result<T, Stopped> {
    loop {
        match rx.recv_timeout(POLL) {
            Ok(v) => return Ok(v),
            Err(RecvTimeoutError::Disconnected) => return Err(Stopped::WorkerLost),
            Err(RecvTimeoutError::Timeout) => {
                if cancel.is_cancelled() {
                    return Err(Stopped::Cancelled);
                }
                if Instant::now() >= deadline {
                    return Err(Stopped::Deadline);
                }
            }
        }
    }
}

/// Per-host web state: where `web_fetch{prompt}` saves pages (the profile home's cache).
#[derive(Debug, Default)]
pub struct WebState {
    pub home: Option<PathBuf>,
}

/// What one `web_fetch` reads through; the tool passes the product setup, tests their fakes.
pub(crate) struct FetchDeps<'a> {
    pub cfg: CorpusConfig,
    pub net: &'a dyn PageFetch,
    pub digest: Option<WebDigest>,
    /// Where pages answered by a digest are saved.
    pub blob_dir: PathBuf,
}

pub(crate) fn web_fetch(t: &BuiltinTools, args: &Value, c: &CallCtx) -> ToolOutcome {
    let cfg = CorpusConfig { same_host_redirects_only: true, ..CorpusConfig::default() };
    let home = t.state::<WebState>().home.clone();
    let blob_dir = home.map(|h| h.join("cache").join("web")).unwrap_or_else(|| std::env::temp_dir().join("marlowe-web"));
    let deps = FetchDeps { cfg, net: &crate::corpus::NetPages, digest: c.scope.services.get::<WebDigest>().cloned(), blob_dir };
    fetch_with(args, &deps, &c.cancel)
}

/// `http://` becomes `https://` (Claude Code's upgrade); everything else is left for the reader
/// to refuse.
fn upgrade_http(url: &str) -> String {
    match url.get(..7) {
        Some(p) if p.eq_ignore_ascii_case("http://") => format!("https://{}", &url[7..]),
        _ => url.to_string(),
    }
}

struct Cached {
    at: Instant,
    page: Arc<Page>,
}

fn cache() -> &'static Mutex<HashMap<String, Cached>> {
    static C: OnceLock<Mutex<HashMap<String, Cached>>> = OnceLock::new();
    C.get_or_init(Mutex::default)
}

fn cache_get(url: &str) -> Option<(Arc<Page>, Duration)> {
    let mut c = cache().lock().unwrap_or_else(|e| e.into_inner());
    c.retain(|_, v| v.at.elapsed() < FETCH_CACHE_TTL);
    c.get(url).map(|v| (v.page.clone(), v.at.elapsed()))
}

fn cache_put(url: &str, page: Arc<Page>) {
    let mut c = cache().lock().unwrap_or_else(|e| e.into_inner());
    if c.len() >= FETCH_CACHE_MAX {
        if let Some(oldest) = c.iter().min_by_key(|(_, v)| v.at).map(|(k, _)| k.clone()) {
            c.remove(&oldest);
        }
    }
    c.insert(url.to_string(), Cached { at: Instant::now(), page });
}

pub(crate) fn fetch_with(args: &Value, deps: &FetchDeps<'_>, cancel: &CancelToken) -> ToolOutcome {
    let Some(asked) = str_arg(args, "url").map(str::trim).filter(|u| !u.is_empty()) else {
        return ToolOutcome::error("`url` is required: a full https:// URL");
    };
    let url = upgrade_http(asked);
    let offset = int_arg(args, "offset").unwrap_or(0).max(0) as usize;
    let prompt = str_arg(args, "prompt").map(str::trim).filter(|p| !p.is_empty());
    let raw = args.get("raw").and_then(Value::as_bool).unwrap_or(false);

    let (page, age) = match cache_get(&url) {
        Some((page, age)) => (page, Some(age)),
        None => {
            let selector = Selector::with_defaults();
            match crate::corpus::read_one(&url, &deps.cfg, deps.net, &selector, cancel) {
                Outcome::Read(page) => {
                    let page: Arc<Page> = Arc::from(page);
                    cache_put(&url, page.clone());
                    (page, None)
                }
                Outcome::Redirect { status, location, cross_host: true, .. } => {
                    let to = match &location {
                        crate::corpus::RedirectTo::Url(u) => u.clone(),
                        other => other.render(),
                    };
                    return ToolOutcome::ok(
                        format!(
                            "REDIRECT: {url} answered HTTP {status} with a redirect to another host: {to}\n\n\
                             web_fetch does not follow a redirect to a different host. To read it, call web_fetch \
                             again with that URL (and the same prompt, if you gave one)."
                        ),
                        format!("redirect to another host ({status})"),
                    );
                }
                Outcome::Refused { reason: marlowe_research::Refusal::Cancelled, .. } => {
                    return ToolOutcome {
                        content: "web_fetch stopped: the turn was interrupted.".into(),
                        is_error: true,
                        summary: "interrupted".into(),
                        parts: Vec::new(),
                    }
                }
                Outcome::Unreachable { detail, .. } => return ToolOutcome::error(format!("could not fetch {url}: {detail}")),
                other => {
                    let why = other.why_not().unwrap_or_default();
                    return ToolOutcome::error(format!("{url} was not read: {why}."));
                }
            }
        }
    };
    let cached_note = age.map(|a| format!(" · from the cache ({} min old)", a.as_secs() / 60)).unwrap_or_default();
    match prompt.filter(|_| !raw) {
        Some(prompt) => answer_from(&page, prompt, offset, deps, &cached_note, cancel),
        None => {
            let mut o = render_page(&page, offset);
            if !cached_note.is_empty() {
                if let Some(nl) = o.content.find('\n') {
                    o.content.insert_str(nl, &cached_note);
                }
                o.summary.push_str(" · cached");
            }
            o
        }
    }
}

/// `prompt` answered from the page by the digest model; the page goes to a file, not the
/// conversation. Without a digest model the page window is returned with a note.
fn answer_from(page: &Page, prompt: &str, offset: usize, deps: &FetchDeps<'_>, cached_note: &str, cancel: &CancelToken) -> ToolOutcome {
    let Some(digest) = &deps.digest else {
        let mut o = render_page(page, offset);
        o.content = format!("[No model is available to answer the prompt here, so this is the page itself.]\n\n{}", o.content);
        return o;
    };
    let body = crate::corpus::render(page);
    let total = body.chars().count();
    let cut = body.char_indices().nth(DIGEST_PAGE_CHARS).map_or(body.len(), |(i, _)| i);
    let mut text = body[..floor_boundary(&body, cut)].to_string();
    if cut < body.len() {
        text.push_str(&format!("\n[page cut at {DIGEST_PAGE_CHARS} of {total} characters]"));
    }
    let saved = save_page(&deps.blob_dir, page, &body);
    let req = DigestRequest { url: &page.final_url, prompt, page: &text };
    match digest.answer(&req, cancel) {
        Ok(answer) => {
            let mut out = format!("{} · answer to the prompt{cached_note}\n\n{}\n\n", page.final_url, answer.trim());
            match &saved {
                Ok(path) => out.push_str(&format!(
                    "[The page ({total} characters) is not in the conversation. It is saved at {}; `read` or `grep` it, or call web_fetch with raw: true for the page itself.]",
                    path.display()
                )),
                Err(_) => out.push_str(&format!(
                    "[The page ({total} characters) is not in the conversation; call web_fetch with raw: true for the page itself.]"
                )),
            }
            ToolOutcome::ok(out, format!("answered from {total} chars"))
        }
        Err(e) => {
            let mut o = render_page(page, offset);
            o.content = format!("[The prompt could not be answered ({e}), so this is the page itself.]\n\n{}", o.content);
            o
        }
    }
}

fn save_page(dir: &std::path::Path, page: &Page, body: &str) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let name: String = page.doc.as_str().chars().take(16).collect();
    let path = dir.join(format!("{name}.md"));
    std::fs::write(&path, format!("<!-- {} -->\n{body}", page.final_url))?;
    Ok(path)
}

/// A page for the model: where it came from, how it was read, then a window of its markdown.
fn render_page(page: &Page, offset: usize) -> ToolOutcome {
    let body = crate::corpus::render(page);
    let total = body.chars().count();
    let start_byte = body.char_indices().nth(offset).map_or(body.len(), |(i, _)| i);
    let rest = &body[start_byte..];
    let window_end = rest.char_indices().nth(FETCH_WINDOW_CHARS).map_or(rest.len(), |(i, _)| i);
    let window = &rest[..floor_boundary(rest, window_end)];
    let shown = window.chars().count();

    let mut out = format!("{} · {}", page.final_url, page.document.format.as_str());
    if page.rendered {
        out.push_str(" · rendered in a browser");
    }
    out.push_str("\n\n");
    out.push_str(window);
    if offset + shown < total {
        out.push_str(&format!(
            "\n\n[showing characters {offset}-{} of {total}. Continue with offset {}.]",
            offset + shown,
            offset + shown
        ));
    }
    ToolOutcome::ok(out, format!("{} · {total} chars", page.status))
}

#[cfg(test)]
#[path = "web_tests.rs"]
mod web_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use marlowe_extract::Document;
    use std::sync::mpsc::sync_channel;

    #[test]
    fn waiting_stops_on_cancel_and_at_the_deadline() {
        let (_tx, rx) = sync_channel::<()>(1);
        let cancel = CancelToken::new();
        let c = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            c.cancel();
        });
        let t0 = Instant::now();
        assert_eq!(wait_for(&rx, &cancel, t0 + Duration::from_secs(30)), Err(Stopped::Cancelled));
        assert!(t0.elapsed() < Duration::from_secs(5));
        let t0 = Instant::now();
        assert_eq!(wait_for(&rx, &CancelToken::new(), t0 + Duration::from_millis(100)), Err(Stopped::Deadline));
        let (tx, rx) = sync_channel(1);
        tx.send(7).unwrap();
        assert_eq!(wait_for(&rx, &CancelToken::new(), Instant::now()), Ok(7));
    }

    #[test]
    fn a_long_document_is_windowed_with_the_next_offset() {
        let doc = Document {
            format: marlowe_extract::Format::PlainText,
            title: Some("T".into()),
            text: "x".repeat(FETCH_WINDOW_CHARS + 500),
            links: vec![],
            headings: vec![],
            lang: None,
            description: None,
            meta: Vec::new(),
            bytes_in: 0,
            encoding: "utf-8",
            warnings: vec![],
        };
        let page = Page {
            url: "https://a.com".into(),
            final_url: "https://a.com/".into(),
            status: 200,
            content_type: None,
            markdown: doc.text.clone(),
            doc: marlowe_research::DocRef::of(b"x"),
            document: doc,
            reading: None,
            rendered: false,
            wire_bytes: 0,
        };
        let o = render_page(&page, 0);
        assert!(!o.is_error);
        assert!(o.content.starts_with("https://a.com/ · text"), "{}", &o.content[..40]);
        let total = page.markdown.chars().count() + "# T\n\n".len();
        assert!(o.content.contains(&format!("of {total}. Continue with offset {FETCH_WINDOW_CHARS}")));
        let o = render_page(&page, FETCH_WINDOW_CHARS);
        assert!(!o.content.contains("Continue"));
    }
}
