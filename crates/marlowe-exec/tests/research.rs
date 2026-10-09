//! arxiv and paper against recorded responses. Live tests are #[ignore].

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use marlowe_agent::{Consequence, Exposure, ToolHost};
use marlowe_exec::{HttpGet, HttpResponse, RateLimiter, ResearchConfig};
use marlowe_model::CancelToken;
use serde_json::json;

const SAMPLE: &str = include_str!("fixtures/arxiv_sample.xml");
const ERROR: &str = include_str!("fixtures/arxiv_error.xml");

type Route = (&'static str, u16, &'static str, Vec<u8>);

/// Answers by URL substring; records every URL asked.
struct Fake {
    routes: Vec<Route>,
    seen: Mutex<Vec<String>>,
    hits: AtomicUsize,
}

impl Fake {
    fn new(routes: Vec<Route>) -> Arc<Self> {
        Arc::new(Self { routes, seen: Mutex::new(vec![]), hits: AtomicUsize::new(0) })
    }
}

impl HttpGet for Fake {
    fn get(&self, url: &str, _: &CancelToken) -> Result<HttpResponse, String> {
        self.hits.fetch_add(1, Ordering::SeqCst);
        self.seen.lock().unwrap().push(url.to_string());
        let (_, status, ct, body) = self.routes.iter().find(|r| url.contains(r.0)).ok_or("no route")?;
        Ok(HttpResponse {
            status: *status,
            content_type: Some(ct.to_string()),
            bytes: body.clone(),
            final_url: url.to_string(),
        })
    }
}

fn cfg(http: Arc<Fake>, cache: Option<&std::path::Path>) -> ResearchConfig {
    ResearchConfig {
        cache_dir: cache.map(|p| p.to_path_buf()),
        ttl: Duration::from_secs(3600),
        limiter: Arc::new(RateLimiter::new(Duration::from_millis(1))),
        http,
    }
}

fn atom(body: &str) -> Vec<u8> {
    body.as_bytes().to_vec()
}

/// A one-page PDF with a text layer, xref offsets computed.
fn text_pdf(lines: &[&str]) -> Vec<u8> {
    let mut stream = String::from("BT /F1 12 Tf 72 720 Td 14 TL\n");
    for l in lines {
        stream.push_str(&format!("({l}) Tj T*\n"));
    }
    stream.push_str("ET");
    let objs = [
        "<</Type/Catalog/Pages 2 0 R>>".to_string(),
        "<</Type/Pages/Kids[3 0 R]/Count 1>>".to_string(),
        "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Contents 4 0 R/Resources<</Font<</F1 5 0 R>>>>>>".to_string(),
        format!("<</Length {}>>\nstream\n{stream}\nendstream", stream.len()),
        "<</Type/Font/Subtype/Type1/BaseFont/Helvetica/Encoding/WinAnsiEncoding>>".to_string(),
    ];
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offs = vec![];
    for (i, o) in objs.iter().enumerate() {
        offs.push(out.len());
        out.extend(format!("{} 0 obj\n{o}\nendobj\n", i + 1).bytes());
    }
    let xref = out.len();
    out.extend(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).bytes());
    for o in offs {
        out.extend(format!("{o:010} 00000 n \n").bytes());
    }
    out.extend(format!("trailer<</Size {}/Root 1 0 R>>\nstartxref\n{xref}\n%%EOF\n", objs.len() + 1).bytes());
    out
}

#[test]
fn arxiv_parses_and_renders_recorded_atom() {
    let ws = Ws::new("rs-arxiv");
    let http = Fake::new(vec![("export.arxiv.org/api/query", 200, "application/atom+xml", atom(SAMPLE))]);
    let t = ws.tools().with_research(cfg(http.clone(), None));
    let o = call(&t, "arxiv", json!({"query": "attention memory", "max": 2}));
    assert!(!o.is_error, "{}", o.content);
    assert!(o.content.contains("2 of 1234 papers"), "{}", o.content);
    assert!(
        o.content.contains("1. Attention & Memory in Small Models — Ada Lovelace, Alan Turing (2023-01-03, cs.LG)"),
        "{}",
        o.content
    );
    assert!(o.content.contains("id 2301.01234v2 · https://arxiv.org/abs/2301.01234v2 · pdf https://arxiv.org/pdf/2301.01234v2"));
    assert!(o.content.contains("We study how attention and memory trade off. Results hold for <10M parameter models."));
    assert!(o.content.contains("A One, B Two, C Three et al. (5 authors)"));
    assert!(o.content.contains("id cs/0601001v1"));
    assert_eq!(o.summary, "2 papers");
    let url = http.seen.lock().unwrap()[0].clone();
    assert!(url.contains("search_query=all:attention+memory") && url.contains("max_results=2"), "{url}");
}

