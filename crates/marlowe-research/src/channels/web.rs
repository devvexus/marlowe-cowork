//! General web search over the first backend that answers: a SearXNG instance (loopback by
//! default, ADR-035), DuckDuckGo's HTML lite page, or a hosted search the daemon supplies.
//!
//! "Found nothing" and "could not look" stay distinct: an empty result list is an answer; a
//! backend that is down, refuses JSON, or challenges us is skipped and named in the error when
//! none answers. DuckDuckGo is read under its robots.txt; a challenge page (HTTP 202 or a
//! captcha form) is taken as "not now": the backend rests and nothing tries to get around it.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use marlowe_model::CancelToken;
use marlowe_proto::ChannelId;
use serde_json::Value;

use super::{clip, enc, get_raw, limit, parse_json, s, spec_of, SNIPPET_CHARS};
use crate::cache::CacheKey;
use crate::channel::{Channel, ChannelCtx, ChannelError, ChannelQuery, ChannelSpec, HttpRequest};
use crate::politeness::{HostGate, Refusal};
use crate::source::SourceRef;
use crate::xml::{decode_entities, strip_tags};

/// The SearXNG instance assumed when none is configured (ADR-035: local by default).
pub const DEFAULT_SEARXNG: &str = "http://127.0.0.1:8080";
pub const DUCKDUCKGO_LITE: &str = "https://lite.duckduckgo.com/lite/";

/// How long a backend that did not answer is skipped before it is tried again.
const DOWN_FOR: Duration = Duration::from_secs(60);
/// How long a backend rests after it served a challenge (a CAPTCHA) instead of results. A
/// challenge says automated clients are unwelcome, so the rest is long and survives restarts
/// (recorded in the research cache when one is available).
pub const CHALLENGED_FOR: Duration = Duration::from_secs(24 * 3600);
const REST_BUCKET: &str = "web-rest";

/// A hosted web search the daemon wires in (for example OpenRouter's web plugin) when its
/// credential works. The credential stays with the daemon; this crate never sees one.
pub trait HostedSearch: Send + Sync {
    /// Short display name, e.g. `openrouter`.
    fn name(&self) -> &str;
    fn search(&self, query: &str, limit: usize, cancel: &CancelToken) -> Result<Vec<SourceRef>, String>;
}

/// Where general web search goes.
#[derive(Clone)]
pub enum WebBackend {
    Searxng { base_url: String },
    DuckDuckGoLite,
    Hosted(Arc<dyn HostedSearch>),
}

impl std::fmt::Debug for WebBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WebBackend::Searxng { base_url } => write!(f, "Searxng({base_url})"),
            WebBackend::DuckDuckGoLite => f.write_str("DuckDuckGoLite"),
            WebBackend::Hosted(h) => write!(f, "Hosted({})", h.name()),
        }
    }
}

impl WebBackend {
    fn key(&self) -> String {
        match self {
            WebBackend::Searxng { base_url } => format!("searxng:{base_url}"),
            WebBackend::DuckDuckGoLite => "duckduckgo".into(),
            WebBackend::Hosted(h) => format!("hosted:{}", h.name()),
        }
    }
}

/// Web search settings (profile config).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebConfig {
    /// SearXNG base URL; loopback by default. `None` skips SearXNG.
    pub searxng_url: Option<String>,
    /// Fall back to DuckDuckGo's HTML lite page when SearXNG is unreachable.
    pub duckduckgo: bool,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self { searxng_url: Some(DEFAULT_SEARXNG.to_string()), duckduckgo: true }
    }
}

/// General web search over the first reachable backend.
pub struct WebChannel {
    spec: ChannelSpec,
    config: WebConfig,
    hosted: Option<Arc<dyn HostedSearch>>,
    gate: HostGate,
    /// Backend key -> skipped until.
    resting: Mutex<Vec<(String, Instant)>>,
}

impl WebChannel {
    pub fn new(config: WebConfig, hosted: Option<Arc<dyn HostedSearch>>) -> Self {
        let mut spec = spec_of(ChannelId::Web);
        if let Some(u) = &config.searxng_url {
            spec.endpoint = u.trim_end_matches('/').to_string();
        }
        Self { spec, config, hosted, gate: HostGate::new(), resting: Mutex::new(Vec::new()) }
    }

    pub fn config(&self) -> &WebConfig {
        &self.config
    }

    /// Backends in fallback order.
    pub fn backends(&self) -> Vec<WebBackend> {
        let mut out = Vec::new();
        if let Some(u) = &self.config.searxng_url {
            out.push(WebBackend::Searxng { base_url: u.trim_end_matches('/').to_string() });
        }
        if self.config.duckduckgo {
            out.push(WebBackend::DuckDuckGoLite);
        }
        if let Some(h) = &self.hosted {
            out.push(WebBackend::Hosted(h.clone()));
        }
        out
    }

