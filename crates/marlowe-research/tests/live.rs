//! Live checks against every keyless channel, one polite query each. All #[ignore]; run with
//!
//!   cargo test -p marlowe-research --test live -- --ignored --test-threads=1 --nocapture
//!
//! With `MARLOWE_RECORD=<dir>` every response body is also written there (`<label>-<n>.bin` plus
//! `manifest.txt`), which is how the fixtures under `tests/fixtures` were recorded.

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use marlowe_model::CancelToken;
use marlowe_research::{
    ChannelCtx, ChannelId, ChannelQuery, ChannelRegistry, HttpGet, HttpRequest, HttpResponse, KeylessConfig, Namespace,
    NetHttp, SourceRef, WebConfig, USER_AGENT,
};

struct Rec {
    inner: NetHttp,
    dir: Option<PathBuf>,
    label: &'static str,
    n: AtomicUsize,
}

impl Rec {
    fn new(label: &'static str) -> Self {
        let dir = std::env::var_os("MARLOWE_RECORD").map(PathBuf::from);
        if let Some(d) = &dir {
            std::fs::create_dir_all(d).unwrap();
        }
        Self { inner: NetHttp::new(), dir, label, n: AtomicUsize::new(0) }
    }
}

impl HttpGet for Rec {
    fn get(&self, req: &HttpRequest, cancel: &CancelToken) -> Result<HttpResponse, String> {
        assert_eq!(req.user_agent, USER_AGENT, "every request names the client");
        let t = Instant::now();
        let r = self.inner.get(req, cancel);
        let n = self.n.fetch_add(1, Ordering::SeqCst);
        let line = match &r {
            Ok(x) => format!("{}-{n} {} {}B {:?} {}", self.label, x.status, x.bytes.len(), t.elapsed(), req.url),
            Err(e) => format!("{}-{n} ERR {e} {:?} {}", self.label, t.elapsed(), req.url),
        };
        println!("  http {line}");
        if let (Some(d), Ok(x)) = (&self.dir, &r) {
            std::fs::write(d.join(format!("{}-{n}.bin", self.label)), &x.bytes).unwrap();
            let mut m = std::fs::OpenOptions::new().create(true).append(true).open(d.join("manifest.txt")).unwrap();
            writeln!(m, "{line}").unwrap();
        }
        r
    }
}

fn show(label: &str, refs: &[SourceRef]) {
    println!("{label}: {} results", refs.len());
    for r in refs.iter().take(5) {
        let keys: Vec<String> = r.effective_identity().keys().iter().map(|(n, v)| format!("{n:?}={v}")).collect();
        println!("  - {} | {} | {} | {}", r.title, r.date.as_deref().unwrap_or("-"), r.url, keys.join(" "));
    }
}

fn run(label: &'static str, id: ChannelId, q: ChannelQuery) -> Vec<SourceRef> {
    let reg = ChannelRegistry::keyless_with(&KeylessConfig::default(), None);
    let http = Rec::new(label);
    let cancel = CancelToken::new();
    let ctx = ChannelCtx { http: &http, cache: None, cancel: &cancel };
    let t = Instant::now();
    let out = reg.search(id, &q, &ctx).unwrap_or_else(|e| panic!("{label}: {e}"));
    println!("{label} took {:?}", t.elapsed());
    show(label, &out);
    out
}

fn has(refs: &[SourceRef], f: impl Fn(&SourceRef) -> bool) -> bool {
    refs.iter().any(f)
}

#[test]
#[ignore = "live network: export.arxiv.org"]
fn live_arxiv() {
    let r = run(
        "arxiv_search",
        ChannelId::Arxiv,
        ChannelQuery::new("ti:\"LoRA: Low-Rank Adaptation of Large Language Models\"", 3),
    );
    assert!(has(&r, |s| s.identity.as_ref().and_then(|i| i.arxiv_id.as_deref()) == Some("2106.09685")));
    let reg = ChannelRegistry::keyless(WebConfig::default(), None);
    let http = Rec::new("arxiv_lookup");
    let cancel = CancelToken::new();
    let ctx = ChannelCtx { http: &http, cache: None, cancel: &cancel };
    let hit = reg.lookup(Namespace::Arxiv, "1706.03762", &ctx).unwrap().unwrap();
    show("arxiv_lookup", std::slice::from_ref(&hit));
    assert!(hit.title.contains("Attention"));
}

#[test]
#[ignore = "live network: api.openalex.org"]
fn live_openalex() {
    let r = run(
        "openalex_search",
        ChannelId::OpenAlex,
        ChannelQuery::new("LoRA low-rank adaptation of large language models", 3),
    );
    assert!(!r.is_empty() && has(&r, |s| s.identity.as_ref().is_some_and(|i| i.openalex.is_some())));
}

#[test]
#[ignore = "live network: api.crossref.org"]
fn live_crossref() {
    let r = run("crossref_search", ChannelId::Crossref, ChannelQuery::new("deep learning LeCun Bengio Hinton", 3));
    assert!(!r.is_empty() && r.iter().all(|s| s.identity.as_ref().is_some_and(|i| i.doi.is_some())));
    let reg = ChannelRegistry::keyless(WebConfig::default(), None);
    let http = Rec::new("crossref_lookup");
    let cancel = CancelToken::new();
    let ctx = ChannelCtx { http: &http, cache: None, cancel: &cancel };
    let hit = reg.lookup(Namespace::Doi, "10.1038/nature14539", &ctx).unwrap().unwrap();
    show("crossref_lookup", std::slice::from_ref(&hit));
    assert_eq!(hit.title, "Deep learning");
}

