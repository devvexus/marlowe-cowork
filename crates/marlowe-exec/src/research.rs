//! Research tools: `arxiv` (export.arxiv.org Atom API) and `paper` (a paper by URL or arXiv id;
//! the reading order and parsing live in `crate::paper` and `marlowe_extract::paper`). Both return untrusted content (the host taints).
//!
//! arXiv asks for at most one request per 3 s; the process-wide arXiv limiter in marlowe-research
//! covers every arXiv request (this tool's and every research run's). Search results are cached on
//! disk for 24 h, keyed by the query. The Atom parser and query encoding are the arXiv channel's
//! (`marlowe_research::arxiv`); requests carry its User-Agent.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime};

use marlowe_agent::ToolOutcome;
use marlowe_model::CancelToken;
use marlowe_research::{ChannelId, HttpGet as _, Limiter, RateLimit};
use regex::Regex;
use serde_json::Value;

use crate::text::floor_boundary;
use crate::{int_arg, str_arg};

pub use marlowe_research::arxiv::{arxiv_url, parse_atom, ArxivEntry, ArxivFeed, ARXIV_API};
pub const ARXIV_MIN_GAP: Duration = Duration::from_secs(3);
pub const CACHE_TTL: Duration = Duration::from_secs(24 * 3600);
pub const MAX_RESULTS: usize = 25;
pub const DEFAULT_RESULTS: usize = 8;
/// Characters of a paper returned per call.
pub const PAPER_WINDOW_CHARS: usize = 12_000;
const SUMMARY_CHARS: usize = 700;

/// A completed GET.
#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub content_type: Option<String>,
    pub bytes: Vec<u8>,
    pub final_url: String,
}

/// The HTTP port the research tools use; tests inject a recorded one.
pub trait HttpGet: Send + Sync {
    fn get(&self, url: &str, cancel: &CancelToken) -> Result<HttpResponse, String>;
}

/// Real network through marlowe-research's port: marlowe-net, https only, redirects followed,
/// the research User-Agent.
pub struct NetHttp;

impl HttpGet for NetHttp {
    fn get(&self, url: &str, cancel: &CancelToken) -> Result<HttpResponse, String> {
        let r = marlowe_research::NetHttp::new().get(&marlowe_research::HttpRequest::get(url), cancel)?;
        Ok(HttpResponse { status: r.status, content_type: r.content_type, bytes: r.bytes, final_url: r.final_url })
    }
}

/// Spaces requests at least `gap` apart, across threads.
pub struct RateLimiter {
    inner: Arc<Limiter>,
}

impl RateLimiter {
    pub fn new(gap: Duration) -> Self {
        Self { inner: Arc::new(Limiter::new(&[RateLimit::every(gap)])) }
    }

    /// The arXiv limiter shared by this process: this tool and every research registry.
    pub fn arxiv_global() -> Arc<RateLimiter> {
        static G: OnceLock<Arc<RateLimiter>> = OnceLock::new();
        G.get_or_init(|| {
            let limits = marlowe_research::builtin_spec(ChannelId::Arxiv).map(|s| s.rate_limits).unwrap_or_default();
            Arc::new(RateLimiter { inner: Limiter::shared(ChannelId::Arxiv, &limits) })
        })
        .clone()
    }

    /// Blocks until this caller's slot. False if cancelled while waiting (the slot is forfeited).
    pub fn wait(&self, cancel: &CancelToken) -> bool {
        self.inner.wait(cancel)
    }
}

/// Settings for the research tools.
#[derive(Clone)]
pub struct ResearchConfig {
    /// `<home>\cache`; the arXiv cache lives in `arxiv` below it. None: no caching.
    pub cache_dir: Option<PathBuf>,
    pub ttl: Duration,
    pub limiter: Arc<RateLimiter>,
    pub http: Arc<dyn HttpGet>,
}

impl Default for ResearchConfig {
    fn default() -> Self {
        Self {
            cache_dir: None,
            ttl: CACHE_TTL,
            limiter: RateLimiter::arxiv_global(),
            http: Arc::new(NetHttp),
        }
    }
}

impl ResearchConfig {
    pub fn with_cache_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cache_dir = Some(dir.into());
        self
    }
}

// ---------------------------------------------------------------- arxiv

