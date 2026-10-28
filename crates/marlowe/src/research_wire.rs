//! Research I/O for the daemon: the keyless channel registry, the corpus fetcher, the research
//! cache, and the user's settings in `<home>\research.json`.
//!
//! ```json
//! { "searxng_url": "http://127.0.0.1:8080", "free_search": true, "duckduckgo": true,
//!   "feeds": [], "wikipedia_lang": "en", "stackexchange_site": "stackoverflow" }
//! ```
//!
//! Every field is optional. A file that does not parse is reported on stderr (the daemon log) and
//! the defaults apply.

use std::path::Path;
use std::sync::Arc;

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
    /// The free agent search APIs (Parallel, Exa, Marginalia) after SearXNG (default on).
    pub free_search: Option<bool>,
    /// DuckDuckGo HTML lite after them (default on).
    pub duckduckgo: Option<bool>,
    /// Ignored: accepted so an old research.json still parses. Web search never uses a paid API.
    pub hosted_search: Option<bool>,
    /// Ignored: accepted so an old research.json still parses (the retired deep research's settings).
    pub auto_approve_under_tokens: Option<u64>,
    /// Ignored, like `auto_approve_under_tokens`.
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
            free_search: self.free_search.unwrap_or(d.free_search),
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

/// The `web_search` tool's backend: the same keyless web channel a project uses (SearXNG
/// when it answers, else the free agent search APIs in turn, Marginalia, then DuckDuckGo HTML
/// lite), under `<home>\research.json`. No key, no paid API.
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

    #[cfg(test)]
    pub fn registry(&self) -> &ChannelRegistry {
        &self.registry
    }

    fn no_backend(&self, detail: &str) -> String {
        let url = self.searxng.as_deref().unwrap_or(marlowe_research::DEFAULT_SEARXNG);
        format!(
            "no web search backend answered ({detail}): the free search APIs rest a few minutes after a refusal and come back on their own; or run SearXNG at {url}, or turn free_search or duckduckgo on in research.json"
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

// ---- projects ------------------------------------------------------------------------------------

/// Channel search for a project: the keyless registry behind `marlowe_project::research::Web`.
pub struct ProjectWeb {
    registry: Arc<ChannelRegistry>,
    http: Arc<dyn HttpGet>,
    cache: Option<Arc<ResearchCache>>,
}

impl ProjectWeb {
    /// Over `registry`, searching through `http` and caching under `cache`.
    pub fn new(registry: Arc<ChannelRegistry>, http: Arc<dyn HttpGet>, cache: Option<Arc<ResearchCache>>) -> Self {
        Self { registry, http, cache }
    }
}

impl marlowe_project::research::Web for ProjectWeb {
    fn channels(&self) -> Vec<ChannelId> {
        self.registry.ids().into_iter().filter(|c| c.is_network()).collect()
    }

    fn search(&self, channel: ChannelId, query: &ChannelQuery, cancel: &CancelToken) -> Result<Vec<marlowe_research::SourceRef>, ChannelError> {
        let ctx = ChannelCtx { http: self.http.as_ref(), cache: self.cache.as_deref(), cancel };
        self.registry.search(channel, query, &ctx)
    }

    fn unavailable(&self) -> Vec<(ChannelId, String)> {
        self.registry.skipped().to_vec()
    }
}

/// The corpus fetcher as a project's page reader. The corpus keeps what it always did (https
/// only, robots.txt and per-host spacing on every hop, the SSRF guard at connect time, Edge for
/// thin pages); the project's network grant is asked first, for the address and for every
/// redirect hop, through [`Admitting`].
#[allow(dead_code)] // wired by the project tool
pub struct ProjectPages(pub Corpus);

/// A network that asks the grant before each request, and remembers if it said no.
struct Admitting<'a> {
    inner: &'a dyn marlowe_exec::corpus::PageFetch,
    admit: marlowe_project::research::Admit<'a>,
    refused: &'a std::sync::Mutex<Option<String>>,
}

impl marlowe_exec::corpus::PageFetch for Admitting<'_> {
    fn fetch(&self, url: &str, cancel: &CancelToken) -> Result<marlowe_exec::corpus::RawResponse, String> {
        if let Err(why) = (self.admit)(url) {
            *self.refused.lock().unwrap_or_else(|e| e.into_inner()) = Some(why);
            return Err("not requested".into());
        }
        self.inner.fetch(url, cancel)
    }
}

