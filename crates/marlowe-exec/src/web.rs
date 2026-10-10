//! web_fetch (one page through the corpus reader: robots.txt, host spacing, main content as
//! markdown, a browser for client-rendered pages) and web_search (injected backend).

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use marlowe_agent::ToolOutcome;
use marlowe_extract::main_content::Selector;
use marlowe_model::CancelToken;
use serde_json::Value;

use crate::corpus::{Outcome, Page};
use crate::text::floor_boundary;
use crate::{int_arg, str_arg};

/// Characters of a fetched document returned per call.
pub const FETCH_WINDOW_CHARS: usize = 24_000;

/// Wall-clock limit on one web_fetch, redirects included. marlowe-net's timeout is per read.
pub const FETCH_DEADLINE: Duration = Duration::from_secs(60);

/// How often a waiting fetch checks for cancel.
const POLL: Duration = Duration::from_millis(50);

/// One search hit with its citation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// What a search backend returns: an optional synthesised answer plus cited results.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchResponse {
    pub answer: String,
    pub results: Vec<SearchResult>,
}

/// The search port. The M1 backend is an OpenRouter server tool wired by the bin; closures of the
/// right shape implement it.
pub trait SearchBackend: Send + Sync {
    fn search(&self, query: &str, cancel: &CancelToken) -> Result<SearchResponse, String>;
}

impl<F> SearchBackend for F
where
    F: Fn(&str, &CancelToken) -> Result<SearchResponse, String> + Send + Sync,
{
    fn search(&self, query: &str, cancel: &CancelToken) -> Result<SearchResponse, String> {
        self(query, cancel)
    }
}

pub(crate) fn web_search(backend: &dyn SearchBackend, args: &Value, cancel: &CancelToken) -> ToolOutcome {
    let Some(query) = str_arg(args, "query").map(str::trim).filter(|q| !q.is_empty()) else {
        return ToolOutcome::error("`query` is required");
    };
    match backend.search(query, cancel) {
        Err(e) => ToolOutcome::error(format!("web search failed: {e}")),
        Ok(r) => {
            let mut out = String::new();
            if !r.answer.trim().is_empty() {
                out.push_str(r.answer.trim());
                out.push_str("\n\n");
            }
            if r.results.is_empty() {
                out.push_str("[no cited results]");
            } else {
                out.push_str("Sources:\n");
                for (i, s) in r.results.iter().enumerate() {
                    out.push_str(&format!("{}. {} — {}\n", i + 1, s.title, s.url));
                    if !s.snippet.is_empty() {
                        out.push_str(&format!("   {}\n", s.snippet.trim()));
                    }
                }
            }
            ToolOutcome::ok(out.trim_end().to_string(), format!("{} sources", r.results.len()))
        }
    }
}

/// Why waiting for a worker ended without a result.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Stopped {
    Cancelled,
    Deadline,
    WorkerLost,
}

/// Waits for `rx`, giving up on cancel or at `deadline`. An abandoned worker finishes detached.
pub(crate) fn wait_for<T>(rx: &Receiver<T>, cancel: &CancelToken, deadline: Instant) -> Result<T, Stopped> {
    loop {
        match rx.recv_timeout(POLL) {
            Ok(v) => return Ok(v),
            Err(RecvTimeoutError::Disconnected) => return Err(Stopped::WorkerLost),
            Err(RecvTimeoutError::Timeout) => {
                if cancel.is_cancelled() {
                    return Err(Stopped::Cancelled);
                }
                if Instant::now() >= deadline {
                    return Err(Stopped::Deadline);
                }
            }
        }
    }
}

