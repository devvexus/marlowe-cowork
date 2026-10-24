//! General web search over the first backend that answers: a SearXNG instance (loopback by
//! default, ADR-035), the free search APIs that serve agents without a key (Parallel's and Exa's
//! public MCP endpoints, taken in turn so neither free tier carries every query, then
//! Marginalia's public API), DuckDuckGo's HTML lite page, or a hosted search the daemon supplies.
//! No key and no paid API: a free tier that says "too many" rests and the next one answers.
//!
//! "Found nothing" and "could not look" stay distinct: an empty result list is an answer; a
//! backend that is down, refuses JSON, or challenges us is skipped and named in the error when
//! none answers. DuckDuckGo is read under its robots.txt; a challenge page (HTTP 202 or a
//! captcha form) is taken as "not now": the backend rests and nothing tries to get around it.

use std::sync::atomic::{AtomicUsize, Ordering};
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
/// Parallel's free search for agents (MCP over HTTP, no account or key).
pub const PARALLEL_MCP: &str = "https://search.parallel.ai/mcp";
/// Exa's hosted MCP endpoint; its search tool answers without a key on a free tier.
pub const EXA_MCP: &str = "https://mcp.exa.ai/mcp";
/// Marginalia's public search API (the shared `public` key is part of the URL, not a credential).
pub const MARGINALIA_API: &str = "https://api.marginalia.nu/public/search/";
/// How long a free tier that refused ("too many requests") rests when it named no time.
const FREE_TIER_REST: Duration = Duration::from_secs(15 * 60);

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
    /// Parallel's free agent search.
    Parallel,
    /// Exa's free hosted search.
    Exa,
    /// Marginalia's public API.
    Marginalia,
    DuckDuckGoLite,
    Hosted(Arc<dyn HostedSearch>),
}

impl std::fmt::Debug for WebBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WebBackend::Searxng { base_url } => write!(f, "Searxng({base_url})"),
            WebBackend::Parallel => f.write_str("Parallel"),
            WebBackend::Exa => f.write_str("Exa"),
            WebBackend::Marginalia => f.write_str("Marginalia"),
            WebBackend::DuckDuckGoLite => f.write_str("DuckDuckGoLite"),
            WebBackend::Hosted(h) => write!(f, "Hosted({})", h.name()),
        }
    }
}

