//! The `paper` tool's reading order (arXiv HTML, LaTeX, PDF) against recorded responses.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use marlowe_exec::{HttpGet, HttpResponse, RateLimiter, ResearchConfig};
use marlowe_model::CancelToken;
use serde_json::json;

const SAMPLE: &str = include_str!("fixtures/arxiv_sample.xml");
const ARXIV_HTML: &str = include_str!("../../marlowe-extract/tests/fixtures/arxiv_paper.html");

type Route = (&'static str, u16, &'static str, Vec<u8>);

/// Answers by URL substring (robots.txt: 404, allow all); records every URL asked.
struct Fake(Vec<Route>, Mutex<Vec<String>>);

impl HttpGet for Fake {
    fn get(&self, url: &str, _: &CancelToken) -> Result<HttpResponse, String> {
        self.1.lock().unwrap().push(url.to_string());
        if url.ends_with("/robots.txt") {
            return Ok(HttpResponse { status: 404, content_type: None, bytes: vec![], final_url: url.into() });
        }
        let (_, status, ct, body) = self.0.iter().find(|r| url.contains(r.0)).ok_or("no route")?;
        Ok(HttpResponse {
            status: *status,
            content_type: Some(ct.to_string()),
            bytes: body.clone(),
            final_url: url.into(),
        })
    }
}

fn tools(ws: &Ws, routes: Vec<Route>) -> (marlowe_exec::BuiltinTools, Arc<Fake>) {
    let http = Arc::new(Fake(routes, Mutex::new(Vec::new())));
    let cfg = ResearchConfig {
        cache_dir: None,
        ttl: Duration::from_secs(60),
        limiter: Arc::new(RateLimiter::new(Duration::from_millis(1))),
        http: http.clone(),
    };
    (ws.tools().with_research(cfg), http)
}

#[test]
fn arxiv_html_is_read_first_with_sections_and_parsed_references() {
    let ws = Ws::new("paper-html");
    let (t, http) = tools(
        &ws,
        vec![
            ("api/query", 200, "application/atom+xml", SAMPLE.as_bytes().to_vec()),
            ("arxiv.org/html/2301.01234", 200, "text/html; charset=utf-8", ARXIV_HTML.as_bytes().to_vec()),
        ],
    );
    let o = call(&t, "paper", json!({"url_or_arxiv_id": "2301.01234"}));
    assert!(!o.is_error, "{}", o.content);
    assert!(o.content.starts_with("paper https://arxiv.org/html/2301.01234 · arxiv-html"), "{}", o.content);
    assert!(o.content.contains("Title: Attention & Memory in Small Models"), "API metadata wins: {}", o.content);
    assert!(o.content.contains("Sections: 1 Introduction | 2 Method | 2.1 Training | 3 Results"), "{}", o.content);
    assert!(o.content.contains("References: 4 parsed"), "{}", o.content);
    assert!(o.content.contains("Abstract: We study how attention and memory trade off"), "{}", o.content);
    assert!(o.content.contains("| Heads kept | No memory | With memory |"), "{}", o.content);
    assert!(!http.1.lock().unwrap().iter().any(|u| u.contains("/pdf/") || u.contains("e-print")), "HTML sufficed");

    let r = call(&t, "paper", json!({"url_or_arxiv_id": "2301.01234", "part": "references"}));
    assert!(!r.is_error, "{}", r.content);
    assert!(
        r.content.contains("[1] A. Vaswani, N. Shazeer, N. Parmar, et al. Attention is all you need."),
        "{}",
        r.content
    );
    assert!(r.content.contains("(arXiv:1706.03762)"), "{}", r.content);
    assert!(r.content.contains("(doi:10.1038/s41586-021-03819-2)"), "{}", r.content);
    assert_eq!(r.summary, "4 references");
}

#[test]
fn a_latex_eprint_is_read_when_there_is_no_html() {
    let ws = Ws::new("paper-tex");
    let body = "Sparse memories trade compute for recall in small models. ".repeat(40);
    let tex = format!(
        "\\documentclass{{article}}\\title{{T}}\\begin{{document}}\\begin{{abstract}}A short abstract.\\end{{abstract}}\
         \\section{{Introduction}}{body}\\section{{Method}}\\subsection{{Training}}We train it.\
         \\begin{{thebibliography}}{{9}}\\bibitem{{v}} A. Vaswani. Attention is all you need. arXiv:1706.03762, 2017.\
         \\end{{thebibliography}}\\end{{document}}"
    );
    let (t, http) = tools(
        &ws,
        vec![
            ("api/query", 200, "application/atom+xml", SAMPLE.as_bytes().to_vec()),
            ("arxiv.org/html/", 404, "text/html", b"not found".to_vec()),
            ("arxiv.org/e-print/2301.01234", 200, "application/x-eprint", tex.into_bytes()),
        ],
    );
    let o = call(&t, "paper", json!({"url_or_arxiv_id": "arXiv:2301.01234"}));
    assert!(!o.is_error, "{}", o.content);
    assert!(o.content.starts_with("paper https://arxiv.org/e-print/2301.01234 · latex"), "{}", o.content);
    assert!(o.content.contains("[arXiv HTML: HTTP 404]"), "{}", o.content);
    assert!(o.content.contains("Sections: Introduction | Method | Training"), "{}", o.content);
    assert!(
        o.content.contains("References: 1 parsed") && o.content.contains("Abstract: A short abstract."),
        "{}",
        o.content
    );
    assert!(!http.1.lock().unwrap().iter().any(|u| u.contains("/pdf/")));
}
