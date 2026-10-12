//! T20 (`web_fetch`: prompt digest, cross-host redirects, the 15-minute cache, the https upgrade)
//! and T21 (`web_search` domain filters), against recorded pages and a scripted digest model.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use marlowe_research::{HostGate, HttpRequest, HttpResponse};
use serde_json::json;

use super::*;
use crate::corpus::{ExtractMode, RawResponse};

const ARTICLE: &str = include_str!("../../marlowe-extract/tests/fixtures/article.html");

/// robots.txt: 404 everywhere (allow all).
struct NoRobots;

impl marlowe_research::HttpGet for NoRobots {
    fn get(&self, req: &HttpRequest, _: &CancelToken) -> Result<HttpResponse, String> {
        Ok(HttpResponse { status: 404, content_type: None, bytes: Vec::new(), final_url: req.url.clone(), retry_after: None })
    }
}

/// Pages by URL, counting fetches.
#[derive(Default)]
struct Pages {
    routes: HashMap<String, RawResponse>,
    hits: Mutex<HashMap<String, usize>>,
}

impl Pages {
    fn html(mut self, url: &str, body: &str) -> Self {
        let r = RawResponse { status: 200, content_type: Some("text/html; charset=utf-8".into()), bytes: body.as_bytes().to_vec(), ..Default::default() };
        self.routes.insert(url.into(), r);
        self
    }

    fn redirect(mut self, url: &str, to: &str) -> Self {
        self.routes.insert(url.into(), RawResponse { status: 302, location: Some(to.into()), ..Default::default() });
        self
    }

    fn hits(&self, url: &str) -> usize {
        self.hits.lock().unwrap().get(url).copied().unwrap_or(0)
    }
}

impl PageFetch for Pages {
    fn fetch(&self, url: &str, _: &CancelToken) -> Result<RawResponse, String> {
        *self.hits.lock().unwrap().entry(url.to_string()).or_default() += 1;
        self.routes.get(url).cloned().ok_or_else(|| format!("no route for {url}"))
    }
}

