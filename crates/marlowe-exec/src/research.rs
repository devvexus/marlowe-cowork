//! Research tools: `arxiv` (export.arxiv.org Atom API) and `paper` (a PDF/HTML paper by URL or
//! arXiv id, read through marlowe-extract). Both return untrusted content (the host taints).
//!
//! arXiv asks for at most one request per 3 s; one process-wide limiter covers every arXiv
//! request. Search results are cached on disk for 24 h, keyed by the query.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use marlowe_agent::ToolOutcome;
use marlowe_extract::Input;
use marlowe_model::CancelToken;
use regex::Regex;
use serde_json::Value;

use crate::text::floor_boundary;
use crate::{int_arg, str_arg};

pub const ARXIV_API: &str = "https://export.arxiv.org/api/query";
pub const ARXIV_MIN_GAP: Duration = Duration::from_secs(3);
pub const CACHE_TTL: Duration = Duration::from_secs(24 * 3600);
pub const MAX_RESULTS: usize = 25;
pub const DEFAULT_RESULTS: usize = 8;
/// Characters of a paper returned per call.
pub const PAPER_WINDOW_CHARS: usize = 12_000;
const SUMMARY_CHARS: usize = 700;
const MAX_SECTIONS: usize = 40;

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

/// Real network through marlowe-net (https only, redirects followed).
pub struct NetHttp;

impl HttpGet for NetHttp {
    fn get(&self, url: &str, cancel: &CancelToken) -> Result<HttpResponse, String> {
        match crate::web::fetch_blocking(url.to_string(), cancel) {
            Ok((final_url, f)) => Ok(HttpResponse {
                status: f.status,
                content_type: f.content_type,
                bytes: f.bytes,
                final_url,
            }),
            Err(o) => Err(o.content),
        }
    }
}

/// Spaces requests at least `gap` apart, across threads.
pub struct RateLimiter {
    gap: Duration,
    next: Mutex<Option<Instant>>,
}

impl RateLimiter {
    pub fn new(gap: Duration) -> Self {
        Self { gap, next: Mutex::new(None) }
    }

    /// The limiter shared by every arXiv request in this process.
    pub fn arxiv_global() -> Arc<RateLimiter> {
        static G: OnceLock<Arc<RateLimiter>> = OnceLock::new();
        G.get_or_init(|| Arc::new(RateLimiter::new(ARXIV_MIN_GAP))).clone()
    }

