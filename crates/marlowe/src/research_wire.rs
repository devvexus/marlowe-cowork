//! Deep research v2's I/O for the daemon: the keyless channel registry, the corpus fetcher, the
//! research cache, and the user's settings in `<home>\research.json`.
//!
//! ```json
//! { "searxng_url": "http://127.0.0.1:8080", "duckduckgo": true, "hosted_search": true,
//!   "auto_approve_under_tokens": null, "fetch_concurrency": 16,
//!   "feeds": [], "wikipedia_lang": "en", "stackexchange_site": "stackoverflow" }
//! ```
//!
//! Every field is optional. A file that does not parse is reported on stderr (the daemon log) and
//! the defaults apply.

use std::path::Path;
use std::sync::Arc;

use marlowe_brains::CredentialStore;
use marlowe_crew::playbook::research_v2::{ChannelIo, FetchedPage, PageFetcher, ResearchIo, ResearchV2Config};
use marlowe_exec::corpus::{Corpus, Outcome};
use marlowe_model::CancelToken;
use marlowe_research::{
    ChannelId, ChannelRegistry, HostedSearch, KeylessConfig, NetHttp, ResearchCache, SourceRef, WebConfig,
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
    /// OpenRouter's server-side search as the last web backend, when a key works (default on).
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
                Some(u) => Some(u.to_string()),
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

/// OpenRouter's server-side web search as the last web backend. The key stays in the binary.
struct OpenRouterSearch {
    credentials: Arc<dyn CredentialStore>,
}

impl HostedSearch for OpenRouterSearch {
    fn name(&self) -> &str {
        "openrouter"
    }

    fn search(&self, query: &str, limit: usize, cancel: &CancelToken) -> Result<Vec<SourceRef>, String> {
        let r = crate::wire::web_search(self.credentials.as_ref(), query, cancel)?;
        Ok(r.results
            .into_iter()
            .take(limit.max(1))
            .map(|s| {
                let mut src = SourceRef::new(ChannelId::Web, &s.url, &s.title);
                src.snippet = s.snippet;
                src
            })
            .collect())
    }
}

/// The product [`ResearchIo`] for `home`.
pub fn research_io(home: &Path, settings: &ResearchSettings, credentials: Arc<dyn CredentialStore>) -> Arc<dyn ResearchIo> {
    let hosted: Option<Arc<dyn HostedSearch>> =
        settings.hosted_search.unwrap_or(true).then(|| Arc::new(OpenRouterSearch { credentials }) as Arc<dyn HostedSearch>);
    Arc::new(ChannelIo {
        registry: Arc::new(ChannelRegistry::keyless_with(&settings.keyless(), hosted)),
        http: Arc::new(NetHttp::new()),
        cache: Some(Arc::new(ResearchCache::at_home(home))),
        pages: Arc::new(CorpusPages(Corpus::product())),
    })
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

    #[test]
    fn an_unread_page_carries_only_the_harness_reason() {
        let o = Outcome::Status { url: "https://example.org/x".into(), status: 403 };
        let p = page_of(o);
        assert_eq!(p.error.as_deref(), Some("HTTP 403"));
        assert!(p.text.is_empty() && p.links.is_empty());
    }
}
