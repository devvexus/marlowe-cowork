//! The keyless channel registry and routing by question type (ADR-036 §3): the plan picks
//! channels from the question's shape before spending, never fans out to all of them.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use marlowe_model::CancelToken;
use marlowe_proto::ChannelId;

use crate::cache::CacheKey;
use crate::channel::Limiter;
use crate::channel::{
    Channel, ChannelCtx, ChannelError, ChannelQuery, ChannelSpec, HttpGet, HttpRequest, HttpResponse,
};
use crate::channels::{HostedSearch, KeylessChannel, KeylessConfig, WebChannel, WebConfig};
use crate::source::{Namespace, SourceRef};

/// Back-off applied when a server rate-limits without saying for how long.
pub const DEFAULT_BACKOFF: Duration = Duration::from_secs(30);

/// The HTTP port a channel sees inside one registry call: every request takes a limiter slot,
/// except the first, whose slot the registry already waited for.
struct Paced<'a> {
    inner: &'a dyn HttpGet,
    limiter: &'a Limiter,
    prepaid: AtomicBool,
}

impl HttpGet for Paced<'_> {
    fn get(&self, request: &HttpRequest, cancel: &CancelToken) -> Result<HttpResponse, String> {
        if !self.prepaid.swap(false, Ordering::SeqCst) && !self.limiter.wait(cancel) {
            return Err("cancelled".into());
        }
        self.inner.get(request, cancel)
    }
}

/// The shape of a research question, which decides its channels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum QuestionType {
    /// Published research: papers, results, citations.
    PublishedResearch,
    /// Biomedical or clinical literature.
    Biomedical,
    /// A library's behaviour, code, tooling, an error message.
    Engineering,
    /// What happened recently: releases, news, discussion.
    Recent,
    /// A named entity: a person, organisation, place, thing.
    Entity,
    General,
}

/// The routing table. `Memory` is always consulted by the playbook and is not listed here.
pub fn route(q: QuestionType) -> &'static [ChannelId] {
    use ChannelId as C;
    match q {
        QuestionType::PublishedResearch => &[C::Arxiv, C::SemanticScholar, C::OpenAlex, C::Crossref],
        QuestionType::Biomedical => &[C::Pubmed, C::OpenAlex, C::SemanticScholar, C::Crossref],
        QuestionType::Engineering => &[C::Github, C::StackExchange, C::HackerNews, C::Web],
        QuestionType::Recent => &[C::HackerNews, C::Rss, C::Web],
        QuestionType::Entity => &[C::Wikidata, C::Wikipedia],
        QuestionType::General => &[C::Wikipedia, C::Web],
    }
}

/// A first guess at a question's type from its words. The planner may override it.
pub fn classify(question: &str) -> QuestionType {
    let q = format!(" {} ", question.to_lowercase());
    let any = |words: &[&str]| words.iter().any(|w| q.contains(w));
    if any(&[
        " clinical",
        " patient",
        " disease",
        " drug",
        " protein",
        " gene ",
        " pubmed",
        " trial",
        " cancer",
        " mrna",
    ]) {
        QuestionType::Biomedical
    } else if any(&[
        " paper",
        " papers",
        " arxiv",
        " literature",
        " ablation",
        " benchmark",
        " state of the art",
        " sota",
        " study",
        " studies",
        " peer-reviewed",
        " citation",
        " survey of",
        " results on",
    ]) {
        QuestionType::PublishedResearch
    } else if any(&[
        " library",
        " crate",
        " api ",
        " error",
        " compile",
        " bug",
        " github",
        " install",
        " function",
        " rust ",
        " python",
        " npm",
        " cargo",
        " stack trace",
        " exception",
        " how do i",
        " config",
    ]) {
        QuestionType::Engineering
    } else if any(&[
        " latest",
        " recent",
        " this week",
        " this month",
        " today",
        " announced",
        " released",
        " news",
        " 2026",
    ]) {
        QuestionType::Recent
    } else if any(&[" who is", " who was", " what is ", " where is", " founded", " born", " capital of"]) {
        QuestionType::Entity
    } else {
        QuestionType::General
    }
}

/// Why a channel could not be registered.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RegistryError {
    #[error("{0} is read through the Mind, not registered as a network channel")]
    NotNetwork(ChannelId),
    #[error("{0} is already registered")]
    Duplicate(ChannelId),
    #[error("{0} declares no rate limit")]
    NoRateLimit(ChannelId),
    #[error("{0}: endpoint must be https or loopback http, not {1}")]
    Endpoint(ChannelId, String),
}

struct Entry {
    channel: Arc<dyn Channel>,
    limiter: Arc<Limiter>,
}