fn deps<'a>(net: &'a Pages, digest: Option<WebDigest>, dir: &std::path::Path) -> FetchDeps<'a> {
    let cfg = CorpusConfig {
        concurrency: 1,
        mode: ExtractMode::MainContent,
        gate: HostGate::new(),
        robots_http: Arc::new(NoRobots),
        renderer: None,
        same_host_redirects_only: true,
    };
    FetchDeps { cfg, net, digest, blob_dir: dir.to_path_buf() }
}

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("marlowe-web-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

#[test]
fn parity_t20_a_prompt_is_answered_from_the_page_and_the_page_stays_out_of_the_result() {
    let url = "https://t20-prompt.example/post";
    let net = Pages::default().html(url, ARTICLE);
    let asked = Arc::new(Mutex::new(Vec::<(String, String)>::new()));
    let seen = asked.clone();
    let digest = WebDigest::new(move |req: &DigestRequest<'_>, _: &CancelToken| {
        seen.lock().unwrap().push((req.prompt.to_string(), req.page.to_string()));
        Ok("Rank 8 matches rank 64 because the update is low rank. DIGEST-ANSWER".to_string())
    });
    let dir = scratch("prompt");
    let o = fetch_with(&json!({"url": url, "prompt": "Why does rank 8 match rank 64?"}), &deps(&net, Some(digest), &dir), &CancelToken::new());
    assert!(!o.is_error, "{}", o.content);
    assert!(o.content.contains("DIGEST-ANSWER"), "{}", o.content);
    assert!(!o.content.contains("Experimental setup"), "the page itself is not in the result: {}", o.content);
    let calls = asked.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "Why does rank 8 match rank 64?");
    assert!(calls[0].1.contains("Experimental setup"), "the digest model reads the page");
    // The page is saved where the result says, for `read`.
    let saved = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.path()).next().expect("page saved");
    assert!(o.content.contains(&saved.display().to_string()), "{}", o.content);
    assert!(std::fs::read_to_string(&saved).unwrap().contains("Experimental setup"));
    // raw: true returns the page even with a prompt.
    let o = fetch_with(&json!({"url": url, "prompt": "x", "raw": true}), &deps(&net, None, &dir), &CancelToken::new());
    assert!(o.content.contains("Experimental setup"), "{}", o.content);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn without_a_digest_model_the_page_comes_back_with_a_note() {
    let url = "https://t20-nodigest.example/post";
    let net = Pages::default().html(url, ARTICLE);
    let dir = scratch("nodigest");
    let o = fetch_with(&json!({"url": url, "prompt": "anything"}), &deps(&net, None, &dir), &CancelToken::new());
    assert!(!o.is_error);
    assert!(o.content.starts_with("[No model is available to answer the prompt"), "{}", o.content);
    assert!(o.content.contains("Experimental setup"));
    let failing = WebDigest::new(|_: &DigestRequest<'_>, _: &CancelToken| Err("timed out".to_string()));
    let o = fetch_with(&json!({"url": url, "prompt": "anything"}), &deps(&net, Some(failing), &dir), &CancelToken::new());
    assert!(o.content.starts_with("[The prompt could not be answered (timed out)"), "{}", o.content);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn parity_t20_a_redirect_to_another_host_is_returned_not_followed() {
    let net = Pages::default()
        .redirect("https://t20-redir.example/old", "https://t20-elsewhere.example/new")
        .html("https://t20-elsewhere.example/new", ARTICLE)
        .redirect("https://t20-redir.example/moved", "/here")
        .html("https://t20-redir.example/here", ARTICLE);
    let dir = scratch("redir");
    let o = fetch_with(&json!({"url": "https://t20-redir.example/old"}), &deps(&net, None, &dir), &CancelToken::new());
    assert!(!o.is_error, "{}", o.content);
    assert!(o.content.starts_with("REDIRECT:"), "{}", o.content);
    assert!(o.content.contains("https://t20-elsewhere.example/new"), "{}", o.content);
    assert_eq!(net.hits("https://t20-elsewhere.example/new"), 0, "the other host is never contacted");
    // A redirect within the host is followed.
    let o = fetch_with(&json!({"url": "https://t20-redir.example/moved"}), &deps(&net, None, &dir), &CancelToken::new());
    assert!(o.content.contains("Experimental setup"), "{}", o.content);
    assert_eq!(net.hits("https://t20-redir.example/here"), 1);
}

#[test]
fn parity_t20_a_second_fetch_inside_fifteen_minutes_is_a_cache_hit() {
    let url = "https://t20-cache.example/post";
    let net = Pages::default().html(url, ARTICLE);
    let dir = scratch("cache");
    let first = fetch_with(&json!({"url": url}), &deps(&net, None, &dir), &CancelToken::new());
    let second = fetch_with(&json!({"url": url}), &deps(&net, None, &dir), &CancelToken::new());
    assert_eq!(net.hits(url), 1, "the second read came from the cache");
    assert!(!first.summary.contains("cached"));
    assert!(second.summary.contains("cached"), "{}", second.summary);
    assert!(second.content.contains("from the cache"), "{}", &second.content[..120.min(second.content.len())]);
    // A prompt on a cached page still gets an answer.
    let digest = WebDigest::new(|_: &DigestRequest<'_>, _: &CancelToken| Ok("CACHED-ANSWER".to_string()));
    let o = fetch_with(&json!({"url": url, "prompt": "q"}), &deps(&net, Some(digest), &dir), &CancelToken::new());
    assert!(o.content.contains("CACHED-ANSWER") && o.content.contains("from the cache"), "{}", o.content);
    assert_eq!(net.hits(url), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn plain_http_is_upgraded_to_https() {
    let net = Pages::default().html("https://t20-upgrade.example/p", ARTICLE);
    let dir = scratch("upgrade");
    let o = fetch_with(&json!({"url": "http://t20-upgrade.example/p"}), &deps(&net, None, &dir), &CancelToken::new());
    assert!(!o.is_error, "{}", o.content);
    assert_eq!(net.hits("https://t20-upgrade.example/p"), 1);
    assert_eq!(net.hits("http://t20-upgrade.example/p"), 0, "nothing went out over plain http");
}

struct Recorded {
    queries: Mutex<Vec<String>>,
    calls: AtomicUsize,
}

impl SearchBackend for Recorded {
    fn search(&self, query: &str, _: &CancelToken) -> Result<SearchResponse, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.queries.lock().unwrap().push(query.to_string());
        let r = |t: &str, u: &str| SearchResult { title: t.into(), url: u.into(), snippet: String::new() };
        Ok(SearchResponse {
            answer: "Summary citing [blocked](https://spam.example/x).".into(),
            results: vec![
                r("Docs", "https://docs.rs/serde"),
                r("Sub", "https://api.docs.rs/x"),
                r("Spam", "https://spam.example/x"),
                r("Spam www", "https://www.spam.example/y"),
                r("Other", "https://other.org/z"),
            ],
        })
    }
}

fn recorded() -> Recorded {
    Recorded { queries: Mutex::new(Vec::new()), calls: AtomicUsize::new(0) }
}

#[test]
fn parity_t21_a_blocked_domain_is_never_returned_and_an_allow_list_returns_only_its_domains() {
    let b = recorded();
    let o = web_search(&b, &json!({"query": "serde", "blocked_domains": ["spam.example"]}), &CancelToken::new());
    assert!(!o.is_error);
    assert!(!o.content.contains("spam.example"), "{}", o.content);
    assert!(o.content.contains("https://docs.rs/serde") && o.content.contains("https://other.org/z"));
    assert!(o.content.contains("2 result(s) outside the domain filter were removed"), "{}", o.content);
    assert!(b.queries.lock().unwrap()[0].contains("-site:spam.example"), "the backend is told too");

    let o = web_search(&b, &json!({"query": "serde", "allowed_domains": ["https://www.docs.rs/"]}), &CancelToken::new());
    let urls: Vec<&str> = o.content.lines().filter_map(|l| l.split(" — ").nth(1)).collect();
    assert_eq!(urls, ["https://docs.rs/serde", "https://api.docs.rs/x"], "{}", o.content);
    assert!(b.queries.lock().unwrap()[1].contains("site:docs.rs"));

    // With no filter the summary and every source come back as before.
    let o = web_search(&b, &json!({"query": "serde"}), &CancelToken::new());
    assert!(o.content.starts_with("Summary citing"), "{}", o.content);
    assert_eq!(o.summary, "5 sources");
}

#[test]
fn domain_filters_match_subdomains_but_not_lookalikes() {
    let f = DomainFilter::from_args(&json!({"blocked_domains": ["example.com"]}));
    assert!(!f.admits("https://example.com/a"));
    assert!(!f.admits("https://a.b.example.com/a"));
    assert!(!f.admits("https://user@EXAMPLE.com:8443/a"));
    assert!(f.admits("https://notexample.com/a"));
    assert!(f.admits("https://example.com.evil.org/a"));
    let f = DomainFilter::from_args(&json!({"allowed_domains": "docs.rs, *.python.org"}));
    assert_eq!(f.allowed, ["docs.rs", "python.org"]);
    assert!(f.admits("https://docs.python.org/3/"));
    assert!(!f.admits("not a url"));
}