pub(crate) fn web_fetch(args: &Value, cancel: &CancelToken) -> ToolOutcome {
    let Some(url) = str_arg(args, "url").map(str::trim) else {
        return ToolOutcome::error("`url` is required: a full https:// URL");
    };
    let offset = int_arg(args, "offset").unwrap_or(0).max(0) as usize;
    let cfg = crate::corpus::CorpusConfig::default();
    let selector = Selector::with_defaults();
    match crate::corpus::read_one(url, &cfg, &crate::corpus::NetPages, &selector, cancel) {
        Outcome::Read(page) => render_page(&page, offset),
        Outcome::Refused { reason: marlowe_research::Refusal::Cancelled, .. } => ToolOutcome {
            content: "web_fetch stopped: the turn was interrupted.".into(),
            is_error: true,
            summary: "interrupted".into(),
            parts: Vec::new(),
        },
        Outcome::Unreachable { detail, .. } => ToolOutcome::error(format!("could not fetch {url}: {detail}")),
        other => {
            let why = other.why_not().unwrap_or_default();
            ToolOutcome::error(format!("{url} was not read: {why}."))
        }
    }
}

/// A page for the model: where it came from, how it was read, then a window of its markdown.
fn render_page(page: &Page, offset: usize) -> ToolOutcome {
    let body = crate::corpus::render(page);
    let total = body.chars().count();
    let start_byte = body.char_indices().nth(offset).map_or(body.len(), |(i, _)| i);
    let rest = &body[start_byte..];
    let window_end = rest.char_indices().nth(FETCH_WINDOW_CHARS).map_or(rest.len(), |(i, _)| i);
    let window = &rest[..floor_boundary(rest, window_end)];
    let shown = window.chars().count();

    let mut out = format!("{} · {}", page.final_url, page.document.format.as_str());
    if page.rendered {
        out.push_str(" · rendered in a browser");
    }
    out.push_str("\n\n");
    out.push_str(window);
    if offset + shown < total {
        out.push_str(&format!(
            "\n\n[showing characters {offset}-{} of {total}. Continue with offset {}.]",
            offset + shown,
            offset + shown
        ));
    }
    ToolOutcome::ok(out, format!("{} · {total} chars", page.status))
}

#[cfg(test)]
mod tests {
    use super::*;
    use marlowe_extract::Document;
    use std::sync::mpsc::sync_channel;

    #[test]
    fn waiting_stops_on_cancel_and_at_the_deadline() {
        let (_tx, rx) = sync_channel::<()>(1);
        let cancel = CancelToken::new();
        let c = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            c.cancel();
        });
        let t0 = Instant::now();
        assert_eq!(wait_for(&rx, &cancel, t0 + Duration::from_secs(30)), Err(Stopped::Cancelled));
        assert!(t0.elapsed() < Duration::from_secs(5));
        let t0 = Instant::now();
        assert_eq!(wait_for(&rx, &CancelToken::new(), t0 + Duration::from_millis(100)), Err(Stopped::Deadline));
        let (tx, rx) = sync_channel(1);
        tx.send(7).unwrap();
        assert_eq!(wait_for(&rx, &CancelToken::new(), Instant::now()), Ok(7));
    }

    #[test]
    fn a_long_document_is_windowed_with_the_next_offset() {
        let doc = Document {
            format: marlowe_extract::Format::PlainText,
            title: Some("T".into()),
            text: "x".repeat(FETCH_WINDOW_CHARS + 500),
            links: vec![],
            headings: vec![],
            lang: None,
            description: None,
            bytes_in: 0,
            encoding: "utf-8",
            warnings: vec![],
        };
        let page = Page {
            url: "https://a.com".into(),
            final_url: "https://a.com/".into(),
            status: 200,
            content_type: None,
            markdown: doc.text.clone(),
            doc: marlowe_research::DocRef::of(b"x"),
            document: doc,
            reading: None,
            rendered: false,
            wire_bytes: 0,
        };
        let o = render_page(&page, 0);
        assert!(!o.is_error);
        assert!(o.content.starts_with("https://a.com/ · text"), "{}", &o.content[..40]);
        let total = page.markdown.chars().count() + "# T\n\n".len();
        assert!(o.content.contains(&format!("of {total}. Continue with offset {FETCH_WINDOW_CHARS}")));
        let o = render_page(&page, FETCH_WINDOW_CHARS);
        assert!(!o.content.contains("Continue"));
    }
}
