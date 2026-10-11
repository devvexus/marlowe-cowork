//! Every channel against responses recorded live on 2026-10-09 (tests/live.rs with
//! MARLOWE_RECORD). The PubMed efetch fixture has its `<ReferenceList>` blocks removed for size;
//! `searxng_synthetic.json` follows SearXNG's documented JSON shape (no instance was running to
//! record from). Everything goes through `ChannelRegistry::search`, so pacing and parsing are
//! exercised together.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use marlowe_model::CancelToken;
use marlowe_research::channels::{RssChannel, StackExchangeChannel, WebChannel, WebConfig};
use marlowe_research::{
    Channel, ChannelCtx, ChannelError, ChannelId, ChannelQuery, ChannelRegistry, HttpGet, HttpRequest, HttpResponse,
    KeylessConfig, Namespace, ResearchCache, SourceRef, USER_AGENT,
};

fn fixture(name: &str) -> Vec<u8> {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures").join(name);
    std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

/// Answers by URL substring (first match wins); records every URL asked.
struct Replay {
    routes: Vec<(&'static str, u16, Vec<u8>)>,
    seen: Mutex<Vec<String>>,
}

impl Replay {
    fn new(routes: &[(&'static str, u16, &str)]) -> Self {
        let routes = routes.iter().map(|(k, s, f)| (*k, *s, fixture(f))).collect();
        Self { routes, seen: Mutex::new(vec![]) }
    }

    fn seen(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }
}

impl HttpGet for Replay {
    fn get(&self, req: &HttpRequest, _: &CancelToken) -> Result<HttpResponse, String> {
        assert_eq!(req.user_agent, USER_AGENT);
        self.seen.lock().unwrap().push(req.url.clone());
        let (_, status, body) = self
            .routes
            .iter()
            .find(|r| req.url.contains(r.0))
            .ok_or_else(|| format!("connection refused: {}", req.url))?;
        Ok(HttpResponse {
            status: *status,
            content_type: None,
            bytes: body.clone(),
            final_url: req.url.clone(),
            retry_after: None,
        })
    }
}

fn search(id: ChannelId, q: &str, n: usize, http: &Replay) -> Result<Vec<SourceRef>, ChannelError> {
    // Recorded 429s are not waited out (the product waits up to a minute and asks again).
    let reg = ChannelRegistry::keyless_with(&KeylessConfig::default(), None).with_max_retry_wait(Duration::ZERO);
    let cancel = CancelToken::new();
    reg.search(id, &ChannelQuery::new(q, n), &ChannelCtx { http, cache: None, cancel: &cancel })
}

fn ids(r: &SourceRef) -> marlowe_research::Identity {
    r.identity.clone().unwrap_or_default()
}

#[test]
fn arxiv_search_and_lookup() {
    let http = Replay::new(&[("export.arxiv.org/api/query?search_query", 200, "arxiv_search.xml")]);
    let r = search(ChannelId::Arxiv, "ti:\"LoRA: Low-Rank Adaptation of Large Language Models\"", 3, &http).unwrap();
    let lora = r.iter().find(|s| ids(s).arxiv_id.as_deref() == Some("2106.09685")).expect("LoRA");
    assert_eq!(lora.title, "LoRA: Low-Rank Adaptation of Large Language Models");
    assert_eq!(lora.url, "https://arxiv.org/abs/2106.09685");
    assert!(lora.authors.iter().any(|a| a.contains("Hu")), "{:?}", lora.authors);
    assert!(lora.meta.get("pdf").is_some_and(|p| p.starts_with("https://arxiv.org/pdf/2106.09685")));
    assert!(lora.snippet.len() > 200);

    let http = Replay::new(&[("id_list=1706.03762", 200, "arxiv_lookup.xml")]);
    let reg = ChannelRegistry::keyless(WebConfig::default(), None);
    let cancel = CancelToken::new();
    let ctx = ChannelCtx { http: &http, cache: None, cancel: &cancel };
    let hit = reg.lookup(Namespace::Arxiv, "arXiv:1706.03762v7", &ctx).unwrap().unwrap();
    assert_eq!(hit.title, "Attention Is All You Need");
    assert_eq!(ids(&hit).arxiv_id.as_deref(), Some("1706.03762"));
    assert!(http.seen()[0].contains("id_list=1706.03762v7"), "the lookup asks for the version given");
}

#[test]
fn openalex_rebuilds_abstracts_and_finds_preprint_ids() {
    let http = Replay::new(&[("api.openalex.org/works?search=", 200, "openalex_search.json")]);
    let r = search(ChannelId::OpenAlex, "LoRA low-rank adaptation of large language models", 3, &http).unwrap();
    assert_eq!(r.len(), 3);
    let qa = &r[0];
    assert!(qa.title.starts_with("QA-LoRA"));
    assert_eq!(ids(qa).openalex.as_deref(), Some("W4387156572"));
    assert_eq!(ids(qa).arxiv_id.as_deref(), Some("2309.14717"), "the arXiv landing page gives the preprint id");
    assert!(qa.snippet.split_whitespace().count() > 30, "abstract rebuilt from the inverted index");
    assert!(qa.meta.contains_key("cited_by"));
    assert_eq!(ids(&r[1]).doi.as_deref(), Some("10.5281/zenodo.20209068"));
}

#[test]
fn crossref_search_and_doi_lookup() {
    let http = Replay::new(&[("api.crossref.org/works?query=", 200, "crossref_search.json")]);
    let r = search(ChannelId::Crossref, "deep learning LeCun Bengio Hinton", 3, &http).unwrap();
    assert_eq!(r.len(), 3);
    assert!(r.iter().all(|s| ids(s).doi.is_some()));
    let ai = r.iter().find(|s| ids(s).doi.as_deref() == Some("10.1145/3448250")).unwrap();
    assert_eq!((ai.title.as_str(), ai.date.as_deref()), ("Deep learning for AI", Some("2021-06-21")));

    let http = Replay::new(&[("api.crossref.org/works/10.1038%2Fnature14539", 200, "crossref_lookup.json")]);
    let reg = ChannelRegistry::keyless(WebConfig::default(), None);
    let cancel = CancelToken::new();
    let ctx = ChannelCtx { http: &http, cache: None, cancel: &cancel };
    let hit = reg.lookup(Namespace::Doi, "https://doi.org/10.1038/NATURE14539", &ctx).unwrap().unwrap();
    assert_eq!((hit.title.as_str(), hit.date.as_deref()), ("Deep learning", Some("2015-05-27")));
    assert_eq!(hit.authors.first().map(String::as_str), Some("LeCun, Yann"));
    assert_eq!(hit.meta.get("venue").map(String::as_str), Some("Nature"));
}

#[test]
fn semantic_scholar_rate_limits_hold_the_channel() {
    let http = Replay::new(&[("api.semanticscholar.org", 429, "s2_429.json")]);
    // The registry waits out holds up to a minute and asks again; not in a unit test.
    let reg = ChannelRegistry::keyless(WebConfig::default(), None).with_max_retry_wait(std::time::Duration::ZERO);
    let cancel = CancelToken::new();
    let ctx = ChannelCtx { http: &http, cache: None, cancel: &cancel };
    let e = reg.search(ChannelId::SemanticScholar, &ChannelQuery::new("lora", 3), &ctx).unwrap_err();
    assert!(matches!(e, ChannelError::RateLimited { channel: ChannelId::SemanticScholar, .. }), "{e}");
    let wait = reg.limiter(ChannelId::SemanticScholar).unwrap().reserve();
    assert!(wait >= std::time::Duration::from_secs(25), "held for the default back-off: {wait:?}");
}

#[test]
fn wikipedia_pages_carry_their_wikidata_item_as_a_claim() {
    let http = Replay::new(&[("en.wikipedia.org/w/api.php", 200, "wikipedia_search.json")]);
    let r = search(ChannelId::Wikipedia, "low-rank adaptation fine-tuning", 3, &http).unwrap();
    let titles: Vec<&str> = r.iter().map(|s| s.title.as_str()).collect();
    assert_eq!(
        titles,
        ["Fine-tuning (deep learning)", "LoRA (machine learning)", "Post-training of large language models"]
    );
    assert_eq!(ids(&r[1]).qid.as_deref(), Some("Q130145067"));
    assert!(!r[1].snippet.is_empty());
}

#[test]
fn wikidata_names_and_questions() {
    let http = Replay::new(&[("search=Hugging%20Face", 200, "wikidata_search.json")]);
    let r = search(ChannelId::Wikidata, "Hugging Face", 3, &http).unwrap();
    assert_eq!(ids(&r[0]).qid.as_deref(), Some("Q108943604"));
    assert_eq!(r[0].url, "https://www.wikidata.org/wiki/Q108943604");

    let http = Replay::new(&[
        ("search=who%20founded", 200, "wikidata_question_miss.json"),
        ("search=Hugging%20Face", 200, "wikidata_search.json"),
    ]);
    let r = search(ChannelId::Wikidata, "who founded Hugging Face?", 3, &http).unwrap();
    assert_eq!(ids(&r[0]).qid.as_deref(), Some("Q108943604"));
    assert_eq!(http.seen().len(), 2, "the name inside the question was tried second");
}

#[test]
fn github_repositories_and_issues() {
    let http = Replay::new(&[("api.github.com/search/repositories", 200, "github_repos.json")]);
    let r = search(ChannelId::Github, "lora fine-tuning language:python", 3, &http).unwrap();
    let peft = r.iter().find(|s| s.title == "huggingface/peft").unwrap();
    assert_eq!(ids(peft).repo.unwrap().repo, "huggingface/peft");
    assert!(peft.meta.get("stars").is_some_and(|s| s.parse::<u64>().unwrap() > 1000));

    let http = Replay::new(&[("api.github.com/search/issues", 200, "github_issues.json")]);
    let r = search(ChannelId::Github, "is:issue repo:pykeio/ort cuda", 3, &http).unwrap();
    assert_eq!(r[0].url, "https://github.com/pykeio/ort/issues/619");
    assert_eq!(r[0].meta.get("repo").map(String::as_str), Some("pykeio/ort"));
    assert!(ids(&r[0]).repo.is_none(), "an issue is not the repository");

    let body = br#"{"message":"API rate limit exceeded for 203.0.113.9."}"#.to_vec();
    let limited = Replay { routes: vec![("api.github.com", 403, body)], seen: Mutex::new(vec![]) };
    assert!(matches!(search(ChannelId::Github, "x", 1, &limited), Err(ChannelError::RateLimited { .. })));
}

#[test]
fn hacker_news_threads_point_at_what_they_discuss() {
    let http = Replay::new(&[("hn.algolia.com/api/v1/search", 200, "hn_search.json")]);
    let r = search(ChannelId::HackerNews, "LoRA fine-tuning", 3, &http).unwrap();
    assert_eq!(r[0].url, "https://news.ycombinator.com/item?id=37871203");
    assert!(r[0].meta.contains_key("points"));
    assert!(r.iter().any(|s| s.meta.get("linked_url").is_some_and(|u| u.starts_with("http"))));
}

#[test]
fn stack_exchange_falls_back_to_similar_titles() {
    let http = Replay::new(&[
        ("/search/advanced", 200, "stackexchange_empty.json"),
        ("/similar?", 200, "stackexchange_similar.json"),
    ]);
    let r = search(ChannelId::StackExchange, "cargo workspace glob pattern", 3, &http).unwrap();
    assert_eq!(r[0].title, "Where to put the .cargo/config.toml file in a cargo workspace?");
    assert_eq!(r[0].date.as_deref(), Some("2023-06-05"));
    assert_eq!(r[0].meta.get("site").map(String::as_str), Some("stackoverflow"));
    assert!(!r[0].snippet.contains('<'), "bodies are plain text");
    assert_eq!(http.seen().len(), 2);
    let _ = StackExchangeChannel::new("stackoverflow").spec();
}

#[test]
fn pubmed_esearch_then_efetch_in_rank_order() {
    let http = Replay::new(&[("esearch.fcgi", 200, "pubmed_esearch.json"), ("efetch.fcgi", 200, "pubmed_efetch.xml")]);
    let r = search(ChannelId::Pubmed, "GLP-1 receptor agonist cardiovascular outcomes trial", 3, &http).unwrap();
    let pmids: Vec<String> = r.iter().filter_map(|s| ids(s).pmid).collect();
    assert_eq!(pmids, ["39114288", "39145614", "39210781"]);
    assert_eq!(ids(&r[0]).doi.as_deref(), Some("10.3389/fendo.2024.1431292"));
    assert_eq!(ids(&r[0]).pmcid.as_deref(), Some("PMC11304055"));
    assert!(r[2].title.starts_with("Cardiovascular, Kidney, and Safety Outcomes"));
    assert!(r.iter().all(|s| s.snippet.len() > 100 && !s.authors.is_empty()));
    assert!(http.seen()[1].contains("id=39114288,39145614,39210781"));
}

#[test]
fn rss_reads_only_named_feeds_and_searches_configured_feeds_under_robots() {
    let http = Replay::new(&[
        ("lwn.net/robots.txt", 200, "lwn_robots.txt"),
        ("lwn.net/headlines/rss", 200, "lwn_headlines.rss"),
    ]);
    // A URL in the query text (a model's follow-up question can carry one) is never fetched.
    let refused = search(ChannelId::Rss, "https://attacker.example/feed?leak=secret", 3, &http).unwrap_err();
    assert!(refused.to_string().contains("not read as a feed"), "{refused}");
    assert!(http.seen().is_empty(), "nothing was requested");
    // A feed the user names is.
    let reg = ChannelRegistry::keyless_with(&KeylessConfig::default(), None);
    let cancel = CancelToken::new();
    let named = ChannelQuery { feed_url: Some("https://lwn.net/headlines/rss".into()), ..ChannelQuery::new("", 3) };
    let r = reg.search(ChannelId::Rss, &named, &ChannelCtx { http: &http, cache: None, cancel: &cancel }).unwrap();
    assert_eq!(r[0].title, "[$] The state of systemd: 2026 edition");
    assert_eq!(r[0].date.as_deref(), Some("2026-10-09"));
    assert_eq!(http.seen()[0], "https://lwn.net/robots.txt", "robots.txt is read first");

    let feeds = RssChannel::new(vec!["https://lwn.net/headlines/rss".into()]);
    let mut reg = ChannelRegistry::empty();
    reg.register(Arc::new(feeds)).unwrap();
    let home = std::env::temp_dir().join(format!("marlowe-research-rss-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    let cache = ResearchCache::at_home(&home);
    let cancel = CancelToken::new();
    let ctx = ChannelCtx { http: &http, cache: Some(&cache), cancel: &cancel };
    let hits = reg.search(ChannelId::Rss, &ChannelQuery::new("systemd", 5), &ctx).unwrap();
    assert!(hits[0].title.contains("systemd"));
    let before = http.seen().len();
    let other = reg.search(ChannelId::Rss, &ChannelQuery::new("encrypt certificates", 5), &ctx).unwrap();
    assert!(other.iter().any(|s| s.title.contains("Let's Encrypt")));
    assert_eq!(http.seen().len(), before, "the feed came from the per-feed cache");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn web_uses_searxng_then_names_why_duckduckgo_could_not_answer() {
    let http = Replay::new(&[("127.0.0.1:8080/search", 200, "searxng_synthetic.json")]);
    let r = search(ChannelId::Web, "cargo workspace members glob", 5, &http).unwrap();
    assert_eq!(r[0].url, "https://doc.rust-lang.org/cargo/reference/workspaces.html");
    assert_eq!(r[0].meta.get("backend").map(String::as_str), Some("searxng"));
    assert!(http.seen()[0].contains("format=json"));

    // SearXNG down, DuckDuckGo challenges: an error naming both, then DuckDuckGo rests, and the
    // rest survives a new channel through the cache.
    let http = Replay::new(&[
        ("lite.duckduckgo.com/robots.txt", 200, "duckduckgo_robots.txt"),
        ("lite.duckduckgo.com/lite/", 202, "duckduckgo_challenge.html"),
    ]);
    let home = std::env::temp_dir().join(format!("marlowe-research-web-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    let cache = ResearchCache::at_home(&home);
    let cancel = CancelToken::new();
    let ctx = ChannelCtx { http: &http, cache: Some(&cache), cancel: &cancel };
    let web = WebChannel::new(WebConfig::default(), None);
    let q = ChannelQuery::new("x", 3);
    let e = web.search(&q, &ctx).unwrap_err().to_string();
    assert!(e.contains("connection refused") && e.contains("challenge"), "{e}");
    let asked = http.seen().len();
    let again = WebChannel::new(WebConfig::default(), None).search(&q, &ctx).unwrap_err().to_string();
    assert!(again.contains("resting"), "{again}");
    assert_eq!(http.seen().len(), asked + 1, "only SearXNG was tried again");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn unconfirmed_identifiers_are_confirmed_by_the_authority_and_fold() {
    use marlowe_research::{Deduper, Identity, Resolution};
    let efetch = String::from_utf8(fixture("corpus/pubmed_deeplearning_efetch.xml")).unwrap();
    let pm = marlowe_research::channels::pubmed::parse_efetch(&efetch, &["26017442".into()]).unwrap().remove(0);
    // A second index carrying the same DOI under a different title spelling and year.
    let oa = SourceRef {
        identity: Some(Identity::default().with_openalex("W1").with_doi("10.1038/nature14539")),
        authors: vec!["Yann LeCun".into()],
        date: Some("2016".into()),
        ..SourceRef::new(ChannelId::OpenAlex, "https://openalex.org/W1", "Deep Learning (review)")
    };
    let mut d = Deduper::default();
    let a = d.add(pm);
    let b = d.add(oa);
    assert_eq!(b.resolution, Resolution::Unresolved);
    let todo = d.unconfirmed();
    assert_eq!(todo, vec![(Namespace::Doi, "10.1038/nature14539".to_string())]);

    let http = Replay::new(&[("api.crossref.org/works/10.1038%2Fnature14539", 200, "crossref_lookup.json")]);
    let reg = ChannelRegistry::keyless(WebConfig::default(), None);
    let cancel = CancelToken::new();
    let ctx = ChannelCtx { http: &http, cache: None, cancel: &cancel };
    for (ns, v) in todo {
        if let Some(authority) = reg.lookup(ns, &v, &ctx).unwrap() {
            d.add(authority);
        }
    }
    assert_eq!(d.len(), 1, "the authority confirmed both indexes' DOI");
    assert_eq!(d.resolve(b.source), a.source);
    assert_eq!(d.get(a.source).unwrap().channels(), [ChannelId::OpenAlex, ChannelId::Crossref, ChannelId::Pubmed]);
    assert!(d.unconfirmed().is_empty() && d.unresolved().is_empty());
}

/// S2's keyless pool answered 429 to every live attempt (see the report); this fixture follows
/// the documented `/paper/search` shape and is labelled synthetic.
#[test]
fn semantic_scholar_maps_external_ids() {
    let http = Replay::new(&[("api.semanticscholar.org/graph/v1/paper/search", 200, "s2_search_synthetic.json")]);
    // Straight to the channel: the process-wide S2 limiter is held by the 429 test above.
    let cancel = CancelToken::new();
    let ctx = ChannelCtx { http: &http, cache: None, cancel: &cancel };
    let r =
        marlowe_research::channels::SemanticScholarChannel::new().search(&ChannelQuery::new("lora", 2), &ctx).unwrap();
    let lora = ids(&r[0]);
    assert_eq!(lora.arxiv_id.as_deref(), Some("2106.09685"));
    assert_eq!(lora.s2.as_deref(), Some("a8ca46b171467ceb2d7652fbfb67fe701ad86092"));
    assert_eq!(r[0].date.as_deref(), Some("2021-06-17"));
    assert_eq!(r[0].meta.get("corpus_id").map(String::as_str), Some("235458009"));
    assert!(!r[0].meta.contains_key("pdf"), "an empty open-access URL is not kept");
    let dl = ids(&r[1]);
    assert_eq!((dl.doi.as_deref(), dl.pmid.as_deref()), (Some("10.1038/nature14539"), Some("26017442")));
    assert_eq!(r[1].date.as_deref(), Some("2015"), "the year stands in for a missing date");
    assert!(http.seen()[0].contains("fields=title,abstract,year"));
}
