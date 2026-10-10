//! The built-in keyless channels: their declared specs and their searches. General web search
//! picks a backend: a configured SearXNG (local by default), else DuckDuckGo's HTML lite page,
//! else a hosted search the daemon supplies.
//!
//! Channel APIs are called under each API's published usage policy: the declared rate limit, a
//! User-Agent that names the client and says it is automated, and no credential. robots.txt
//! governs page fetches (the corpus fetcher), RSS feeds and the DuckDuckGo lite page.

use std::time::Duration;

use marlowe_proto::ChannelId;
use serde_json::Value;

use crate::channel::{Channel, ChannelCtx, ChannelError, ChannelQuery, ChannelSpec, HttpRequest, HttpResponse};
use crate::channel::{RateLimit, ResultShape};
use crate::source::{Namespace, SourceRef};

pub mod arxiv;
pub mod community;
pub mod feeds;
pub mod pubmed;
pub mod scholarly;
pub mod web;
pub mod wiki;

pub use arxiv::ArxivChannel;
pub use community::{GithubChannel, HackerNewsChannel, StackExchangeChannel};
pub use feeds::{RssChannel, DEFAULT_FEEDS};
pub use pubmed::PubmedChannel;
pub use scholarly::{CrossrefChannel, OpenAlexChannel, SemanticScholarChannel};
pub use web::{HostedSearch, WebBackend, WebChannel, WebConfig, DEFAULT_SEARXNG, DUCKDUCKGO_LITE};
pub use wiki::{WikidataChannel, WikipediaChannel};

const SEC: Duration = Duration::from_secs(1);
const MIN: Duration = Duration::from_secs(60);
const HOUR: Duration = Duration::from_secs(3600);
const DAY: Duration = Duration::from_secs(24 * 3600);

/// Characters of abstract/description kept per result.
pub const SNIPPET_CHARS: usize = 1200;
/// Results asked of any channel in one search.
pub const MAX_LIMIT: usize = 50;

/// The declared spec of a built-in network channel; `None` for `Memory`. Limits are the
/// published keyless limits, or conservative ones where none is published.
pub fn builtin_spec(id: ChannelId) -> Option<ChannelSpec> {
    use ChannelId as C;
    use Namespace as N;
    let (endpoint, limits, shape, authority, curated, indexes, ttl): (
        &str,
        Vec<RateLimit>,
        _,
        Vec<N>,
        bool,
        Vec<C>,
        Duration,
    ) = match id {
        // arXiv asks for one request per 3 s.
        C::Arxiv => (
            "https://export.arxiv.org/api/query",
            vec![RateLimit::every(3 * SEC)],
            ResultShape::Atom,
            vec![N::Arxiv],
            true,
            vec![],
            DAY,
        ),
        C::OpenAlex => (
            "https://api.openalex.org",
            vec![RateLimit::new(5, SEC), RateLimit::new(100_000, DAY)],
            ResultShape::Json,
            vec![N::OpenAlex],
            true,
            vec![C::Crossref, C::Arxiv, C::Pubmed],
            DAY,
        ),
        // Crossref public pool: a few requests per second, one at a time.
        C::Crossref => (
            "https://api.crossref.org",
            vec![RateLimit::new(4, SEC)],
            ResultShape::Json,
            vec![N::Doi],
            true,
            vec![],
            DAY,
        ),
        // Keyless S2 shares one pool across all anonymous users; one per second is courteous.
        C::SemanticScholar => (
            "https://api.semanticscholar.org/graph/v1",
            vec![RateLimit::every(Duration::from_millis(1100))],
            ResultShape::Json,
            vec![N::S2],
            true,
            vec![C::Arxiv, C::Crossref, C::Pubmed],
            DAY,
        ),
        C::Wikipedia => (
            "https://en.wikipedia.org/w/api.php",
            vec![RateLimit::new(2, SEC)],
            ResultShape::Json,
            vec![],
            false,
            vec![],
            DAY,
        ),
        C::Wikidata => (
            "https://www.wikidata.org/w/api.php",
            vec![RateLimit::new(2, SEC)],
            ResultShape::Json,
            vec![N::Qid],
            true,
            vec![],
            DAY,
        ),
        // Unauthenticated: 60 core requests an hour, search 10 a minute.
        C::Github => (
            "https://api.github.com",
            vec![RateLimit::new(10, MIN), RateLimit::new(60, HOUR)],
            ResultShape::Json,
            vec![N::Repo],
            true,
            vec![],
            6 * HOUR,
        ),
        C::HackerNews => (
            "https://hn.algolia.com/api/v1",
            vec![RateLimit::new(2, SEC), RateLimit::new(10_000, HOUR)],
            ResultShape::Json,
            vec![],
            false,
            vec![],
            HOUR,
        ),
        // Keyless: 300 requests a day per IP; honour `backoff` in responses.
        C::StackExchange => (
            "https://api.stackexchange.com/2.3",
            vec![RateLimit::every(SEC), RateLimit::new(300, DAY)],
            ResultShape::Json,
            vec![],
            false,
            vec![],
            DAY,
        ),
        // E-utilities without a key: 3 requests per second.
        C::Pubmed => (
            "https://eutils.ncbi.nlm.nih.gov/entrez/eutils",
            vec![RateLimit::new(3, SEC)],
            ResultShape::Json,
            vec![N::Pmid, N::Pmcid],
            true,
            vec![],
            DAY,
        ),
        // Feeds live on many hosts; the channel's host gate adds robots.txt and per-host spacing.
        C::Rss => ("https://", vec![RateLimit::new(4, SEC)], ResultShape::Feed, vec![], false, vec![], HOUR),
        C::Web => (
            DEFAULT_SEARXNG,
            vec![RateLimit::every(SEC)],
            ResultShape::Json,
            vec![],
            false,
            ChannelId::ALL.iter().copied().filter(|c| *c != C::Web && *c != C::Memory).collect(),
            HOUR,
        ),
        C::Memory => return None,
    };
    Some(ChannelSpec {
        id,
        endpoint: endpoint.to_string(),
        rate_limits: limits,
        shape,
        identity_authority: authority,
        curated_ids: curated,
        indexes,
        cache_ttl: ttl,
    })
}

