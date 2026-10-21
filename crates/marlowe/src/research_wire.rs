//! Deep research v2's I/O for the daemon: the keyless channel registry, the corpus fetcher, the
//! research cache, and the user's settings in `<home>\research.json`.
//!
//! ```json
//! { "searxng_url": "http://127.0.0.1:8080", "duckduckgo": true,
//!   "auto_approve_under_tokens": null, "fetch_concurrency": 16,
//!   "feeds": [], "wikipedia_lang": "en", "stackexchange_site": "stackoverflow" }
//! ```
//!
//! Every field is optional. A file that does not parse is reported on stderr (the daemon log) and
//! the defaults apply.

use std::path::Path;
use std::sync::Arc;

use marlowe_crew::playbook::research_v2::{ChannelIo, FetchedPage, PageFetcher, ResearchIo, ResearchV2Config};
use marlowe_exec::corpus::{Corpus, Outcome};
use marlowe_exec::{SearchBackend, SearchResponse, SearchResult};
use marlowe_model::CancelToken;
use marlowe_research::{
    ChannelCtx, ChannelError, ChannelId, ChannelQuery, ChannelRegistry, HttpGet, KeylessConfig, NetHttp, ResearchCache,
    WebConfig,
};
use serde::Deserialize;

pub const SETTINGS_FILE: &str = "research.json";

/// `<home>\research.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ResearchSettings {
    /// SearXNG base URL; absent = loopback default, `""` = no SearXNG.
    pub searxng_url: Option<String>,
    /// DuckDuckGo HTML lite as the second web backend (default on).
    pub duckduckgo: Option<bool>,
    /// Ignored: accepted so an old research.json still parses. Web search never uses a paid API.
    pub hosted_search: Option<bool>,
    /// Plans quoted under this many model tokens start without asking (default off).
    pub auto_approve_under_tokens: Option<u64>,
    pub fetch_concurrency: Option<usize>,
    pub feeds: Option<Vec<String>>,
    pub wikipedia_lang: Option<String>,
    pub stackexchange_site: Option<String>,
}

impl ResearchSettings {
    /// The settings in `home`, or the defaults (said on stderr when the file is unreadable).
    pub fn load(home: &Path) -> Self {
        let path = home.join(SETTINGS_FILE);
        let Ok(text) = std::fs::read_to_string(&path) else { return Self::default() };
        serde_json::from_str(&text).unwrap_or_else(|e| {
            eprintln!("marlowe: {} is not valid research settings ({e}); using the defaults", path.display());
            Self::default()
        })
    }

    pub fn keyless(&self) -> KeylessConfig {
        let mut k = KeylessConfig::default();
        let d = WebConfig::default();
        k.web = WebConfig {
            searxng_url: match self.searxng_url.as_deref().map(str::trim) {
                None => d.searxng_url,
                Some("") => None,
                Some(u) if searxng_url_ok(u) => Some(u.to_string()),
                Some(u) => {
                    eprintln!(
                        "marlowe: research.json searxng_url {u} is neither https nor http on this machine \
                         (127.0.0.1, localhost, [::1]); SearXNG is skipped"
                    );
                    None
                }
            },
            duckduckgo: self.duckduckgo.unwrap_or(d.duckduckgo),
        };
        if let Some(f) = &self.feeds {
            k.feeds = f.clone();
        }
        if let Some(l) = self.wikipedia_lang.as_deref().map(str::trim).filter(|l| !l.is_empty()) {
            k.wikipedia_lang = l.to_string();
        }
        if let Some(s) = self.stackexchange_site.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            k.stackexchange_site = s.to_string();
        }
        k
    }

    pub fn v2(&self) -> ResearchV2Config {
        let mut c = ResearchV2Config { auto_approve_under_tokens: self.auto_approve_under_tokens, ..Default::default() };
        if let Some(n) = self.fetch_concurrency.filter(|n| *n > 0) {
            c.fetch_concurrency = n.min(64);
        }
        c
    }
}

/// A SearXNG URL the registry accepts: https anywhere, plain http only to this machine (a LAN
/// SearXNG would carry queries in the clear).
pub fn searxng_url_ok(url: &str) -> bool {
    let u = url.trim().to_ascii_lowercase();
    if u.starts_with("https://") {
        return u.len() > "https://".len();
    }
    let Some(rest) = u.strip_prefix("http://") else { return false };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = if host.starts_with('[') { host.split(']').next().map(|h| format!("{h}]")).unwrap_or_default() } else { host.split(':').next().unwrap_or("").to_string() };
    matches!(host.as_str(), "127.0.0.1" | "localhost" | "[::1]")
}

/// The corpus fetcher as crew's [`PageFetcher`]: a read page becomes its markdown and links;
/// anything else carries only the harness-written reason.
pub struct CorpusPages(pub Corpus);