/// Registered network channels with their limiters. Nothing here can hold a credential.
#[derive(Default)]
pub struct ChannelRegistry {
    entries: BTreeMap<ChannelId, Entry>,
}

impl ChannelRegistry {
    pub fn empty() -> Self {
        Self::default()
    }

    /// Every built-in keyless channel, plus general web search over `web` (and `hosted`, the
    /// daemon's optional hosted search).
    pub fn keyless(web: WebConfig, hosted: Option<Arc<dyn HostedSearch>>) -> Self {
        Self::keyless_with(&KeylessConfig { web, ..KeylessConfig::default() }, hosted)
    }

    /// [`ChannelRegistry::keyless`] with every channel setting (feeds, Wikipedia edition, ...).
    pub fn keyless_with(cfg: &KeylessConfig, hosted: Option<Arc<dyn HostedSearch>>) -> Self {
        let mut r = Self::empty();
        for id in ChannelId::ALL {
            if let Some(c) = KeylessChannel::with_config(id, cfg) {
                r.register(Arc::new(c)).expect("built-in specs are valid");
            }
        }
        r.register(Arc::new(WebChannel::new(cfg.web.clone(), hosted))).expect("the web spec is valid");
        r
    }

    /// Adds a channel after checking its declaration.
    pub fn register(&mut self, channel: Arc<dyn Channel>) -> Result<(), RegistryError> {
        let spec = channel.spec();
        let id = spec.id;
        if !id.is_network() {
            return Err(RegistryError::NotNetwork(id));
        }
        if self.entries.contains_key(&id) {
            return Err(RegistryError::Duplicate(id));
        }
        if spec.rate_limits.is_empty() || spec.rate_limits.iter().any(|l| l.requests == 0) {
            return Err(RegistryError::NoRateLimit(id));
        }
        let e = spec.endpoint.to_ascii_lowercase();
        let loopback = ["http://127.0.0.1", "http://localhost", "http://[::1]"].iter().any(|p| e.starts_with(p));
        if !e.starts_with("https://") && !loopback {
            return Err(RegistryError::Endpoint(id, spec.endpoint.clone()));
        }
        let limiter = Limiter::shared(id, &spec.rate_limits);
        self.entries.insert(id, Entry { channel, limiter });
        Ok(())
    }

    pub fn get(&self, id: ChannelId) -> Option<Arc<dyn Channel>> {
        self.entries.get(&id).map(|e| e.channel.clone())
    }

    pub fn ids(&self) -> Vec<ChannelId> {
        self.entries.keys().copied().collect()
    }

    pub fn specs(&self) -> Vec<ChannelSpec> {
        self.entries.values().map(|e| e.channel.spec().clone()).collect()
    }

    pub fn limiter(&self, id: ChannelId) -> Option<Arc<Limiter>> {
        self.entries.get(&id).map(|e| e.limiter.clone())
    }

    /// The registered channels for a question type, in routing order.
    pub fn route(&self, q: QuestionType) -> Vec<ChannelId> {
        route(q).iter().copied().filter(|c| self.entries.contains_key(c)).collect()
    }

    /// Classifies `question` and routes it.
    pub fn route_question(&self, question: &str) -> (QuestionType, Vec<ChannelId>) {
        let q = classify(question);
        (q, self.route(q))
    }

    /// One query on `id`: cache first, then the declared rate limit, then the channel, then the
    /// cache again. Every HTTP request the channel makes takes a limiter slot; a rate-limit
    /// answer holds the channel's limiter for the server's `Retry-After` (or
    /// [`DEFAULT_BACKOFF`]). Errors are never cached; an empty answer is.
    pub fn search(
        &self,
        id: ChannelId,
        query: &ChannelQuery,
        ctx: &ChannelCtx<'_>,
    ) -> Result<Vec<SourceRef>, ChannelError> {
        let e = self.entries.get(&id).ok_or(ChannelError::NotRegistered(id))?;
        let key = CacheKey::of(&query.cache_parts(id));
        let ttl = e.channel.spec().cache_ttl;
        if let Some(hit) = ctx.cache.and_then(|c| c.get_json::<Vec<SourceRef>>(id, &key, ttl)) {
            return Ok(hit);
        }
        let out = self.paced(e, ctx, |c| e.channel.search(query, c))?;
        if let Some(c) = ctx.cache {
            let _ = c.put_json(id, &key, &out);
        }
        Ok(out)
    }