/// Settings for the keyless channels that have any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeylessConfig {
    pub web: WebConfig,
    /// RSS/Atom feeds searched when a query is not itself a feed URL.
    pub feeds: Vec<String>,
    /// Wikipedia language edition (`en`, `de`, ...).
    pub wikipedia_lang: String,
    /// Stack Exchange site searched by default (`stackoverflow`, `superuser`, ...).
    pub stackexchange_site: String,
}

impl Default for KeylessConfig {
    fn default() -> Self {
        Self {
            web: WebConfig::default(),
            feeds: DEFAULT_FEEDS.iter().map(|s| s.to_string()).collect(),
            wikipedia_lang: "en".into(),
            stackexchange_site: "stackoverflow".into(),
        }
    }
}

/// A built-in keyless channel other than web search (see [`WebChannel`]).
pub struct KeylessChannel {
    inner: Box<dyn Channel>,
}

impl KeylessChannel {
    /// `None` for `Web` and `Memory`.
    pub fn new(id: ChannelId) -> Option<Self> {
        Self::with_config(id, &KeylessConfig::default())
    }

    pub fn with_config(id: ChannelId, cfg: &KeylessConfig) -> Option<Self> {
        use ChannelId as C;
        let inner: Box<dyn Channel> = match id {
            C::Arxiv => Box::new(ArxivChannel::new()),
            C::OpenAlex => Box::new(OpenAlexChannel::new()),
            C::Crossref => Box::new(CrossrefChannel::new()),
            C::SemanticScholar => Box::new(SemanticScholarChannel::new()),
            C::Wikipedia => Box::new(WikipediaChannel::new(&cfg.wikipedia_lang)),
            C::Wikidata => Box::new(WikidataChannel::new()),
            C::Github => Box::new(GithubChannel::new()),
            C::HackerNews => Box::new(HackerNewsChannel::new()),
            C::StackExchange => Box::new(StackExchangeChannel::new(&cfg.stackexchange_site)),
            C::Pubmed => Box::new(PubmedChannel::new()),
            C::Rss => Box::new(RssChannel::new(cfg.feeds.clone())),
            C::Web | C::Memory => return None,
        };
        Some(Self { inner })
    }
}

impl Channel for KeylessChannel {
    fn spec(&self) -> &ChannelSpec {
        self.inner.spec()
    }

    fn search(&self, query: &ChannelQuery, ctx: &ChannelCtx<'_>) -> Result<Vec<SourceRef>, ChannelError> {
        let id = self.spec().id;
        // Only the scholarly graphs answer a citation query; elsewhere it would be an empty search.
        if query.citations.is_some() && !matches!(id, ChannelId::SemanticScholar | ChannelId::OpenAlex) {
            return Err(ChannelError::NotImplemented(id));
        }
        self.inner.search(query, ctx)
    }

    fn lookup(&self, ns: Namespace, id: &str, ctx: &ChannelCtx<'_>) -> Result<Option<SourceRef>, ChannelError> {
        self.inner.lookup(ns, id, ctx)
    }
}

// ------------------------------------------------------------------ shared helpers

/// The spec of a built-in channel (every caller passes a network channel).
pub(crate) fn spec_of(id: ChannelId) -> ChannelSpec {
    builtin_spec(id).unwrap_or_else(|| panic!("{id} has no built-in spec"))
}

/// Percent-encodes a query component (RFC 3986 unreserved characters kept).
pub fn enc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// One GET through the context's HTTP port, any status returned.
pub(crate) fn get_raw(ctx: &ChannelCtx<'_>, id: ChannelId, req: HttpRequest) -> Result<HttpResponse, ChannelError> {
    if ctx.cancel.is_cancelled() {
        return Err(ChannelError::Cancelled);
    }
    ctx.http.get(&req, ctx.cancel).map_err(|detail| {
        if ctx.cancel.is_cancelled() {
            ChannelError::Cancelled
        } else {
            ChannelError::Network { channel: id, detail }
        }
    })
}

