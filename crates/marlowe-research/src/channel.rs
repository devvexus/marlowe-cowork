//! The channel contract: a declared spec (no credential field exists), a rate limiter that
//! schedules against the declaration, and the HTTP port channels fetch through.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use marlowe_model::CancelToken;
use marlowe_proto::ChannelId;

use crate::cache::ResearchCache;
use crate::source::{Identity, Namespace, SourceRef};

/// Sent with every research request: names the agent and says it is automated.
pub const USER_AGENT: &str =
    "MarloweCowork/0.1 (local research assistant; automated, rate-limited; honours robots.txt)";

/// `requests` per `per`, declared by the channel (ADR-036 §2.1). There is no "unlimited".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RateLimit {
    pub requests: u32,
    pub per: Duration,
}

impl RateLimit {
    pub const fn new(requests: u32, per: Duration) -> Self {
        Self { requests, per }
    }

    /// One request per `gap`.
    pub const fn every(gap: Duration) -> Self {
        Self { requests: 1, per: gap }
    }
}

/// Schedules requests against every declared limit at once (sliding windows). Thread-safe: one
/// limiter per channel (or host) is shared by every scout.
#[derive(Debug)]
pub struct Limiter {
    limits: Vec<RateLimit>,
    /// Granted (possibly future) slot times per limit, sorted.
    log: Mutex<Vec<VecDeque<Instant>>>,
    /// No slot before this (a server's `Retry-After`, or a declared backoff).
    hold: Mutex<Option<Instant>>,
}

impl Limiter {
    pub fn new(limits: &[RateLimit]) -> Self {
        let limits: Vec<RateLimit> = limits.iter().copied().filter(|l| l.requests > 0).collect();
        let log = Mutex::new(limits.iter().map(|_| VecDeque::new()).collect());
        Self { limits, log, hold: Mutex::new(None) }
    }

    /// Grants no slot for `d` from now: the server asked us to back off.
    pub fn penalise(&self, d: Duration) {
        let until = Instant::now() + d;
        let mut h = self.hold.lock().unwrap_or_else(|p| p.into_inner());
        *h = Some(h.map_or(until, |x| x.max(until)));
    }

    pub fn limits(&self) -> &[RateLimit] {
        &self.limits
    }

    /// The process-wide limiter for `id` under `limits`. Rate limits are per client address, so
    /// every registry in the process (and the exec `arxiv` tool) shares one schedule per channel.
    pub fn shared(id: ChannelId, limits: &[RateLimit]) -> Arc<Limiter> {
        type Shared = Mutex<HashMap<(ChannelId, Vec<RateLimit>), Arc<Limiter>>>;
        static ALL: OnceLock<Shared> = OnceLock::new();
        let mut all = ALL.get_or_init(Default::default).lock().unwrap_or_else(|p| p.into_inner());
        all.entry((id, limits.to_vec())).or_insert_with(|| Arc::new(Limiter::new(limits))).clone()
    }

    /// Reserves the next slot and returns how long until it.
    pub fn reserve(&self) -> Duration {
        let now = Instant::now();
        let hold = *self.hold.lock().unwrap_or_else(|p| p.into_inner());
        let mut log = self.log.lock().unwrap_or_else(|p| p.into_inner());
        let mut at = hold.map_or(now, |h| h.max(now));
        for (l, w) in self.limits.iter().zip(log.iter_mut()) {
            while w.front().is_some_and(|t| *t + l.per <= now) {
                w.pop_front();
            }
            let n = l.requests as usize;
            if w.len() >= n {
                at = at.max(w[w.len() - n] + l.per);
            }
        }
        for w in log.iter_mut() {
            w.push_back(at);
        }
        at.saturating_duration_since(now)
    }

    /// Blocks until this caller's slot. False if cancelled while waiting (the slot is forfeited).
    pub fn wait(&self, cancel: &CancelToken) -> bool {
        let until = Instant::now() + self.reserve();
        loop {
            if cancel.is_cancelled() {
                return false;
            }
            let now = Instant::now();
            if now >= until {
                return true;
            }
            std::thread::sleep((until - now).min(Duration::from_millis(100)));
        }
    }
}

/// One GET through the HTTP port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest {
    pub url: String,
    pub accept: Option<&'static str>,
    pub user_agent: &'static str,
}

impl HttpRequest {
    pub fn get(url: impl Into<String>) -> Self {
        Self { url: url.into(), accept: None, user_agent: USER_AGENT }
    }

    pub fn accept(mut self, accept: &'static str) -> Self {
        self.accept = Some(accept);
        self
    }
}