impl marlowe_project::research::PageFetch for ProjectPages {
    fn read(&self, url: &str, admit: marlowe_project::research::Admit<'_>, cancel: &CancelToken) -> marlowe_project::research::Fetched {
        use marlowe_project::research::{Failure, Fetched};
        let refused = std::sync::Mutex::new(None);
        let net = Admitting { inner: self.0.net.as_ref(), admit, refused: &refused };
        let selector = marlowe_extract::main_content::Selector::with_defaults();
        let out = marlowe_exec::corpus::read_one(url, &self.0.config, &net, &selector, cancel);
        if let Some(why) = refused.into_inner().unwrap_or_else(|e| e.into_inner()) {
            return Fetched::failed(url, Failure::Blocked, why);
        }
        let why = out.why_not().unwrap_or_else(|| "not read".into());
        match out {
            Outcome::Read(p) => Fetched {
                url: p.url.clone(),
                final_url: p.final_url.clone(),
                title: p.document.title.clone(),
                text: p.markdown.clone(),
                links: p.links().to_vec(),
                meta: p.document.meta.clone(),
                references: Vec::new(),
                failure: None,
            },
            Outcome::Refused { reason: marlowe_research::Refusal::NotHttp(_), .. } => Fetched::failed(url, Failure::Blocked, why),
            Outcome::Refused { reason: marlowe_research::Refusal::Cancelled, .. } => Fetched::failed(url, Failure::Unreachable, why),
            Outcome::Refused { .. } => Fetched::failed(url, Failure::Robots, why),
            Outcome::Status { .. } => Fetched::failed(url, Failure::Http, why),
            Outcome::Unreachable { .. } => Fetched::failed(url, Failure::Unreachable, why),
            Outcome::Unreadable { .. } => Fetched::failed(url, Failure::Unreadable, why),
            Outcome::Redirect { .. } => Fetched::failed(url, Failure::Blocked, why),
        }
    }
}