/// Maps a non-2xx status to its error: 429 (and 503 with `Retry-After`) is a rate limit.
pub(crate) fn check_status(id: ChannelId, r: HttpResponse) -> Result<HttpResponse, ChannelError> {
    match r.status {
        200..=299 => Ok(r),
        429 => Err(ChannelError::RateLimited { channel: id, retry_after: r.retry_after }),
        503 if r.retry_after.is_some() => Err(ChannelError::RateLimited { channel: id, retry_after: r.retry_after }),
        status => Err(ChannelError::Status { channel: id, status }),
    }
}

/// GET with an `Accept`, 2xx required.
pub(crate) fn get(
    ctx: &ChannelCtx<'_>,
    id: ChannelId,
    url: String,
    accept: &'static str,
) -> Result<HttpResponse, ChannelError> {
    check_status(id, get_raw(ctx, id, HttpRequest::get(url).accept(accept))?)
}

/// GET and parse JSON.
pub(crate) fn get_json(ctx: &ChannelCtx<'_>, id: ChannelId, url: String) -> Result<Value, ChannelError> {
    let r = get(ctx, id, url, "application/json")?;
    parse_json(id, &r.bytes)
}

pub(crate) fn parse_json(id: ChannelId, bytes: &[u8]) -> Result<Value, ChannelError> {
    serde_json::from_slice(bytes).map_err(|e| ChannelError::Parse { channel: id, detail: e.to_string() })
}

/// A JSON string field, trimmed; `None` when absent or empty.
pub(crate) fn s(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::trim).filter(|t| !t.is_empty()).map(str::to_string)
}

/// A JSON number or numeric string as text.
pub(crate) fn num(v: &Value, key: &str) -> Option<String> {
    match v.get(key)? {
        Value::Number(n) => Some(n.to_string()),
        Value::String(t) if !t.is_empty() => Some(t.clone()),
        _ => None,
    }
}

/// At most `n` characters, with an ellipsis when cut.
pub fn clip(text: &str, n: usize) -> String {
    match text.char_indices().nth(n) {
        Some((i, _)) => format!("{}…", text[..i].trim_end()),
        None => text.to_string(),
    }
}

/// Days since 1970-01-01 for a civil date (Howard Hinnant's algorithm).
pub(crate) fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = m as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `YYYY-MM-DD` for a unix timestamp.
pub(crate) fn iso_date(unix: i64) -> String {
    let z = unix.div_euclid(86_400) + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Unix time of 1 January `year`, 00:00 UTC.
pub(crate) fn year_start(year: u16) -> i64 {
    days_from_civil(year as i64, 1, 1) * 86_400
}

/// The `limit` a channel actually asks for.
pub(crate) fn limit(q: &ChannelQuery) -> usize {
    q.limit.clamp(1, MAX_LIMIT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_network_channel_declares_limits_and_independence() {
        for id in ChannelId::ALL {
            let Some(s) = builtin_spec(id) else {
                assert_eq!(id, ChannelId::Memory);
                continue;
            };
            assert!(!s.rate_limits.is_empty() && s.rate_limits.iter().all(|l| l.requests > 0), "{id}");
            assert!(s.endpoint.starts_with("https://") || s.endpoint.starts_with("http://127.0.0.1"), "{id}");
            assert!(!s.indexes.contains(&id), "{id} cannot index itself");
            if id != ChannelId::Web {
                let k = KeylessChannel::new(id).unwrap();
                assert_eq!(k.spec().id, id);
            }
        }
        assert_eq!(builtin_spec(ChannelId::Crossref).unwrap().identity_authority, [Namespace::Doi]);
        assert!(builtin_spec(ChannelId::Web).unwrap().indexes.contains(&ChannelId::Arxiv));
        assert!(KeylessChannel::new(ChannelId::Web).is_none() && KeylessChannel::new(ChannelId::Memory).is_none());
    }

    #[test]
    fn dates_round_trip_through_days() {
        assert_eq!(iso_date(0), "1970-01-01");
        assert_eq!(iso_date(1_700_000_000), "2023-11-14");
        assert_eq!(year_start(2024), 1_704_067_200);
        assert_eq!(iso_date(year_start(2000) - 1), "1999-12-31");
        assert_eq!(iso_date(days_from_civil(2024, 2, 29) * 86_400), "2024-02-29");
    }

    #[test]
    fn helpers_encode_and_clip() {
        assert_eq!(enc("a b/c&d=é"), "a%20b%2Fc%26d%3D%C3%A9");
        assert_eq!(clip("abcdef", 3), "abc…");
        assert_eq!(clip("abc", 3), "abc");
    }
}