impl WebBackend {
    fn key(&self) -> String {
        match self {
            WebBackend::Searxng { base_url } => format!("searxng:{base_url}"),
            WebBackend::Parallel => "parallel".into(),
            WebBackend::Exa => "exa".into(),
            WebBackend::Marginalia => "marginalia".into(),
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
    /// The free search APIs (Parallel, Exa, Marginalia) when SearXNG is unreachable.
    pub free_search: bool,
    /// Fall back to DuckDuckGo's HTML lite page after them.
    pub duckduckgo: bool,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self { searxng_url: Some(DEFAULT_SEARXNG.to_string()), free_search: true, duckduckgo: true }
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
    /// Which free tier goes first on the next search (they take turns).
    turn: AtomicUsize,
}

impl WebChannel {
    pub fn new(config: WebConfig, hosted: Option<Arc<dyn HostedSearch>>) -> Self {
        let mut spec = spec_of(ChannelId::Web);
        if let Some(u) = &config.searxng_url {
            spec.endpoint = u.trim_end_matches('/').to_string();
        }
        Self { spec, config, hosted, gate: HostGate::new(), resting: Mutex::new(Vec::new()), turn: AtomicUsize::new(0) }
    }

    pub fn config(&self) -> &WebConfig {
        &self.config
    }

    /// Backends in fallback order: SearXNG, the two free agent searches (in the order of this
    /// turn), Marginalia, DuckDuckGo, a hosted search.
    pub fn backends(&self) -> Vec<WebBackend> {
        self.backends_from(0)
    }

    fn backends_from(&self, turn: usize) -> Vec<WebBackend> {
        let mut out = Vec::new();
        if let Some(u) = &self.config.searxng_url {
            out.push(WebBackend::Searxng { base_url: u.trim_end_matches('/').to_string() });
        }
        if self.config.free_search {
            let pair = [WebBackend::Parallel, WebBackend::Exa];
            out.extend((0..pair.len()).map(|i| pair[(turn + i) % pair.len()].clone()));
            out.push(WebBackend::Marginalia);
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

    /// One JSON-RPC `tools/call` to a free MCP search endpoint, stateless (both answer without a
    /// session). A refusal for load rests the backend for its `Retry-After` or [`FREE_TIER_REST`].
    fn mcp_call(&self, url: &str, name: &str, tool: &str, args: Value, ctx: &ChannelCtx<'_>) -> Result<String, Miss> {
        let body = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": tool, "arguments": args}});
        let req = HttpRequest::get(url).accept("application/json, text/event-stream");
        let r = ctx.http.post_json(&req, body.to_string().as_bytes(), ctx.cancel).map_err(|e| {
            if ctx.cancel.is_cancelled() {
                Miss::Cancelled
            } else {
                Miss::Down(format!("{name}: {e}"))
            }
        })?;
        if r.status == 429 {
            return Err(Miss::Busy(format!("{name}'s free tier is busy (HTTP 429)"), r.retry_after.unwrap_or(FREE_TIER_REST)));
        }
        if !(200..300).contains(&r.status) {
            return Err(Miss::Down(format!("{name} answered HTTP {}", r.status)));
        }
        mcp_text(&String::from_utf8_lossy(&r.bytes)).map_err(|e| {
            let busy = ["rate", "limit", "quota", "too many"].iter().any(|w| e.to_ascii_lowercase().contains(w));
            if busy {
                Miss::Busy(format!("{name}: {e}"), FREE_TIER_REST)
            } else {
                Miss::Down(format!("{name}: {e}"))
            }
        })
    }

    fn parallel(&self, q: &ChannelQuery, ctx: &ChannelCtx<'_>) -> Result<Vec<SourceRef>, Miss> {
        let text = q.text.trim();
        let args = serde_json::json!({"objective": text, "search_queries": [text]});
        let body = self.mcp_call(PARALLEL_MCP, "Parallel", "web_search", args, ctx)?;
        parse_parallel(&body).map(|v| v.into_iter().take(limit(q)).collect()).map_err(|e| Miss::Down(format!("Parallel: {e}")))
    }

    fn exa(&self, q: &ChannelQuery, ctx: &ChannelCtx<'_>) -> Result<Vec<SourceRef>, Miss> {
        let args = serde_json::json!({"query": q.text.trim(), "numResults": limit(q)});
        let body = self.mcp_call(EXA_MCP, "Exa", "web_search_exa", args, ctx)?;
        Ok(parse_exa(&body).into_iter().take(limit(q)).collect())
    }

    fn marginalia(&self, q: &ChannelQuery, ctx: &ChannelCtx<'_>) -> Result<Vec<SourceRef>, Miss> {
        let url = format!("{MARGINALIA_API}{}?count={}", enc(q.text.trim()), limit(q));
        let r = get_raw(ctx, ChannelId::Web, HttpRequest::get(url).accept("application/json")).map_err(Miss::from)?;
        if r.status == 429 || r.status == 503 {
            return Err(Miss::Busy(format!("Marginalia is busy (HTTP {})", r.status), r.retry_after.unwrap_or(FREE_TIER_REST)));
        }
        if !(200..300).contains(&r.status) {
            return Err(Miss::Down(format!("Marginalia answered HTTP {}", r.status)));
        }
        let v = parse_json(ChannelId::Web, &r.bytes).map_err(|_| Miss::Down("Marginalia answered, but not JSON".into()))?;
        Ok(parse_marginalia(&v).into_iter().take(limit(q)).collect())
    }
}

/// Why one backend gave no answer.
enum Miss {
    Down(String),
    Challenged(String),
    /// A free tier refused for load: rest it this long.
    Busy(String, Duration),
    Cancelled,
}

/// The tool result text of a JSON-RPC answer, whether it came as JSON or as one server-sent
/// event (`data: {...}`). A JSON-RPC error or an `isError` result is an `Err` with its message.
pub fn mcp_text(body: &str) -> Result<String, String> {
    let json = body.lines().find_map(|l| l.strip_prefix("data:")).map(str::trim).unwrap_or_else(|| body.trim());
    let v: Value = serde_json::from_str(json).map_err(|_| "the answer is not JSON-RPC".to_string())?;
    if let Some(e) = v.get("error") {
        return Err(e.get("message").and_then(Value::as_str).unwrap_or("an error").to_string());
    }
    let result = v.get("result").ok_or("the answer has no result")?;
    let text: String = result
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|c| c.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        return Err(clip(&text, 300));
    }
    Ok(text)
}

/// Parallel's `web_search` text: a JSON object with `results` of `{url, title, publish_date,
/// excerpts}`.
pub fn parse_parallel(text: &str) -> Result<Vec<SourceRef>, String> {
    let v: Value = serde_json::from_str(text.trim()).map_err(|_| "the search result is not JSON".to_string())?;
    let results = v.get("results").and_then(Value::as_array).ok_or("the search result has no results")?;
    Ok(results
        .iter()
        .filter_map(|r| {
            let url = s(r, "url").filter(|u| u.starts_with("https://") || u.starts_with("http://"))?;
            let excerpts: Vec<&str> = r.get("excerpts").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
            Some(
                SourceRef {
                    date: s(r, "publish_date").filter(|d| !d.is_empty()),
                    snippet: clip(&excerpts.join(" … "), SNIPPET_CHARS),
                    ..SourceRef::new(ChannelId::Web, &url, &s(r, "title").unwrap_or_else(|| url.clone()))
                }
                .with_meta("backend", "parallel"),
            )
        })
        .collect())
}

/// Exa's `web_search_exa` text: results separated by a `---` line, each with `Title:`, `URL:`,
/// `Published:` and `Highlights:` lines.
pub fn parse_exa(text: &str) -> Vec<SourceRef> {
    text.split("\n---\n")
        .filter_map(|block| {
            let field = |name: &str| block.lines().find_map(|l| l.strip_prefix(name)).map(str::trim).filter(|v| !v.is_empty() && *v != "N/A");
            let url = field("URL:").filter(|u| u.starts_with("https://") || u.starts_with("http://"))?.to_string();
            let title = field("Title:").unwrap_or(&url).to_string();
            let highlights = block.split_once("Highlights:").map(|(_, h)| h).unwrap_or("");
            let snippet = highlights.lines().map(str::trim).filter(|l| !l.is_empty() && *l != "...").collect::<Vec<_>>().join(" ");
            Some(
                SourceRef { date: field("Published:").map(str::to_string), snippet: clip(&snippet, SNIPPET_CHARS), ..SourceRef::new(ChannelId::Web, &url, &title) }
                    .with_meta("backend", "exa"),
            )
        })
        .collect()
}

/// Marginalia's `results` of `{url, title, description}`.
pub fn parse_marginalia(v: &Value) -> Vec<SourceRef> {
    v.get("results")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|r| {
            let url = s(r, "url").filter(|u| u.starts_with("https://") || u.starts_with("http://"))?;
            Some(
                SourceRef {
                    snippet: clip(&s(r, "description").unwrap_or_default(), SNIPPET_CHARS),
                    ..SourceRef::new(ChannelId::Web, &url, &s(r, "title").unwrap_or_else(|| url.clone()))
                }
                .with_meta("backend", "marginalia"),
            )
        })
        .collect()
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
        let turn = self.turn.fetch_add(1, Ordering::Relaxed);
        for b in self.backends_from(turn) {
            if ctx.cancel.is_cancelled() {
                return Err(ChannelError::Cancelled);
            }
            if self.is_resting(&b, ctx) {
                misses.push(format!("{b:?}: resting after an earlier failure"));
                continue;
            }
            let got = match &b {
                WebBackend::Searxng { base_url } => self.searxng(base_url, query, ctx),
                WebBackend::Parallel => self.parallel(query, ctx),
                WebBackend::Exa => self.exa(query, ctx),
                WebBackend::Marginalia => self.marginalia(query, ctx),
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
                Err(Miss::Busy(d, rest_for)) => {
                    self.rest(&b, rest_for);
                    misses.push(format!("{d}; resting {} min", rest_for.as_secs().div_ceil(60)));
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

    /// SearXNG is asked for `/healthz`; the free APIs, DuckDuckGo and a hosted search cannot be
    /// probed without spending a search, so they count as up unless resting after a failure.
    fn ready(&self, ctx: &ChannelCtx<'_>) -> Result<(), String> {
        let backends = self.backends();
        if backends.is_empty() {
            return Err("no web search backend is configured (SearXNG, the free search APIs, DuckDuckGo and hosted search are all off)".into());
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
                WebBackend::Parallel | WebBackend::Exa | WebBackend::Marginalia | WebBackend::DuckDuckGoLite | WebBackend::Hosted(_) => {
                    return Ok(())
                }
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
        assert_eq!(names, ["Searxng(http://127.0.0.1:8080)", "Parallel", "Exa", "Marginalia", "DuckDuckGoLite", "Hosted(fake)"]);
        let turned: Vec<String> = w.backends_from(1).iter().map(|b| format!("{b:?}")).collect();
        assert_eq!(turned[1..3], ["Exa".to_string(), "Parallel".to_string()], "the free tiers take turns going first");
        let picked = w.select_backend(&|b| matches!(b, WebBackend::DuckDuckGoLite | WebBackend::Hosted(_)));
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
        let off = WebChannel::new(WebConfig { searxng_url: None, free_search: false, duckduckgo: false }, None);
        assert!(off.ready(&ctx).unwrap_err().contains("all off"));
        let down = WebChannel::new(WebConfig { searxng_url: Some(DEFAULT_SEARXNG.into()), free_search: false, duckduckgo: false }, None);
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
        assert_eq!(ddg_target(&["java", "script:alert(1)"].concat()), None);
        let html = r#"<table><tr><td><a rel="nofollow" href="https://duckduckgo.com/y.js?ad_domain=x" class='result-link'>Ad</a></td></tr>
            <tr><td><a rel="nofollow" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fdoc.rust-lang.org%2Fcargo%2F" class='result-link'>The <b>Cargo</b> Book</a></td></tr>
            <tr><td class='result-snippet'>Cargo is the <b>Rust</b> package manager.</td></tr></table>"#;
        let r = parse_ddg_lite(html);
        assert_eq!(r.len(), 1);
        assert_eq!((r[0].url.as_str(), r[0].title.as_str()), ("https://doc.rust-lang.org/cargo/", "The Cargo Book"));
        assert_eq!(r[0].snippet, "Cargo is the Rust package manager.");
        assert!(is_challenge("<form id=\"challenge-form\">"));
    }

    /// Parallel refuses for load: Exa answers this search, Parallel rests, and the next search
    /// (Exa's turn first anyway) never asks Parallel while it rests.
    #[test]
    fn a_busy_free_tier_rests_and_the_next_one_answers() {
        struct Tiers(Mutex<Vec<String>>);
        impl crate::channel::HttpGet for Tiers {
            fn get(&self, r: &HttpRequest, _: &CancelToken) -> Result<crate::channel::HttpResponse, String> {
                Err(format!("connection refused: {}", r.url))
            }
            fn post_json(&self, r: &HttpRequest, _: &[u8], _: &CancelToken) -> Result<crate::channel::HttpResponse, String> {
                self.0.lock().unwrap().push(r.url.clone());
                let (status, body) = if r.url == PARALLEL_MCP {
                    (429, String::new())
                } else {
                    let text = "Title: A
URL: https://a.example/x
Highlights:
found";
                    (200, serde_json::json!({"result": {"content": [{"type": "text", "text": text}]}}).to_string())
                };
                Ok(crate::channel::HttpResponse { status, content_type: None, bytes: body.into_bytes(), final_url: r.url.clone(), retry_after: None })
            }
        }
        let http = Tiers(Mutex::new(Vec::new()));
        let cancel = CancelToken::new();
        let ctx = ChannelCtx { http: &http, cache: None, cancel: &cancel };
        let w = WebChannel::new(WebConfig { searxng_url: None, free_search: true, duckduckgo: false }, None);
        let q = ChannelQuery::new("x", 3);
        let r = w.search(&q, &ctx).unwrap();
        assert_eq!((r[0].url.as_str(), r[0].meta.get("backend").map(String::as_str)), ("https://a.example/x", Some("exa")));
        assert_eq!(*http.0.lock().unwrap(), [PARALLEL_MCP.to_string(), EXA_MCP.to_string()]);
        w.search(&q, &ctx).unwrap();
        w.search(&q, &ctx).unwrap();
        assert!(!http.0.lock().unwrap()[2..].contains(&PARALLEL_MCP.to_string()), "a resting tier is not asked");
    }

    /// Recorded 2026-10-23 from the two free agent searches and Marginalia (trimmed).
    #[test]
    fn free_agent_search_answers_parse() {
        let inner = serde_json::json!({"search_id": "search_1", "results": [
            {"url": "https://api-docs.deepseek.com/updates", "title": "Change Log | DeepSeek API Docs", "publish_date": "2026-10-17",
             "excerpts": ["## Date: 2026-09-10", "DeepSeek-V4.1-Flash Release"]},
            {"url": "javascript:x", "title": "bad", "excerpts": []}]});
        let parallel_body = serde_json::json!({"jsonrpc": "2.0", "id": 3, "result": {"content": [{"type": "text", "text": inner.to_string()}]}}).to_string();
        let r = parse_parallel(&mcp_text(&parallel_body).unwrap()).unwrap();
        assert_eq!(r.len(), 1, "a result that is no web address is dropped");
        assert_eq!((r[0].url.as_str(), r[0].date.as_deref()), ("https://api-docs.deepseek.com/updates", Some("2026-10-17")));
        assert_eq!(r[0].snippet, "## Date: 2026-09-10 … DeepSeek-V4.1-Flash Release");
        assert_eq!(r[0].meta.get("backend").map(String::as_str), Some("parallel"));

        let exa_text = "Title: Kimi K3 Tech Blog\nURL: https://www.kimi.ai/blog/kimi-k3\nPublished: N/A\nAuthor: N/A\nHighlights:\nToday, we are introducing Kimi K3.\n\n...\n\n---\n\nTitle: Qwen 4: what Alibaba has said\nURL: https://www.orcarouter.ai/blog/qwen-4\nPublished: 2026-10-13T10:27:20.000Z\nHighlights:\nQwen 4 is not available.";
        let event = serde_json::json!({"result": {"content": [{"type": "text", "text": exa_text}]}});
        let exa_body = format!("event: message\ndata: {event}\n");
        let r = parse_exa(&mcp_text(&exa_body).unwrap());
        assert_eq!(r.len(), 2);
        assert_eq!((r[0].title.as_str(), r[0].date.as_deref(), r[0].snippet.as_str()), ("Kimi K3 Tech Blog", None, "Today, we are introducing Kimi K3."));
        assert_eq!(r[1].date.as_deref(), Some("2026-10-13T10:27:20.000Z"));

        let m = parse_marginalia(&serde_json::json!({"results": [{"url": "https://www.unite.ai/qwen4", "title": "Qwen3.8 previews Qwen4", "description": "Availability"}]}));
        assert_eq!((m[0].url.as_str(), m[0].snippet.as_str()), ("https://www.unite.ai/qwen4", "Availability"));

        assert_eq!(mcp_text(r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"Rate limit exceeded"}}"#), Err("Rate limit exceeded".into()));
        assert!(mcp_text(r#"{"result":{"isError":true,"content":[{"type":"text","text":"quota used"}]}}"#).is_err());
    }
}