impl PageFetcher for CorpusPages {
    fn fetch(&self, urls: &[String], concurrency: usize, cancel: &CancelToken) -> Vec<FetchedPage> {
        self.0.fetch(urls, concurrency, cancel).into_iter().map(page_of).collect()
    }
}

pub fn page_of(o: Outcome) -> FetchedPage {
    match o {
        Outcome::Read(p) => FetchedPage {
            url: p.url.clone(),
            final_url: p.final_url.clone(),
            title: p.document.title.clone(),
            text: p.markdown.clone(),
            links: p.links().to_vec(),
            meta: p.document.meta.clone(),
            error: None,
        },
        other => FetchedPage {
            url: other.url().to_string(),
            final_url: other.url().to_string(),
            error: other.why_not().or_else(|| Some("not read".into())),
            ..Default::default()
        },
    }
}

/// The product [`ResearchIo`] for `home`. Web search is keyless: SearXNG, then DuckDuckGo lite.
pub fn research_io(home: &Path, settings: &ResearchSettings) -> Arc<dyn ResearchIo> {
    Arc::new(ChannelIo {
        registry: Arc::new(ChannelRegistry::keyless_with(&settings.keyless(), None)),
        http: Arc::new(NetHttp::new()),
        cache: Some(Arc::new(ResearchCache::at_home(home))),
        pages: Arc::new(CorpusPages(Corpus::product())),
    })
}

/// The `web_search` tool's backend: the same keyless web channel deep research uses (SearXNG
/// when it answers, else DuckDuckGo HTML lite), under `<home>\research.json`. No key, no paid API.
pub struct KeylessSearch {
    registry: ChannelRegistry,
    http: Arc<dyn HttpGet>,
    cache: Option<ResearchCache>,
    searxng: Option<String>,
}

/// Results a `web_search` call returns.
const SEARCH_RESULTS: usize = 10;

impl KeylessSearch {
    pub fn new(home: &Path, settings: &ResearchSettings) -> Self {
        Self::with_http(home, settings, Arc::new(NetHttp::new()))
    }

    pub fn with_http(home: &Path, settings: &ResearchSettings, http: Arc<dyn HttpGet>) -> Self {
        let keyless = settings.keyless();
        Self {
            searxng: keyless.web.searxng_url.clone(),
            registry: ChannelRegistry::keyless_with(&keyless, None),
            http,
            cache: Some(ResearchCache::at_home(home)),
        }
    }

    pub fn registry(&self) -> &ChannelRegistry {
        &self.registry
    }

    fn no_backend(&self, detail: &str) -> String {
        let url = self.searxng.as_deref().unwrap_or(marlowe_research::DEFAULT_SEARXNG);
        format!(
            "no web search backend answered ({detail}): run SearXNG at {url} or enable duckduckgo in research.json"
        )
    }
}