#[test]
fn arxiv_errors_are_reported() {
    let ws = Ws::new("rs-arxiv-err");
    let http = Fake::new(vec![("id_list", 200, "application/atom+xml", atom(ERROR))]);
    let t = ws.tools().with_research(cfg(http, None));
    let o = call(&t, "arxiv", json!({"query": "2301.01234"}));
    assert!(o.is_error && o.content.contains("incorrect id format"), "{}", o.content);
    assert!(call(&t, "arxiv", json!({"query": "  "})).is_error);

    let http = Fake::new(vec![("export.arxiv.org", 503, "text/html", b"busy".to_vec())]);
    let t = ws.tools().with_research(cfg(http, None));
    let o = call(&t, "arxiv", json!({"query": "x"}));
    assert!(o.is_error && o.content.contains("HTTP 503"));
}

#[test]
fn arxiv_results_are_cached_and_expire() {
    let ws = Ws::new("rs-arxiv-cache");
    let cache = ws.path("cache");
    let http = Fake::new(vec![("export.arxiv.org", 200, "application/atom+xml", atom(SAMPLE))]);
    let t = ws.tools().with_research(cfg(http.clone(), Some(&cache)));
    assert_eq!(call(&t, "arxiv", json!({"query": "q"})).summary, "2 papers");
    let again = call(&t, "arxiv", json!({"query": "q"}));
    assert_eq!(again.summary, "2 papers (cached)");
    assert_eq!(http.hits.load(Ordering::SeqCst), 1);
    assert!(cache.join("arxiv").is_dir());
    // A different query is a different key.
    call(&t, "arxiv", json!({"query": "other"}));
    assert_eq!(http.hits.load(Ordering::SeqCst), 2);
    // ttl 0 means every entry is stale.
    let mut c = cfg(http.clone(), Some(&cache));
    c.ttl = Duration::ZERO;
    std::thread::sleep(Duration::from_millis(20));
    let t = ws.tools().with_research(c);
    assert_eq!(call(&t, "arxiv", json!({"query": "q"})).summary, "2 papers");
    assert_eq!(http.hits.load(Ordering::SeqCst), 3);
    // Errors are not cached.
    let bad = Fake::new(vec![("export.arxiv.org", 200, "application/atom+xml", atom(ERROR))]);
    let t = ws.tools().with_research(cfg(bad.clone(), Some(&cache)));
    call(&t, "arxiv", json!({"query": "zzz"}));
    call(&t, "arxiv", json!({"query": "zzz"}));
    assert_eq!(bad.hits.load(Ordering::SeqCst), 2);
}

#[test]
fn arxiv_requests_share_the_rate_limit() {
    let ws = Ws::new("rs-arxiv-rate");
    let http = Fake::new(vec![("export.arxiv.org", 200, "application/atom+xml", atom(SAMPLE))]);
    let mut c = cfg(http, None);
    c.limiter = Arc::new(RateLimiter::new(Duration::from_millis(150)));
    let t = ws.tools().with_research(c);
    let t0 = std::time::Instant::now();
    for q in ["a", "b", "c"] {
        call(&t, "arxiv", json!({"query": q}));
    }
    assert!(t0.elapsed() >= Duration::from_millis(300), "{:?}", t0.elapsed());
}

#[test]
fn paper_reads_a_pdf_by_arxiv_id() {
    let ws = Ws::new("rs-paper-pdf");
    let pdf = text_pdf(&[
        "Abstract",
        "We show that small models can remember.",
        "1 Introduction",
        "Memory matters a great deal.",
    ]);
    let http = Fake::new(vec![
        ("api/query", 200, "application/atom+xml", atom(SAMPLE)),
        ("arxiv.org/pdf/2301.01234", 200, "application/pdf", pdf),
    ]);
    let t = ws.tools().with_research(cfg(http, None));
    let o = call(&t, "paper", json!({"url_or_arxiv_id": "arXiv:2301.01234v2"}));
    assert!(!o.is_error, "{}", o.content);
    assert!(o.content.contains("paper https://arxiv.org/pdf/2301.01234v2"), "{}", o.content);
    assert!(o.content.contains("Title: Attention & Memory in Small Models"));
    assert!(o.content.contains("Authors: Ada Lovelace, Alan Turing"));
    assert!(o.content.contains("small models can remember"), "{}", o.content);
    assert!(o.content.contains("Sections: ") && o.content.contains("1 Introduction"), "{}", o.content);
}