/// A completed GET.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub content_type: Option<String>,
    pub bytes: Vec<u8>,
    pub final_url: String,
    /// `Retry-After`, when the server sent one with a 429/503.
    pub retry_after: Option<Duration>,
}

/// The network as channels see it. The daemon supplies marlowe-net; tests supply recordings.
/// Implementations send `request.user_agent` and never add credentials.
pub trait HttpGet: Send + Sync {
    fn get(&self, request: &HttpRequest, cancel: &CancelToken) -> Result<HttpResponse, String>;
}

/// What one result looks like on the wire, so a parser need not guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultShape {
    Atom,
    Json,
    Html,
    /// RSS 2.0 or Atom, sniffed per feed.
    Feed,
}

/// How far a channel's identifier may be believed (ADR-036 §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Assertion {
    /// Text the channel returned asserts it (a page, a post): a claim, never an identity.
    Asserted,
    /// A curated index returned it in a structured field.
    Indexed,
    /// The channel is the authority for the namespace.
    Authoritative,
}

/// Everything known about a channel at load time. No credential field exists (ADR-036 §2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelSpec {
    pub id: ChannelId,
    /// Base URL: https, or loopback http for a local SearXNG.
    pub endpoint: String,
    /// Every limit holds at once. Never empty.
    pub rate_limits: Vec<RateLimit>,
    pub shape: ResultShape,
    /// Namespaces this channel is authoritative for.
    pub identity_authority: Vec<Namespace>,
    /// Structured identifiers from this channel are curated (scholarly indexes, Wikidata, GitHub).
    pub curated_ids: bool,
    /// Channels this one mirrors or aggregates; empty is an explicit claim of independence.
    pub indexes: Vec<ChannelId>,
    /// Results cached this long.
    pub cache_ttl: Duration,
}

impl ChannelSpec {
    /// How far this channel's `ns` identifiers may be believed. A canonical URL is always the
    /// resource itself.
    pub fn assertion(&self, ns: Namespace) -> Assertion {
        if ns == Namespace::Url || self.identity_authority.contains(&ns) {
            Assertion::Authoritative
        } else if self.curated_ids {
            Assertion::Indexed
        } else {
            Assertion::Asserted
        }
    }
}

/// Which way a citation query follows the graph from its work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CitationDirection {
    /// Works that cite it (forward).
    CitedBy,
    /// Works it cites (backward).
    References,
}

impl CitationDirection {
    pub fn as_str(self) -> &'static str {
        match self {
            CitationDirection::CitedBy => "cited_by",
            CitationDirection::References => "references",
        }
    }
}

/// Citation expansion from one work. Built by the harness from a source's identity, never
/// parsed out of query text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CitationQuery {
    pub work: Identity,
    pub direction: CitationDirection,
}

/// One channel search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelQuery {
    pub text: String,
    /// Results wanted (the plan's breadth, or more for selection).
    pub limit: usize,
    /// Only results on or after this year, where the channel can filter.
    pub since_year: Option<u16>,
    /// A feed to read (RSS). Only the user sets this; a URL in `text` is never fetched.
    pub feed_url: Option<String>,
    /// Citation expansion instead of a text search (Semantic Scholar, OpenAlex).
    pub citations: Option<CitationQuery>,
}

impl ChannelQuery {
    pub fn new(text: impl Into<String>, limit: usize) -> Self {
        Self { text: text.into(), limit, since_year: None, feed_url: None, citations: None }
    }

    /// Works citing (or cited by) `work`.
    pub fn citations(work: Identity, direction: CitationDirection, limit: usize) -> Self {
        Self { citations: Some(CitationQuery { work, direction }), ..Self::new("", limit) }
    }

    /// The cache key parts for this query on `channel`.
    pub fn cache_parts(&self, channel: ChannelId) -> Vec<String> {
        let mut parts = vec![
            channel.as_str().to_string(),
            self.text.trim().to_lowercase(),
            self.limit.to_string(),
            self.since_year.map_or_else(String::new, |y| y.to_string()),
        ];
        if let Some(f) = &self.feed_url {
            parts.push(format!("feed:{}", f.trim()));
        }
        if let Some(c) = &self.citations {
            let keys: Vec<String> = c.work.keys().into_iter().map(|(ns, v)| format!("{ns:?}={v}")).collect();
            parts.push(format!("{}:{}", c.direction.as_str(), keys.join(",")));
        }
        parts
    }
}