impl SearchBackend for KeylessSearch {
    fn search(&self, query: &str, cancel: &CancelToken) -> Result<SearchResponse, String> {
        let ctx = ChannelCtx { http: self.http.as_ref(), cache: self.cache.as_ref(), cancel };
        let q = ChannelQuery::new(query, SEARCH_RESULTS);
        match self.registry.search(ChannelId::Web, &q, &ctx) {
            Ok(found) => Ok(SearchResponse {
                answer: String::new(),
                results: found
                    .into_iter()
                    .take(SEARCH_RESULTS)
                    .map(|s| SearchResult { title: s.title, url: s.url, snippet: s.snippet })
                    .collect(),
            }),
            Err(ChannelError::NoBackend { detail, .. }) => Err(self.no_backend(&detail)),
            Err(ChannelError::NotRegistered(_)) => Err(self.no_backend("the web channel is not registered")),
            Err(e) => Err(e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_default_and_parse() {
        let s: ResearchSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(s.keyless().web, WebConfig::default());
        assert_eq!(s.v2(), ResearchV2Config::default());
        let s: ResearchSettings = serde_json::from_str(
            r#"{"searxng_url": "", "duckduckgo": false, "auto_approve_under_tokens": 50000, "fetch_concurrency": 999}"#,
        )
        .unwrap();
        assert_eq!(s.keyless().web, WebConfig { searxng_url: None, duckduckgo: false });
        assert_eq!(s.v2().auto_approve_under_tokens, Some(50_000));
        assert_eq!(s.v2().fetch_concurrency, 64);
        assert!(serde_json::from_str::<ResearchSettings>(r#"{"api_key": "x"}"#).is_err(), "no credential field");
    }

    /// Found in review: a LAN SearXNG in research.json panicked the daemon at boot.
    #[test]
    fn a_searxng_url_off_this_machine_over_http_is_skipped_not_fatal() {
        for (u, ok) in [
            ("http://127.0.0.1:8080", true),
            ("http://localhost:8888/", true),
            ("http://[::1]:8080", true),
            ("https://searx.example.org", true),
            ("http://192.168.1.10:8080", false),
            ("http://searxng.local:8080", false),
            ("http://127.0.0.1.evil.example/", false),
            ("ftp://127.0.0.1", false),
        ] {
            assert_eq!(searxng_url_ok(u), ok, "{u}");
        }
        let s: ResearchSettings = serde_json::from_str(r#"{"searxng_url": "http://192.168.1.10:8080"}"#).unwrap();
        assert_eq!(s.keyless().web.searxng_url, None);
        // The whole registry still builds, web included (DuckDuckGo stays on).
        let r = ChannelRegistry::keyless_with(&s.keyless(), None);
        assert!(r.ids().contains(&ChannelId::Web) && r.skipped().is_empty());
    }

    #[test]
    fn an_unread_page_carries_only_the_harness_reason() {
        let o = Outcome::Status { url: "https://example.org/x".into(), status: 403 };
        let p = page_of(o);
        assert_eq!(p.error.as_deref(), Some("HTTP 403"));
        assert!(p.text.is_empty() && p.links.is_empty());
    }

    /// Answers SearXNG's JSON for any URL, or refuses every connection.
    struct FakeHttp {
        up: bool,
        urls: std::sync::Mutex<Vec<String>>,
    }

    impl HttpGet for FakeHttp {
        fn get(&self, r: &marlowe_research::HttpRequest, _: &CancelToken) -> Result<marlowe_research::HttpResponse, String> {
            self.urls.lock().unwrap().push(r.url.clone());
            if !self.up {
                return Err(format!("connection refused: {}", r.url));
            }
            let body = r#"{"results":[{"url":"https://doc.rust-lang.org/cargo/","title":"The Cargo Book","content":"Cargo is the <b>Rust</b> package manager."}]}"#;
            Ok(marlowe_research::HttpResponse {
                status: 200,
                content_type: Some("application/json".into()),
                bytes: body.as_bytes().to_vec(),
                final_url: r.url.clone(),
                retry_after: None,
            })
        }
    }

    fn scratch_home(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("marlowe-keyless-search-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn web_search_goes_through_the_keyless_backend() {
        let home = scratch_home("up");
        let http = Arc::new(FakeHttp { up: true, urls: Default::default() });
        let s = KeylessSearch::with_http(&home, &ResearchSettings::default(), http.clone());
        let r = s.search("cargo book", &CancelToken::new()).unwrap();
        assert_eq!(r.results.len(), 1);
        assert_eq!(r.results[0].url, "https://doc.rust-lang.org/cargo/");
        assert_eq!(r.results[0].title, "The Cargo Book");
        assert!(r.results[0].snippet.contains("package manager"));
        let urls = http.urls.lock().unwrap().clone();
        assert!(urls.iter().all(|u| u.starts_with("http://127.0.0.1:8080/")), "{urls:?}");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn no_backend_says_what_to_do_and_never_names_openrouter() {
        let home = scratch_home("down");
        let http = Arc::new(FakeHttp { up: false, urls: Default::default() });
        let settings: ResearchSettings = serde_json::from_str(r#"{"duckduckgo": false}"#).unwrap();
        let s = KeylessSearch::with_http(&home, &settings, http);
        let e = s.search("anything", &CancelToken::new()).unwrap_err();
        assert!(e.contains("no web search backend answered"), "{e}");
        assert!(e.contains("run SearXNG at http://127.0.0.1:8080 or enable duckduckgo in research.json"), "{e}");
        assert!(!e.to_ascii_lowercase().contains("openrouter"), "{e}");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Web search never reaches a paid API: the registry's web channel has no hosted backend and
    /// the binary's wiring builds no OpenRouter search request.
    #[test]
    fn no_code_path_builds_an_openrouter_search() {
        let r = KeylessSearch::new(&scratch_home("reg"), &ResearchSettings::default());
        let web = r.registry().get(ChannelId::Web).expect("web channel");
        assert_eq!(web.spec().id, ChannelId::Web);
        let w = marlowe_research::WebChannel::new(ResearchSettings::default().keyless().web, None);
        assert!(w.backends().iter().all(|b| !matches!(b, marlowe_research::WebBackend::Hosted(_))), "{:?}", w.backends());
        for src in [include_str!("wire.rs"), include_str!("research_wire.rs")] {
            let search_tool = ["openrouter", ":web_search"].concat();
            let hits = src.matches(search_tool.as_str()).count();
            assert_eq!(hits, 0, "an OpenRouter server-tool search is built in the binary");
        }
    }

    #[test]
    fn an_old_research_json_with_hosted_search_still_parses_and_is_ignored() {
        let s: ResearchSettings = serde_json::from_str(r#"{"duckduckgo": true, "hosted_search": true}"#).unwrap();
        assert_eq!(s.hosted_search, Some(true));
        assert_eq!(s.keyless().web, WebConfig::default());
    }
}