/// An arXiv id from a bare id, `arxiv:` form or an arxiv.org abs/pdf URL.
pub fn arxiv_id(s: &str) -> Option<String> {
    static NEW: OnceLock<Regex> = OnceLock::new();
    static OLD: OnceLock<Regex> = OnceLock::new();
    let new = NEW.get_or_init(|| Regex::new(r"^\d{4}\.\d{4,5}(v\d+)?$").unwrap());
    let old = OLD.get_or_init(|| Regex::new(r"^[a-z\-]+(\.[A-Z]{2})?/\d{7}(v\d+)?$").unwrap());
    let mut s = s.trim();
    if s.len() > 6 && s[..6].eq_ignore_ascii_case("arxiv:") {
        s = s[6..].trim();
    }
    for pre in ["https://arxiv.org/", "http://arxiv.org/", "https://www.arxiv.org/", "https://export.arxiv.org/"] {
        if let Some(r) = s.strip_prefix(pre) {
            s = r.strip_prefix("abs/").or_else(|| r.strip_prefix("pdf/"))?;
            s = s.split(['?', '#']).next().unwrap_or(s);
            s = s.strip_suffix(".pdf").unwrap_or(s);
            break;
        }
    }
    (new.is_match(s) || old.is_match(s)).then(|| s.to_string())
}

fn cache_file(dir: &Path, key: &str) -> PathBuf {
    use sha2::{Digest, Sha256};
    let h = Sha256::digest(key.as_bytes());
    let hex: String = h.iter().take(16).map(|b| format!("{b:02x}")).collect();
    dir.join("arxiv").join(format!("{hex}.xml"))
}