#[test]
#[ignore = "live network: api.semanticscholar.org (keyless pool; may answer 429)"]
fn live_semantic_scholar() {
    let r = run(
        "s2_search",
        ChannelId::SemanticScholar,
        ChannelQuery::new("LoRA low-rank adaptation of large language models", 3),
    );
    assert!(!r.is_empty() && has(&r, |s| s.identity.as_ref().is_some_and(|i| i.s2.is_some())));
}

#[test]
#[ignore = "live network: en.wikipedia.org"]
fn live_wikipedia() {
    let r = run("wikipedia_search", ChannelId::Wikipedia, ChannelQuery::new("low-rank adaptation fine-tuning", 3));
    assert!(!r.is_empty());
}

#[test]
#[ignore = "live network: www.wikidata.org"]
fn live_wikidata() {
    let r = run("wikidata_search", ChannelId::Wikidata, ChannelQuery::new("Hugging Face", 3));
    assert!(has(&r, |s| s.identity.as_ref().and_then(|i| i.qid.as_deref()).is_some()));
    let r = run("wikidata_fallback", ChannelId::Wikidata, ChannelQuery::new("who founded Hugging Face", 3));
    assert!(!r.is_empty(), "the full-text fallback finds items for a question");
}

#[test]
#[ignore = "live network: api.github.com (unauthenticated)"]
fn live_github() {
    let r = run("github_repos", ChannelId::Github, ChannelQuery::new("lora fine-tuning language:python", 3));
    assert!(has(&r, |s| s.identity.as_ref().is_some_and(|i| i.repo.is_some())));
    let r = run("github_issues", ChannelId::Github, ChannelQuery::new("is:issue repo:pykeio/ort cuda", 3));
    assert!(!r.is_empty());
}

#[test]
#[ignore = "live network: hn.algolia.com"]
fn live_hacker_news() {
    let r = run("hn_search", ChannelId::HackerNews, ChannelQuery::new("LoRA fine-tuning", 3));
    assert!(has(&r, |s| s.url.starts_with("https://news.ycombinator.com/item?id=")));
}

#[test]
#[ignore = "live network: api.stackexchange.com (300 keyless requests/day)"]
fn live_stack_exchange() {
    let r = run("stackexchange_search", ChannelId::StackExchange, ChannelQuery::new("cargo workspace glob pattern", 3));
    assert!(!r.is_empty());
}

#[test]
#[ignore = "live network: eutils.ncbi.nlm.nih.gov"]
fn live_pubmed() {
    let r = run(
        "pubmed_search",
        ChannelId::Pubmed,
        ChannelQuery::new("GLP-1 receptor agonist cardiovascular outcomes trial", 3),
    );
    assert!(has(&r, |s| s.identity.as_ref().is_some_and(|i| i.pmid.is_some())));
}

#[test]
#[ignore = "live network: the default RSS/Atom feeds"]
fn live_rss_default_feeds() {
    let r = run("rss_search", ChannelId::Rss, ChannelQuery::new("rust release", 5));
    println!("rss matched {} items", r.len());
    let r = run("rss_one_feed", ChannelId::Rss, ChannelQuery::new("https://lwn.net/headlines/rss", 3));
    assert!(!r.is_empty());
}

#[test]
#[ignore = "live network: SearXNG on loopback if running, else lite.duckduckgo.com"]
fn live_web() {
    let r = run("web_search", ChannelId::Web, ChannelQuery::new("cargo workspace members glob", 5));
    assert!(!r.is_empty(), "a web backend answered");
    println!("backend: {:?}", r.first().and_then(|x| x.meta.get("backend")));
}

/// The same works through several channels, recorded once for `dedup_eval.rs` (ADR-036 §6).
#[test]
#[ignore = "live network: records the dedup evaluation corpus"]
fn live_record_dedup_corpus() {
    let qs: [(&'static str, ChannelId, &str, usize); 8] = [
        ("corpus_openalex_attention", ChannelId::OpenAlex, "Attention Is All You Need", 5),
        ("corpus_openalex_lora", ChannelId::OpenAlex, "LoRA: Low-Rank Adaptation of Large Language Models", 5),
        ("corpus_openalex_deeplearning", ChannelId::OpenAlex, "Deep learning LeCun Bengio Hinton", 3),
        ("corpus_crossref_attention", ChannelId::Crossref, "Attention Is All You Need Vaswani", 5),
        ("corpus_pubmed_deeplearning", ChannelId::Pubmed, "LeCun Bengio Hinton deep learning", 3),
        ("corpus_wikipedia_attention", ChannelId::Wikipedia, "Attention Is All You Need", 3),
        ("corpus_hn_attention", ChannelId::HackerNews, "Attention Is All You Need", 5),
        ("corpus_arxiv_attention", ChannelId::Arxiv, "ti:\"Attention Is All You Need\"", 3),
    ];
    for (label, id, q, n) in qs {
        run(label, id, ChannelQuery::new(q, n));
    }
}