/// What a channel search may use.
pub struct ChannelCtx<'a> {
    pub http: &'a dyn HttpGet,
    pub cache: Option<&'a ResearchCache>,
    pub cancel: &'a CancelToken,
}

/// Why a channel search produced nothing.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ChannelError {
    #[error("{0} is not implemented yet")]
    NotImplemented(ChannelId),
    #[error("{0} is not registered")]
    NotRegistered(ChannelId),
    #[error("{channel}: HTTP {status}")]
    Status { channel: ChannelId, status: u16 },
    #[error("{channel}: rate limited; retry after {retry_after:?}")]
    RateLimited { channel: ChannelId, retry_after: Option<Duration> },
    #[error("{channel}: {detail}")]
    Network { channel: ChannelId, detail: String },
    #[error("{channel}: unreadable response: {detail}")]
    Parse { channel: ChannelId, detail: String },
    #[error("{channel}: no backend answered ({detail})")]
    NoBackend { channel: ChannelId, detail: String },
    #[error("{channel} cannot look up {namespace:?} identifiers")]
    NoLookup { channel: ChannelId, namespace: Namespace },
    #[error("cancelled")]
    Cancelled,
}

/// A research source.
pub trait Channel: Send + Sync {
    fn spec(&self) -> &ChannelSpec;

    /// Runs one query. Implementations do not rate-limit or cache themselves; the registry does
    /// both around this call (every HTTP request through `ctx.http` takes a limiter slot).
    fn search(&self, query: &ChannelQuery, ctx: &ChannelCtx<'_>) -> Result<Vec<SourceRef>, ChannelError>;

    /// The record for one identifier in a namespace this channel is authoritative for: how an
    /// identifier asserted elsewhere is confirmed before it may merge anything (ADR-036 §5).
    /// `Ok(None)` when the authority has no such record.
    fn lookup(&self, ns: Namespace, _id: &str, _ctx: &ChannelCtx<'_>) -> Result<Option<SourceRef>, ChannelError> {
        Err(ChannelError::NoLookup { channel: self.spec().id, namespace: ns })
    }

    /// Whether the channel can answer at all, checked cheaply before a plan routes to it (no
    /// search is spent). `Err` says why not. Most channels are a public API: assumed ready.
    fn ready(&self, _ctx: &ChannelCtx<'_>) -> Result<(), String> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_limiter_spaces_slots_by_the_strictest_limit() {
        let l =
            Limiter::new(&[RateLimit::new(2, Duration::from_millis(400)), RateLimit::every(Duration::from_millis(50))]);
        let a = l.reserve();
        let b = l.reserve();
        let c = l.reserve();
        assert_eq!(a, Duration::ZERO);
        assert!(b >= Duration::from_millis(45) && b <= Duration::from_millis(55), "{b:?}");
        assert!(c >= Duration::from_millis(390), "the 2-per-400ms window binds the third: {c:?}");
    }

    #[test]
    fn a_penalty_holds_every_later_slot() {
        let l = Limiter::new(&[RateLimit::every(Duration::from_millis(1))]);
        l.penalise(Duration::from_millis(300));
        assert!(l.reserve() >= Duration::from_millis(290));
        l.penalise(Duration::from_millis(10));
        assert!(l.reserve() >= Duration::from_millis(250), "a shorter penalty never shortens a longer one");
    }

    #[test]
    fn a_cancelled_wait_forfeits_its_slot() {
        let l = Limiter::new(&[RateLimit::every(Duration::from_secs(30))]);
        assert!(l.wait(&CancelToken::new()));
        let cancel = CancelToken::new();
        cancel.cancel();
        let t = Instant::now();
        assert!(!l.wait(&cancel));
        assert!(t.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn assertions_follow_authority_then_curation() {
        let spec = ChannelSpec {
            id: ChannelId::SemanticScholar,
            endpoint: "https://api.semanticscholar.org".into(),
            rate_limits: vec![RateLimit::every(Duration::from_secs(1))],
            shape: ResultShape::Json,
            identity_authority: vec![Namespace::S2],
            curated_ids: true,
            indexes: vec![ChannelId::Arxiv],
            cache_ttl: Duration::from_secs(60),
        };
        assert_eq!(spec.assertion(Namespace::S2), Assertion::Authoritative);
        assert_eq!(spec.assertion(Namespace::Doi), Assertion::Indexed);
        assert_eq!(spec.assertion(Namespace::Url), Assertion::Authoritative);
        let page = ChannelSpec { curated_ids: false, identity_authority: vec![], ..spec };
        assert_eq!(page.assertion(Namespace::Doi), Assertion::Asserted);
    }
}