fn cache_read(cfg: &ResearchConfig, key: &str) -> Option<String> {
    let path = cache_file(cfg.cache_dir.as_deref()?, key);
    let age = SystemTime::now().duration_since(std::fs::metadata(&path).ok()?.modified().ok()?).unwrap_or_default();
    if age > cfg.ttl {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

fn cache_write(cfg: &ResearchConfig, key: &str, xml: &str) {
    let Some(dir) = cfg.cache_dir.as_deref() else { return };
    let path = cache_file(dir, key);
    if let Some(p) = path.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    let _ = std::fs::write(path, xml);
}

/// Feed for `url`: cache first, else a rate-limited GET. Only a parsed, error-free feed is cached.
pub(crate) fn feed_for(cfg: &ResearchConfig, url: &str, cancel: &CancelToken) -> Result<(ArxivFeed, bool), String> {
    if let Some(xml) = cache_read(cfg, url) {
        return Ok((parse_atom(&xml), true));
    }
    if !cfg.limiter.wait(cancel) {
        return Err("interrupted".into());
    }
    let res = cfg.http.get(url, cancel)?;
    if res.status >= 400 {
        return Err(format!("arXiv returned HTTP {}", res.status));
    }
    let xml = String::from_utf8_lossy(&res.bytes).into_owned();
    let feed = parse_atom(&xml);
    if feed.error.is_none() && xml.contains("<feed") {
        cache_write(cfg, url, &xml);
    } else if !xml.contains("<feed") {
        return Err("arXiv did not return an Atom feed".into());
    }
    Ok((feed, false))
}

pub(crate) fn arxiv(cfg: &ResearchConfig, args: &Value, cancel: &CancelToken) -> ToolOutcome {
    let Some(query) = str_arg(args, "query").map(str::trim).filter(|q| !q.is_empty()) else {
        return ToolOutcome::error("`query` is required: keywords, or a field query such as `ti:\"attention\" AND au:vaswani`");
    };
    let max = int_arg(args, "limit").map_or(DEFAULT_RESULTS, |m| (m.max(1) as usize).min(MAX_RESULTS));
    let url = arxiv_url(query, max);
    let (feed, cached) = match feed_for(cfg, &url, cancel) {
        Ok(f) => f,
        Err(e) => return ToolOutcome::error(format!("arxiv search failed: {e}")),
    };
    if let Some(e) = feed.error {
        return ToolOutcome::error(format!("arXiv rejected the query: {e}"));
    }
    ToolOutcome::ok(
        render_feed(query, &feed, cached),
        format!("{} papers{}", feed.entries.len(), if cached { " (cached)" } else { "" }),
    )
}

fn render_feed(query: &str, feed: &ArxivFeed, cached: bool) -> String {
    if feed.entries.is_empty() {
        return format!("arXiv: no papers for `{query}`.");
    }
    let mut out = format!(
        "arXiv: {} of {} papers for `{query}`{}\n",
        feed.entries.len(),
        feed.total.map_or("?".to_string(), |t| t.to_string()),
        if cached { " (cached result)" } else { "" }
    );
    for (i, e) in feed.entries.iter().enumerate() {
        let authors = match e.authors.len() {
            0 => "unknown authors".to_string(),
            1..=4 => e.authors.join(", "),
            n => format!("{} et al. ({n} authors)", e.authors[..3].join(", ")),
        };
        let date = e.published.get(..10).unwrap_or(&e.published);
        out.push_str(&format!("\n{}. {} — {authors} ({date}", i + 1, e.title));
        if !e.category.is_empty() {
            out.push_str(&format!(", {}", e.category));
        }
        out.push_str(&format!(")\n   id {} · {} · pdf {}\n", e.id, e.abs_url, e.pdf_url));
        let cut = floor_boundary(&e.summary, SUMMARY_CHARS);
        let ell = if cut < e.summary.len() { "…" } else { "" };
        out.push_str(&format!("   {}{ell}\n", &e.summary[..cut]));
    }
    out.push_str("\nRead one in full with `web_fetch` (its id or URL). Abstracts are the authors' claims, not findings.");
    out
}

// ---------------------------------------------------------------- paper

pub(crate) fn paper_url(input: &str) -> Result<String, String> {
    let s = input.trim();
    if s.is_empty() {
        return Err("`url` is required: an https URL or an arXiv id such as 2301.01234".into());
    }
    if let Some(id) = arxiv_id(s) {
        return Ok(format!("https://arxiv.org/pdf/{id}"));
    }
    if s.starts_with("https://") {
        Ok(s.to_string())
    } else {
        Err("give an https:// URL or an arXiv id (2301.01234, arxiv:2301.01234, cs/0601001)".into())
    }
}

/// The host a `paper` call will contact, for the egress check.
pub(crate) fn paper_host(input: &str) -> Option<String> {
    let url = paper_url(input).ok()?;
    marlowe_net::Target::parse(&url).ok().map(|t| t.host)
}

/// The `paper` tool; see `crate::paper` (HTML, LaTeX, PDF, sections and references).
pub(crate) fn paper(cfg: &ResearchConfig, args: &Value, cancel: &CancelToken) -> ToolOutcome {
    crate::paper::paper(cfg, args, cancel)
}

/// Section titles from plain text: numbered headings and the standard unnumbered ones.
pub fn find_sections(text: &str) -> Vec<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(
            r"^(?:(?:\d{1,2}(?:\.\d{1,2}){0,2}\.?|[IVX]{1,4}\.)\s+[A-Z][A-Za-z0-9 ,:\-&()/']{2,80}|(?i:abstract|introduction|related work|background|methods?|methodology|experiments?|results|discussion|conclusions?|limitations|acknowledg(?:e)?ments|references|appendix(?: [A-Z])?))$",
        )
        .unwrap()
    });
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        let l = line.trim();
        if l.len() <= 90 && re.is_match(l) && !out.iter().any(|s| s == l) {
            out.push(l.to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    #[test]
    fn ids_and_urls() {
        assert_eq!(arxiv_id("2301.01234").as_deref(), Some("2301.01234"));
        assert_eq!(arxiv_id("arXiv:2301.01234v2").as_deref(), Some("2301.01234v2"));
        assert_eq!(arxiv_id("https://arxiv.org/abs/2301.01234v2").as_deref(), Some("2301.01234v2"));
        assert_eq!(arxiv_id("https://arxiv.org/pdf/2301.01234.pdf").as_deref(), Some("2301.01234"));
        assert_eq!(arxiv_id("cs/0601001").as_deref(), Some("cs/0601001"));
        assert_eq!(arxiv_id("https://example.com/2301.01234"), None);
        assert_eq!(arxiv_id("attention is all you need"), None);
        assert!(arxiv_url("attention", 5).contains("search_query=all:attention&"));
        assert!(arxiv_url("au:del_maestro AND ti:checkerboard", 5).contains("search_query=au:del_maestro+AND+ti:checkerboard"));
        assert!(arxiv_url("2301.01234", 5).contains("id_list=2301.01234"));
        assert_eq!(paper_url("2301.01234").unwrap(), "https://arxiv.org/pdf/2301.01234");
        assert!(paper_url("http://x.org/a.pdf").is_err());
    }

    #[test]
    fn limiter_spaces_requests() {
        let l = RateLimiter::new(Duration::from_millis(120));
        let c = CancelToken::new();
        let t0 = Instant::now();
        assert!(l.wait(&c));
        assert!(l.wait(&c));
        assert!(l.wait(&c));
        assert!(t0.elapsed() >= Duration::from_millis(240));
        let c2 = CancelToken::new();
        c2.cancel();
        assert!(!l.wait(&c2));
    }

    #[test]
    fn sections_from_plain_text() {
        let t = "Title line here\nAbstract\nwords words\n1 Introduction\nblah\n2.1 Model Design\nx\nReferences\n";
        assert_eq!(find_sections(t), ["Abstract", "1 Introduction", "2.1 Model Design", "References"]);
    }
}