#[test]
fn paper_reads_html_and_windows_long_text() {
    let ws = Ws::new("rs-paper-html");
    let mut html =
        String::from("<html><head><title>On Widgets</title></head><body><h1>On Widgets</h1><h2>Methods</h2><p>");
    html.push_str(&"word ".repeat(5000));
    html.push_str("</p></body></html>");
    let http = Fake::new(vec![("example.org/p", 200, "text/html; charset=utf-8", html.into_bytes())]);
    let t = ws.tools().with_research(cfg(http, None));
    let o = call(&t, "paper", json!({"url_or_arxiv_id": "https://example.org/p"}));
    assert!(!o.is_error, "{}", o.content);
    assert!(o.content.contains("Title: On Widgets") && o.content.contains("Methods"), "{}", &o.content[..300]);
    assert!(o.content.contains("Continue with offset"), "long paper is windowed");
    let o2 = call(&t, "paper", json!({"url_or_arxiv_id": "https://example.org/p", "offset": 12000}));
    assert!(!o2.is_error && o2.content.contains("[showing characters 12000-"));
}

#[test]
fn paper_refuses_bad_input_and_http_errors() {
    let ws = Ws::new("rs-paper-bad");
    let http = Fake::new(vec![("example.org/gone", 404, "text/html", b"nope".to_vec())]);
    let t = ws.tools().with_research(cfg(http, None));
    assert!(call(&t, "paper", json!({})).is_error);
    assert!(call(&t, "paper", json!({"url_or_arxiv_id": "http://example.org/a.pdf"})).is_error);
    assert!(call(&t, "paper", json!({"url_or_arxiv_id": "not a paper"})).is_error);
    let o = call(&t, "paper", json!({"url_or_arxiv_id": "https://example.org/gone"}));
    assert!(o.is_error && o.content.contains("HTTP 404"));
}

#[test]
fn research_tools_are_untrusted_content_with_egress_rules() {
    let ws = Ws::new("rs-taint");
    let t = ws.tools();
    let a = req("arxiv", json!({"query": "x"}));
    let p = req("paper", json!({"url_or_arxiv_id": "2301.01234"}));
    let other = req("paper", json!({"url_or_arxiv_id": "https://example.org/a.pdf"}));
    assert!(t.taints(&a) && t.taints(&p));
    assert_eq!(t.egress_host(&p).as_deref(), Some("arxiv.org"));
    assert_eq!(t.consequence(&a), Consequence::Free);
    assert_eq!(t.consequence(&p), Consequence::Free);
    let clean = Exposure::default();
    assert_eq!(t.consequence_exposed(&other, &clean), Consequence::Free);
    let tainted = Exposure { tainted: true, ..Exposure::default() };
    assert!(matches!(t.consequence_exposed(&a, &tainted), Consequence::Ask { .. }));
    assert!(matches!(t.consequence_exposed(&other, &tainted), Consequence::Ask { .. }));
    let mut approved = Exposure { tainted: true, ..Exposure::default() };
    approved.approved_hosts.insert("example.org".into());
    assert_eq!(t.consequence_exposed(&other, &approved), Consequence::Free);
}

#[test]
#[ignore = "live network: export.arxiv.org"]
fn live_arxiv_search_and_paper() {
    let ws = Ws::new("rs-live");
    let t = ws.tools();
    let o = call(&t, "arxiv", json!({"query": "ti:\"attention is all you need\"", "max": 3}));
    assert!(!o.is_error, "{}", o.content);
    assert!(o.content.contains("1706.03762"), "{}", o.content);
    let p = call(&t, "paper", json!({"url_or_arxiv_id": "1706.03762"}));
    assert!(!p.is_error, "{}", p.content);
    assert!(p.content.contains("Attention"));
}