    /// The authoritative record for `(ns, value)` from the channel that owns the namespace: how
    /// [`crate::Deduper::unconfirmed`] identifiers get confirmed. Cached like a search.
    pub fn lookup(&self, ns: Namespace, value: &str, ctx: &ChannelCtx<'_>) -> Result<Option<SourceRef>, ChannelError> {
        let e = self
            .entries
            .values()
            .find(|e| e.channel.spec().identity_authority.contains(&ns))
            .ok_or(ChannelError::NoLookup { channel: ChannelId::Web, namespace: ns })?;
        let id = e.channel.spec().id;
        let key = CacheKey::of(&["lookup", id.as_str(), &format!("{ns:?}"), value.trim()]);
        let ttl = e.channel.spec().cache_ttl;
        if let Some(hit) = ctx.cache.and_then(|c| c.get_json::<Option<SourceRef>>(id, &key, ttl)) {
            return Ok(hit);
        }
        let out = self.paced(e, ctx, |c| e.channel.lookup(ns, value, c))?;
        if let Some(c) = ctx.cache {
            let _ = c.put_json(id, &key, &out);
        }
        Ok(out)
    }

    /// Waits for a slot, runs `f` with a paced HTTP port, and turns a rate-limit answer into a
    /// hold on the limiter.
    fn paced<T>(
        &self,
        e: &Entry,
        ctx: &ChannelCtx<'_>,
        f: impl FnOnce(&ChannelCtx<'_>) -> Result<T, ChannelError>,
    ) -> Result<T, ChannelError> {
        if !e.limiter.wait(ctx.cancel) {
            return Err(ChannelError::Cancelled);
        }
        let paced = Paced { inner: ctx.http, limiter: &e.limiter, prepaid: AtomicBool::new(true) };
        let inner = ChannelCtx { http: &paced, cache: ctx.cache, cancel: ctx.cancel };
        let out = f(&inner);
        if let Err(ChannelError::RateLimited { retry_after, .. }) = &out {
            e.limiter.penalise(retry_after.unwrap_or(DEFAULT_BACKOFF));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use marlowe_model::CancelToken;

    use super::*;
    use crate::cache::ResearchCache;
    use crate::channel::{HttpGet, HttpRequest, HttpResponse, RateLimit, ResultShape};

    struct NoNet;
    impl HttpGet for NoNet {
        fn get(&self, _: &HttpRequest, _: &CancelToken) -> Result<HttpResponse, String> {
            Err("offline".into())
        }
    }

    struct Counting(ChannelSpec, std::sync::atomic::AtomicU32);
    impl Channel for Counting {
        fn spec(&self) -> &ChannelSpec {
            &self.0
        }
        fn search(&self, q: &ChannelQuery, _: &ChannelCtx<'_>) -> Result<Vec<SourceRef>, ChannelError> {
            self.1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(vec![SourceRef::new(ChannelId::Rss, "https://e.org/feed", &q.text)])
        }
    }

    fn spec(id: ChannelId, endpoint: &str) -> ChannelSpec {
        ChannelSpec {
            id,
            endpoint: endpoint.into(),
            rate_limits: vec![RateLimit::every(Duration::from_millis(1))],
            shape: ResultShape::Feed,
            identity_authority: vec![],
            curated_ids: false,
            indexes: vec![],
            cache_ttl: Duration::from_secs(60),
        }
    }

    #[test]
    fn the_keyless_registry_has_every_network_channel_and_routes_by_type() {
        let r = ChannelRegistry::keyless(WebConfig::default(), None);
        assert_eq!(r.ids().len(), 12);
        assert!(r.get(ChannelId::Memory).is_none());
        assert_eq!(r.route(QuestionType::PublishedResearch)[0], ChannelId::Arxiv);
        assert_eq!(classify("What do recent papers say about LoRA rank ablations?"), QuestionType::PublishedResearch);
        assert_eq!(classify("Why does cargo fail to compile ring on ARM64?"), QuestionType::Engineering);
        assert_eq!(classify("Who is the founder of Hugging Face?"), QuestionType::Entity);
        assert_eq!(classify("Clinical trial results for GLP-1 drugs"), QuestionType::Biomedical);
        assert_eq!(classify("What was announced at the latest WWDC?"), QuestionType::Recent);
        assert_eq!(r.route_question("how tall is a giraffe").0, QuestionType::General);
    }

    #[test]
    fn registration_rejects_memory_duplicates_plain_http_and_missing_limits() {
        let mut r = ChannelRegistry::empty();
        let c = |id, ep: &str| Arc::new(Counting(spec(id, ep), Default::default()));
        assert_eq!(r.register(c(ChannelId::Memory, "https://x")), Err(RegistryError::NotNetwork(ChannelId::Memory)));
        assert!(matches!(r.register(c(ChannelId::Rss, "http://example.com")), Err(RegistryError::Endpoint(..))));
        r.register(c(ChannelId::Rss, "https://example.com")).unwrap();
        assert_eq!(r.register(c(ChannelId::Rss, "https://example.com")), Err(RegistryError::Duplicate(ChannelId::Rss)));
        let mut bad = spec(ChannelId::Github, "https://api.github.com");
        bad.rate_limits.clear();
        assert_eq!(
            r.register(Arc::new(Counting(bad, Default::default()))),
            Err(RegistryError::NoRateLimit(ChannelId::Github))
        );
        r.register(c(ChannelId::Web, "http://127.0.0.1:8080")).unwrap();
    }

    #[test]
    fn searches_hit_the_cache_before_the_limiter_and_the_channel() {
        let home = std::env::temp_dir().join(format!("marlowe-research-reg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let cache = ResearchCache::at_home(&home);
        let counting = Arc::new(Counting(spec(ChannelId::Rss, "https://e.org"), Default::default()));
        let mut r = ChannelRegistry::empty();
        r.register(counting.clone()).unwrap();
        let cancel = CancelToken::new();
        let ctx = ChannelCtx { http: &NoNet, cache: Some(&cache), cancel: &cancel };
        let q = ChannelQuery::new("rust release", 5);
        let a = r.search(ChannelId::Rss, &q, &ctx).unwrap();
        let b = r.search(ChannelId::Rss, &q, &ctx).unwrap();
        assert_eq!(a, b);
        assert_eq!(counting.1.load(std::sync::atomic::Ordering::SeqCst), 1, "second answer came from the cache");
        assert_eq!(r.search(ChannelId::Github, &q, &ctx), Err(ChannelError::NotRegistered(ChannelId::Github)));
        let keyless = ChannelRegistry::keyless(WebConfig::default(), None);
        let ctx = ChannelCtx { http: &NoNet, cache: None, cancel: &cancel };
        assert_eq!(
            keyless.search(ChannelId::Crossref, &q, &ctx),
            Err(ChannelError::Network { channel: ChannelId::Crossref, detail: "offline".into() })
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Answers 429 with `Retry-After: 1` once, then 200 with an empty JSON object.
    struct Busy(std::sync::Mutex<Vec<std::time::Instant>>);
    impl HttpGet for Busy {
        fn get(&self, r: &HttpRequest, _: &CancelToken) -> Result<HttpResponse, String> {
            let mut seen = self.0.lock().unwrap();
            seen.push(std::time::Instant::now());
            let first = seen.len() == 1;
            Ok(HttpResponse {
                status: if first { 429 } else { 200 },
                content_type: None,
                bytes: b"{}".to_vec(),
                final_url: r.url.clone(),
                retry_after: first.then_some(Duration::from_millis(400)),
            })
        }
    }

    /// Makes two requests per search.
    struct TwoStep(ChannelSpec);
    impl Channel for TwoStep {
        fn spec(&self) -> &ChannelSpec {
            &self.0
        }
        fn search(&self, _: &ChannelQuery, ctx: &ChannelCtx<'_>) -> Result<Vec<SourceRef>, ChannelError> {
            for _ in 0..2 {
                let r = ctx.http.get(&HttpRequest::get("https://e.org/x"), ctx.cancel).unwrap();
                if r.status == 429 {
                    return Err(ChannelError::RateLimited { channel: self.0.id, retry_after: r.retry_after });
                }
            }
            Ok(vec![])
        }
    }

    #[test]
    fn every_request_is_paced_and_rate_limits_hold_the_channel() {
        let mut sp = spec(ChannelId::Rss, "https://e.org");
        sp.rate_limits = vec![RateLimit::every(Duration::from_millis(150))];
        let mut r = ChannelRegistry::empty();
        r.register(Arc::new(TwoStep(sp))).unwrap();
        let http = Busy(Default::default());
        let cancel = CancelToken::new();
        let ctx = ChannelCtx { http: &http, cache: None, cancel: &cancel };
        let q = ChannelQuery::new("x", 1);
        assert!(matches!(r.search(ChannelId::Rss, &q, &ctx), Err(ChannelError::RateLimited { .. })));
        assert_eq!(r.search(ChannelId::Rss, &q, &ctx), Ok(vec![]));
        let t = http.0.lock().unwrap().clone();
        assert_eq!(t.len(), 3);
        assert!(t[1] - t[0] >= Duration::from_millis(380), "Retry-After held the channel: {:?}", t[1] - t[0]);
        assert!(t[2] - t[1] >= Duration::from_millis(140), "the second request took its own slot: {:?}", t[2] - t[1]);
    }
}