    /// Blocks until this caller's slot. False if cancelled while waiting (the slot is forfeited).
    pub fn wait(&self, cancel: &CancelToken) -> bool {
        let slot = {
            let mut next = self.next.lock().unwrap_or_else(|e| e.into_inner());
            let now = Instant::now();
            let slot = next.map_or(now, |n| n.max(now));
            *next = Some(slot + self.gap);
            slot
        };
        loop {
            if cancel.is_cancelled() {
                return false;
            }
            let now = Instant::now();
            if now >= slot {
                return true;
            }
            std::thread::sleep((slot - now).min(Duration::from_millis(50)));
        }
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

/// One arXiv entry.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ArxivEntry {
    pub id: String,
    pub title: String,
    pub authors: Vec<String>,
    pub summary: String,
    pub published: String,
    pub category: String,
    pub abs_url: String,
    pub pdf_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ArxivFeed {
    pub total: Option<u64>,
    pub entries: Vec<ArxivEntry>,
    /// arXiv reports a malformed request as an entry titled "Error".
    pub error: Option<String>,
}

/// Parses an arXiv Atom response.
pub fn parse_atom(xml: &str) -> ArxivFeed {
    let mut feed = ArxivFeed::default();
    let head_end = xml.find("<entry").unwrap_or(xml.len());
    feed.total = inner(&xml[..head_end], "opensearch:totalResults").and_then(|t| t.trim().parse().ok());
    let mut rest = xml;
    while let Some(i) = rest.find("<entry") {
        let after = &rest[i..];
        let Some(end) = after.find("</entry>") else { break };
        let block = &after[..end];
        rest = &after[end + 8..];
        let title = clean(&inner(block, "title").unwrap_or_default());
        let id_url = clean(&inner(block, "id").unwrap_or_default());
        if id_url.contains("/api/errors") || title == "Error" {
            feed.error = Some(clean(&inner(block, "summary").unwrap_or_default()));
            continue;
        }
        let mut e = ArxivEntry {
            id: id_from_url(&id_url),
            title,
            summary: clean(&inner(block, "summary").unwrap_or_default()),
            published: clean(&inner(block, "published").unwrap_or_default()),
            abs_url: id_url.replace("http://", "https://"),
            ..Default::default()
        };
        let mut a = block;
        while let Some(i) = a.find("<author>") {
            let Some(j) = a[i..].find("</author>") else { break };
            if let Some(n) = inner(&a[i..i + j], "name") {
                e.authors.push(clean(&n));
            }
            a = &a[i + j + 9..];
        }
        for tag in tags(block, "link") {
            let href = attr(tag, "href").unwrap_or_default().replace("http://", "https://");
            if attr(tag, "title").as_deref() == Some("pdf") || attr(tag, "type").as_deref() == Some("application/pdf") {
                e.pdf_url = href;
            }
        }
        if e.pdf_url.is_empty() && !e.id.is_empty() {
            e.pdf_url = format!("https://arxiv.org/pdf/{}", e.id);
        }
        e.category = tags(block, "arxiv:primary_category")
            .first()
            .and_then(|t| attr(t, "term"))
            .unwrap_or_default();
        feed.entries.push(e);
    }
    feed
}

/// `http://arxiv.org/abs/2301.01234v2` -> `2301.01234v2` (old style keeps its archive prefix).
fn id_from_url(url: &str) -> String {
    url.split_once("/abs/").map_or(url, |(_, id)| id).to_string()
}

/// Text between `<name ...>` and `</name>`.
fn inner(s: &str, name: &str) -> Option<String> {
    let open = format!("<{name}");
    let mut from = 0;
    while let Some(i) = s[from..].find(&open) {
        let start = from + i + open.len();
        match s[start..].chars().next() {
            Some('>') | Some(' ') | Some('\n') | Some('\r') | Some('\t') => {
                let gt = s[start..].find('>')? + start;
                if s[..gt].ends_with('/') {
                    return Some(String::new());
                }
                let close = format!("</{name}>");
                let end = s[gt..].find(&close)? + gt;
                return Some(decode_entities(&s[gt + 1..end]));
            }
            _ => from = start,
        }
    }
    None
}

/// Every `<name .../>` or `<name ...>` opening tag (the text between the brackets).
fn tags<'a>(s: &'a str, name: &str) -> Vec<&'a str> {
    let open = format!("<{name}");
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(i) = s[from..].find(&open) {
        let start = from + i;
        let after = start + open.len();
        let Some(gt) = s[after..].find('>') else { break };
        if matches!(s[after..].chars().next(), Some(' ') | Some('/') | Some('>') | Some('\n') | Some('\t')) {
            out.push(&s[start + 1..after + gt]);
        }
        from = after + gt;
    }
    out
}

fn attr(tag: &str, name: &str) -> Option<String> {
    let key = format!("{name}=");
    let mut from = 0;
    while let Some(i) = tag[from..].find(&key) {
        let at = from + i;
        let boundary = at == 0 || tag.as_bytes()[at - 1].is_ascii_whitespace();
        let q = tag[at + key.len()..].chars().next()?;
        if boundary && (q == '"' || q == '\'') {
            let v = &tag[at + key.len() + 1..];
            return v.find(q).map(|e| decode_entities(&v[..e]));
        }
        from = at + key.len();
    }
    None
}

fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let decoded = tail.find(';').filter(|e| *e <= 10).and_then(|e| {
            let ent = &tail[1..e];
            let ch = match ent {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                _ => ent
                    .strip_prefix("#x")
                    .and_then(|h| u32::from_str_radix(h, 16).ok())
                    .or_else(|| ent.strip_prefix('#').and_then(|d| d.parse().ok()))
                    .and_then(char::from_u32),
            }?;
            Some((ch, e + 1))
        });
        match decoded {
            Some((ch, n)) => {
                out.push(ch);
                rest = &tail[n..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Collapses whitespace runs (arXiv hard-wraps titles and abstracts).
fn clean(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn percent_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b':' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// The request URL for a query; a bare arXiv id becomes an `id_list` lookup.
pub fn arxiv_url(query: &str, max: usize) -> String {
    let q = query.trim();
    if let Some(id) = arxiv_id(q) {
        return format!("{ARXIV_API}?id_list={}&max_results=1", percent_encode(&id));
    }
    const FIELDS: [&str; 7] = ["all:", "ti:", "au:", "abs:", "cat:", "co:", "id:"];
    let search = if FIELDS.iter().any(|f| q.starts_with(f)) { q.to_string() } else { format!("all:{q}") };
    format!(
        "{ARXIV_API}?search_query={}&start=0&max_results={max}&sortBy=relevance&sortOrder=descending",
        percent_encode(&search)
    )
}

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
fn feed_for(cfg: &ResearchConfig, url: &str, cancel: &CancelToken) -> Result<(ArxivFeed, bool), String> {
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
    let max = int_arg(args, "max").map_or(DEFAULT_RESULTS, |m| (m.max(1) as usize).min(MAX_RESULTS));
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
    out.push_str("\nRead one in full with `paper` (an id or URL). Abstracts are the authors' claims, not findings.");
    out
}

// ---------------------------------------------------------------- paper

pub(crate) fn paper_url(input: &str) -> Result<String, String> {
    let s = input.trim();
    if s.is_empty() {
        return Err("`url_or_arxiv_id` is required: an https URL or an arXiv id such as 2301.01234".into());
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

pub(crate) fn paper(cfg: &ResearchConfig, args: &Value, cancel: &CancelToken) -> ToolOutcome {
    let input = str_arg(args, "url_or_arxiv_id")
        .or_else(|| str_arg(args, "url"))
        .or_else(|| str_arg(args, "id"))
        .unwrap_or("");
    let url = match paper_url(input) {
        Ok(u) => u,
        Err(e) => return ToolOutcome::error(e),
    };
    let offset = int_arg(args, "offset").unwrap_or(0).max(0) as usize;
    let arxiv = arxiv_id(input);
    // arXiv ids also get title and authors from the API (cached; a failure is not fatal).
    let meta = arxiv.as_ref().and_then(|id| {
        feed_for(cfg, &arxiv_url(id, 1), cancel).ok().and_then(|(f, _)| f.entries.into_iter().next())
    });
    if arxiv.is_some() && !cfg.limiter.wait(cancel) {
        return ToolOutcome::error("interrupted");
    }
    let res = match cfg.http.get(&url, cancel) {
        Ok(r) => r,
        Err(e) => return ToolOutcome::error(format!("could not fetch {url}: {e}")),
    };
    if res.status >= 400 {
        return ToolOutcome::error(format!("{url} returned HTTP {}.", res.status));
    }
    let doc = match marlowe_extract::extract(
        &Input::new(&res.bytes).content_type(res.content_type.as_deref()).url(Some(&res.final_url)),
    ) {
        Ok(d) => d,
        Err(e) => {
            return ToolOutcome::error(format!(
                "{url} returned {} bytes that could not be turned into text ({}).",
                res.bytes.len(),
                e.kind()
            ))
        }
    };
    let title = meta.as_ref().map(|m| m.title.clone()).or_else(|| doc.title.clone().filter(|t| !t.trim().is_empty()));
    let authors = meta.as_ref().map(|m| m.authors.clone()).unwrap_or_default();
    let mut sections: Vec<String> = doc.headings.iter().map(|h| h.text.trim().to_string()).collect();
    if sections.is_empty() {
        sections = find_sections(&doc.text);
    }
    sections.truncate(MAX_SECTIONS);
    render_paper(&res.final_url, doc.format.as_str(), title.as_deref(), &authors, &sections, &doc.text, offset, &doc.warnings)
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

#[allow(clippy::too_many_arguments)]
fn render_paper(
    url: &str,
    format: &str,
    title: Option<&str>,
    authors: &[String],
    sections: &[String],
    text: &str,
    offset: usize,
    warnings: &[marlowe_extract::Warning],
) -> ToolOutcome {
    let total = text.chars().count();
    let start = text.char_indices().nth(offset).map_or(text.len(), |(i, _)| i);
    let rest = &text[start..];
    let end = rest.char_indices().nth(PAPER_WINDOW_CHARS).map_or(rest.len(), |(i, _)| i);
    let window = &rest[..floor_boundary(rest, end)];
    let shown = window.chars().count();

    let mut out = format!("paper {url} · {format}\n");
    if let Some(t) = title {
        out.push_str(&format!("Title: {t}\n"));
    }
    if !authors.is_empty() {
        out.push_str(&format!("Authors: {}\n", authors.join(", ")));
    }
    if !sections.is_empty() {
        out.push_str(&format!("Sections: {}\n", sections.join(" | ")));
    }
    out.push('\n');
    out.push_str(window);
    if offset + shown < total {
        out.push_str(&format!(
            "\n\n[showing characters {offset}-{} of {total}. Continue with offset {}.]",
            offset + shown,
            offset + shown
        ));
    }
    for w in warnings {
        out.push_str(&format!("\n[{w}]"));
    }
    ToolOutcome::ok(out, format!("{total} chars"))
}

#[cfg(test)]
mod tests {
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
    fn entities_decode() {
        assert_eq!(decode_entities("a &amp; b &lt;c&gt; &#233; &#x41; &bogus; &"), "a & b <c> é A &bogus; &");
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