/// The research dependencies of a project run in `home`: keyless channels with the research
/// cache, the corpus fetcher under the run-scoped grant, projects under `<home>\projects`,
/// deliverables under `<home>\output`.
#[allow(dead_code)] // wired by the project tool
pub fn project_research(home: &Path, settings: &ResearchSettings, memory: Arc<dyn marlowe_project::research::MemorySink>) -> Arc<marlowe_project::research::ResearchDeps> {
    let web = ProjectWeb::new(
        Arc::new(ChannelRegistry::keyless_with(&settings.keyless(), None)),
        Arc::new(NetHttp::new()),
        Some(Arc::new(ResearchCache::at_home(home))),
    );
    Arc::new(marlowe_project::research::ResearchDeps::new(
        home.join("projects"),
        home.join("output"),
        Arc::new(web),
        Arc::new(ProjectPages(Corpus::product())),
        Arc::new(marlowe_project::research::SystemResolver),
        memory,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_default_and_parse() {
        let s: ResearchSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(s.keyless().web, WebConfig::default());
        let s: ResearchSettings = serde_json::from_str(
            r#"{"searxng_url": "", "duckduckgo": false, "auto_approve_under_tokens": 50000, "fetch_concurrency": 999}"#,
        )
        .unwrap();
        assert_eq!(s.keyless().web, WebConfig { searxng_url: None, free_search: true, duckduckgo: false });
        assert_eq!((s.auto_approve_under_tokens, s.fetch_concurrency), (Some(50_000), Some(999)), "old settings still parse");
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
        assert!(e.contains("come back on their own; or run SearXNG at http://127.0.0.1:8080"), "{e}");
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

    // ---- projects -------------------------------------------------------------------------

    use marlowe_exec::corpus::{CorpusConfig, ExtractMode, PageFetch as CorpusNet, RawResponse};
    use marlowe_project::research::{Failure, PageFetch as ProjectFetch};
    use std::collections::HashMap;
    use std::sync::Mutex;

    const ARTICLE: &str = include_str!("../../marlowe-extract/tests/fixtures/article.html");

    /// Pages by URL; records what was requested.
    struct Net(HashMap<&'static str, RawResponse>, Mutex<Vec<String>>);

    impl CorpusNet for Net {
        fn fetch(&self, url: &str, _: &CancelToken) -> Result<RawResponse, String> {
            self.1.lock().unwrap().push(url.to_string());
            self.0.get(url).cloned().ok_or_else(|| format!("no route for {url}"))
        }
    }

    struct Robots(&'static str);

    impl HttpGet for Robots {
        fn get(&self, r: &marlowe_research::HttpRequest, _: &CancelToken) -> Result<marlowe_research::HttpResponse, String> {
            Ok(marlowe_research::HttpResponse { status: 200, content_type: Some("text/plain".into()), bytes: self.0.as_bytes().to_vec(), final_url: r.url.clone(), retry_after: None })
        }
    }

    fn pages(routes: Vec<(&'static str, RawResponse)>, robots: &'static str) -> (ProjectPages, Arc<Net>) {
        let net = Arc::new(Net(routes.into_iter().collect(), Mutex::new(Vec::new())));
        let cfg = CorpusConfig { concurrency: 1, mode: ExtractMode::MainContent, gate: marlowe_research::HostGate::new(), robots_http: Arc::new(Robots(robots)), renderer: None, same_host_redirects_only: false };
        (ProjectPages(Corpus { config: cfg, net: net.clone() }), net)
    }

    fn ok(body: &str) -> RawResponse {
        RawResponse { status: 200, content_type: Some("text/html".into()), bytes: body.as_bytes().to_vec(), ..Default::default() }
    }

    fn redirect(to: &str) -> RawResponse {
        RawResponse { status: 301, location: Some(to.into()), ..Default::default() }
    }

    #[test]
    fn a_project_reads_a_page_through_the_corpus_and_the_grant_is_asked_for_the_address() {
        let (p, net) = pages(vec![("https://a.example/post", ok(ARTICLE))], "");
        let asked = Mutex::new(Vec::new());
        let admit = |u: &str| -> Result<(), String> {
            asked.lock().unwrap().push(u.to_string());
            Ok(())
        };
        let f = p.read("https://a.example/post", &admit, &CancelToken::new());
        assert!(f.failure.is_none() && f.text.chars().count() > 200, "{:?}", f.failure);
        assert_eq!(*asked.lock().unwrap(), ["https://a.example/post"]);
        assert_eq!(net.1.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_redirect_hop_the_grant_refuses_is_never_requested() {
        let (p, net) = pages(vec![("https://a.example/go", redirect("https://inner.example/x")), ("https://inner.example/x", ok(ARTICLE))], "");
        let admit = |u: &str| -> Result<(), String> { if u.contains("inner") { Err("the host name points at a private address".into()) } else { Ok(()) } };
        let f = p.read("https://a.example/go", &admit, &CancelToken::new());
        assert_eq!(f.failure, Some((Failure::Blocked, "the host name points at a private address".into())));
        assert_eq!(*net.1.lock().unwrap(), ["https://a.example/go"], "the second hop never went out");
    }

    #[test]
    fn robots_and_errors_keep_their_own_class() {
        let (p, _) = pages(vec![("https://a.example/private/x", ok(ARTICLE))], "User-agent: *
Disallow: /private/");
        let go = |_: &str| -> Result<(), String> { Ok(()) };
        let robots = p.read("https://a.example/private/x", &go, &CancelToken::new());
        assert!(matches!(robots.failure, Some((Failure::Robots, _))), "{:?}", robots.failure);
        let (p, _) = pages(vec![("https://a.example/gone", RawResponse { status: 404, content_type: Some("text/html".into()), ..Default::default() })], "");
        assert!(matches!(p.read("https://a.example/gone", &go, &CancelToken::new()).failure, Some((Failure::Http, _))));
        let (p, _) = pages(vec![], "");
        assert!(matches!(p.read("https://a.example/nothing", &go, &CancelToken::new()).failure, Some((Failure::Unreachable, _))));
        assert!(matches!(p.read("http://a.example/plain", &go, &CancelToken::new()).failure, Some((Failure::Blocked, _))), "plain http is not read");
    }
}