    /// The first backend `reachable` accepts.
    pub fn select_backend(&self, reachable: &dyn Fn(&WebBackend) -> bool) -> Option<WebBackend> {
        self.backends().into_iter().find(|b| reachable(b))
    }

    fn rest(&self, b: &WebBackend, d: Duration) {
        let mut r = self.resting.lock().unwrap_or_else(|p| p.into_inner());
        r.retain(|(k, _)| *k != b.key());
        r.push((b.key(), Instant::now() + d));
    }

    /// Rests `b` for [`CHALLENGED_FOR`], in this process and in the cache.
    fn rest_challenged(&self, b: &WebBackend, ctx: &ChannelCtx<'_>) {
        self.rest(b, CHALLENGED_FOR);
        if let Some(c) = ctx.cache {
            let _ = c.put(REST_BUCKET, &CacheKey::of(&[b.key()]), b"challenged");
        }
    }

    fn is_resting(&self, b: &WebBackend, ctx: &ChannelCtx<'_>) -> bool {
        let here = {
            let r = self.resting.lock().unwrap_or_else(|p| p.into_inner());
            r.iter().any(|(k, until)| *k == b.key() && *until > Instant::now())
        };
        here || ctx.cache.is_some_and(|c| c.get(REST_BUCKET, &CacheKey::of(&[b.key()]), CHALLENGED_FOR).is_some())
    }

    fn searxng(&self, base: &str, q: &ChannelQuery, ctx: &ChannelCtx<'_>) -> Result<Vec<SourceRef>, Miss> {
        let mut url = format!("{base}/search?q={}&format=json&pageno=1&safesearch=0", enc(q.text.trim()));
        if q.since_year.is_some_and(|y| y as i32 >= current_year() - 1) {
            url.push_str("&time_range=year");
        }
        let r = get_raw(ctx, ChannelId::Web, HttpRequest::get(url).accept("application/json")).map_err(Miss::from)?;
        match r.status {
            200..=299 => {}
            403 | 404 | 415 => return Err(Miss::Down(format!(
                "SearXNG at {base} answered HTTP {} to format=json; enable `search.formats: [json]` in settings.yml",
                r.status
            ))),
            429 => return Err(Miss::Down(format!("SearXNG at {base} is rate-limiting (HTTP 429)"))),
            s => return Err(Miss::Down(format!("SearXNG at {base} answered HTTP {s}"))),
        }
        let v = parse_json(ChannelId::Web, &r.bytes)
            .map_err(|_| Miss::Down(format!("something at {base} answered, but not SearXNG JSON")))?;
        let Some(results) = v.get("results").and_then(Value::as_array) else {
            return Err(Miss::Down(format!("something at {base} answered, but not SearXNG JSON")));
        };
        Ok(results.iter().filter_map(searxng_result).take(limit(q)).collect())
    }

    fn duckduckgo(&self, q: &ChannelQuery, ctx: &ChannelCtx<'_>) -> Result<Vec<SourceRef>, Miss> {
        let mut url = format!("{DUCKDUCKGO_LITE}?q={}", enc(q.text.trim()));
        if q.since_year.is_some_and(|y| y as i32 >= current_year() - 1) {
            url.push_str("&df=y");
        }
        self.gate.admit(&url, ctx.http, ctx.cancel).map_err(|r| match r {
            Refusal::Cancelled => Miss::Cancelled,
            other => Miss::Down(format!("DuckDuckGo: {other}")),
        })?;
        let r = get_raw(ctx, ChannelId::Web, HttpRequest::get(url).accept("text/html")).map_err(Miss::from)?;
        let html = String::from_utf8_lossy(&r.bytes);
        if r.status == 202 || is_challenge(&html) {
            return Err(Miss::Challenged("DuckDuckGo served a challenge page instead of results".into()));
        }
        if !(200..300).contains(&r.status) {
            return Err(Miss::Down(format!("DuckDuckGo answered HTTP {}", r.status)));
        }
        Ok(parse_ddg_lite(&html).into_iter().take(limit(q)).collect())
    }
}

/// Why one backend gave no answer.
enum Miss {
    Down(String),
    Challenged(String),
    Cancelled,
}

impl From<ChannelError> for Miss {
    fn from(e: ChannelError) -> Self {
        match e {
            ChannelError::Cancelled => Miss::Cancelled,
            other => Miss::Down(other.to_string()),
        }
    }
}

fn current_year() -> i32 {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    super::iso_date(secs)[..4].parse().unwrap_or(1970)
}

/// One SearXNG JSON result.
pub fn searxng_result(r: &Value) -> Option<SourceRef> {
    let url = s(r, "url")?;
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return None;
    }
    let engines: Vec<String> = r
        .get("engines")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|e| e.as_str().map(String::from))
        .collect();
    Some(
        SourceRef {
            date: s(r, "publishedDate").filter(|d| d != "None"),
            snippet: clip(&strip_tags(&s(r, "content").unwrap_or_default()), SNIPPET_CHARS),
            ..SourceRef::new(ChannelId::Web, &url, &s(r, "title").unwrap_or_else(|| url.clone()))
        }
        .with_meta("backend", "searxng")
        .with_meta("engines", engines.join(" ")),
    )
}

fn is_challenge(html: &str) -> bool {
    let l = html.to_ascii_lowercase();
    !l.contains("result-link")
        && ["anomaly", "challenge-form", "captcha", "bots use duckduckgo"].iter().any(|m| l.contains(m))
}

/// DuckDuckGo wraps result links as `//duckduckgo.com/l/?uddg=<encoded>`; this unwraps them.
fn ddg_target(href: &str) -> Option<String> {
    let href = decode_entities(href);
    let raw = match href.split_once("uddg=") {
        Some((_, rest)) => percent_decode(rest.split('&').next().unwrap_or("")),
        None if href.starts_with("//") => format!("https:{href}"),
        None => href,
    };
    (raw.starts_with("https://") || raw.starts_with("http://")).then_some(raw)
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match (b[i], b.get(i + 1).copied().and_then(hex), b.get(i + 2).copied().and_then(hex)) {
            (b'%', Some(h), Some(l)) => {
                out.push(h * 16 + l);
                i += 3;
            }
            (b'+', _, _) => {
                out.push(b' ');
                i += 1;
            }
            (c, _, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Results of the lite page: each `a.result-link`, with the `td.result-snippet` that follows.
/// Sponsored rows (ad redirects) are skipped.
pub fn parse_ddg_lite(html: &str) -> Vec<SourceRef> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(i) = html[from..].find("result-link") {
        let at = from + i;
        let Some(tag_start) = html[..at].rfind("<a") else { break };
        let Some(tag_end) = html[at..].find('>').map(|e| at + e) else { break };
        let Some(close) = html[tag_end..].find("</a>").map(|e| tag_end + e) else { break };
        let tag = &html[tag_start..tag_end];
        let title = strip_tags(&html[tag_end + 1..close]);
        from = close + 4;
        let Some(href) = super::feeds::attr_value(tag, "href") else { continue };
        if href.contains("y.js?") || href.contains("ad_domain") || href.contains("ad_provider") {
            continue;
        }
        let Some(url) = ddg_target(&href) else { continue };
        let next = html[from..].find("result-link").map_or(html.len(), |n| from + n);
        let snippet = html[from..next]
            .find("result-snippet")
            .and_then(|k| {
                let s0 = from + k;
                let open = html[s0..].find('>')? + s0 + 1;
                let end = html[open..].find("</td>")? + open;
                Some(strip_tags(&html[open..end]))
            })
            .unwrap_or_default();
        out.push(
            SourceRef { snippet: clip(&snippet, SNIPPET_CHARS), ..SourceRef::new(ChannelId::Web, &url, &title) }
                .with_meta("backend", "duckduckgo"),
        );
    }
    out
}

impl Channel for WebChannel {
    fn spec(&self) -> &ChannelSpec {
        &self.spec
    }

    fn search(&self, query: &ChannelQuery, ctx: &ChannelCtx<'_>) -> Result<Vec<SourceRef>, ChannelError> {
        let mut misses = Vec::new();
        for b in self.backends() {
            if ctx.cancel.is_cancelled() {
                return Err(ChannelError::Cancelled);
            }
            if self.is_resting(&b, ctx) {
                misses.push(format!("{b:?}: resting after an earlier failure"));
                continue;
            }
            let got = match &b {
                WebBackend::Searxng { base_url } => self.searxng(base_url, query, ctx),
                WebBackend::DuckDuckGoLite => self.duckduckgo(query, ctx),
                WebBackend::Hosted(h) => h
                    .search(&query.text, limit(query), ctx.cancel)
                    .map(|v| v.into_iter().map(|r| r.with_meta("backend", h.name().to_string())).collect())
                    .map_err(Miss::Down),
            };
            match got {
                Ok(v) => return Ok(v),
                Err(Miss::Cancelled) => return Err(ChannelError::Cancelled),
                Err(Miss::Challenged(d)) => {
                    self.rest_challenged(&b, ctx);
                    misses.push(format!("{d}; not retried for {} h", CHALLENGED_FOR.as_secs() / 3600));
                }
                Err(Miss::Down(d)) => {
                    self.rest(&b, DOWN_FOR);
                    misses.push(d);
                }
            }
        }
        if misses.is_empty() {
            misses.push("no backend configured".into());
        }
        Err(ChannelError::NoBackend { channel: ChannelId::Web, detail: misses.join("; ") })
    }

    /// SearXNG is asked for `/healthz`; DuckDuckGo and a hosted search cannot be probed without
    /// spending a search, so they count as up unless resting after a failure.
    fn ready(&self, ctx: &ChannelCtx<'_>) -> Result<(), String> {
        let backends = self.backends();
        if backends.is_empty() {
            return Err("no web search backend is configured (SearXNG, DuckDuckGo and hosted search are all off)".into());
        }
        let mut why = Vec::new();
        for b in backends {
            if self.is_resting(&b, ctx) {
                why.push(format!("{b:?} is resting after an earlier failure"));
                continue;
            }
            match &b {
                WebBackend::Searxng { base_url } => {
                    match get_raw(ctx, ChannelId::Web, HttpRequest::get(format!("{base_url}/healthz"))) {
                        Ok(r) if r.status < 500 => return Ok(()),
                        Ok(r) => why.push(format!("SearXNG at {base_url} answered HTTP {}", r.status)),
                        Err(e) => {
                            self.rest(&b, DOWN_FOR);
                            why.push(format!("SearXNG at {base_url} is not reachable ({e})"));
                        }
                    }
                }
                WebBackend::DuckDuckGoLite | WebBackend::Hosted(_) => return Ok(()),
            }
        }
        Err(format!("no web search backend answers: {}", why.join("; ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake;
    impl HostedSearch for Fake {
        fn name(&self) -> &str {
            "fake"
        }
        fn search(&self, q: &str, _: usize, _: &CancelToken) -> Result<Vec<SourceRef>, String> {
            Ok(vec![SourceRef::new(ChannelId::Web, "https://example.com", q)])
        }
    }

    #[test]
    fn web_backends_fall_back_in_order() {
        let w = WebChannel::new(WebConfig::default(), Some(Arc::new(Fake)));
        let names: Vec<String> = w.backends().iter().map(|b| format!("{b:?}")).collect();
        assert_eq!(names, ["Searxng(http://127.0.0.1:8080)", "DuckDuckGoLite", "Hosted(fake)"]);
        let picked = w.select_backend(&|b| !matches!(b, WebBackend::Searxng { .. }));
        assert!(matches!(picked, Some(WebBackend::DuckDuckGoLite)));
        assert!(w.select_backend(&|_| false).is_none());
    }

    struct Refused;
    impl crate::channel::HttpGet for Refused {
        fn get(&self, r: &HttpRequest, _: &CancelToken) -> Result<crate::channel::HttpResponse, String> {
            Err(format!("connection refused: {}", r.url))
        }
    }

    /// Found live: SearXNG down and the other backends off, and every web query of a run failed.
    #[test]
    fn readiness_names_a_web_channel_that_cannot_answer() {
        let cancel = CancelToken::new();
        let ctx = ChannelCtx { http: &Refused, cache: None, cancel: &cancel };
        let off = WebChannel::new(WebConfig { searxng_url: None, duckduckgo: false }, None);
        assert!(off.ready(&ctx).unwrap_err().contains("all off"));
        let down = WebChannel::new(WebConfig { searxng_url: Some(DEFAULT_SEARXNG.into()), duckduckgo: false }, None);
        let why = down.ready(&ctx).unwrap_err();
        assert!(why.contains("SearXNG at http://127.0.0.1:8080 is not reachable"), "{why}");
        let fallback = WebChannel::new(WebConfig::default(), None);
        assert_eq!(fallback.ready(&ctx), Ok(()), "DuckDuckGo is taken as up until it fails");
        assert!(WebChannel::new(WebConfig::default(), Some(Arc::new(Fake))).ready(&ctx).is_ok());
    }

    #[test]
    fn duckduckgo_links_unwrap_and_ads_are_skipped() {
        assert_eq!(
            ddg_target("//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fa%3Fb%3D1&amp;rut=x").as_deref(),
            Some("https://example.com/a?b=1")
        );
        assert_eq!(ddg_target("javascript:alert(1)"), None);
        let html = r#"<table><tr><td><a rel="nofollow" href="https://duckduckgo.com/y.js?ad_domain=x" class='result-link'>Ad</a></td></tr>
            <tr><td><a rel="nofollow" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fdoc.rust-lang.org%2Fcargo%2F" class='result-link'>The <b>Cargo</b> Book</a></td></tr>
            <tr><td class='result-snippet'>Cargo is the <b>Rust</b> package manager.</td></tr></table>"#;
        let r = parse_ddg_lite(html);
        assert_eq!(r.len(), 1);
        assert_eq!((r[0].url.as_str(), r[0].title.as_str()), ("https://doc.rust-lang.org/cargo/", "The Cargo Book"));
        assert_eq!(r[0].snippet, "Cargo is the Rust package manager.");
        assert!(is_challenge("<form id=\"challenge-form\">"));
    }
}
