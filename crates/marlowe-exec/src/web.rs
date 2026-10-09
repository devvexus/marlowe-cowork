//! web_fetch (marlowe-net + marlowe-extract) and web_search (injected backend).

use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use marlowe_agent::ToolOutcome;
use marlowe_extract::{Document, Input};
use marlowe_model::CancelToken;
use serde_json::Value;

use crate::text::floor_boundary;
use crate::{int_arg, str_arg};

/// Characters of a fetched document returned per call.
pub const FETCH_WINDOW_CHARS: usize = 24_000;

/// Redirects followed per fetch. Each target is parsed as https only.
pub const MAX_REDIRECTS: usize = 5;

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
    let Some(url) = str_arg(args, "url") else {
        return ToolOutcome::error("`url` is required: a full https:// URL");
    };
    let offset = int_arg(args, "offset").unwrap_or(0).max(0) as usize;
    let url = url.trim().to_string();
    let (tx, rx) = sync_channel(1);
    let worker = std::thread::Builder::new().name("marlowe-web-fetch".into()).spawn(move || {
        let _ = tx.send(fetch_chain(url));
    });
    if let Err(e) = worker {
        return ToolOutcome::error(format!("could not start the fetch: {e}"));
    }
    let (current, res) = match wait_for(&rx, cancel, Instant::now() + FETCH_DEADLINE) {
        Ok(Ok(done)) => done,
        Ok(Err(refused)) => return refused,
        Err(Stopped::Cancelled) => {
            return ToolOutcome {
                content: "web_fetch stopped: the turn was interrupted.".into(),
                is_error: true,
                summary: "interrupted".into(),
            }
        }
        Err(Stopped::Deadline) => {
            return ToolOutcome::error(format!(
                "web_fetch gave up after {} s without a complete response.",
                FETCH_DEADLINE.as_secs()
            ))
        }
        Err(Stopped::WorkerLost) => return ToolOutcome::error("the fetch failed unexpectedly"),
    };
    let status = res.status;
    let doc = match marlowe_extract::extract(
        &Input::new(&res.bytes).content_type(res.content_type.as_deref()).url(Some(&current)),
    ) {
        Ok(d) => d,
        Err(e) => {
            return ToolOutcome::error(format!(
                "{current} returned {} bytes that could not be turned into text ({}).",
                res.bytes.len(),
                e.kind()
            ))
        }
    };
    render_document(&current, status, &doc, offset)
}

/// Fetches `url`, following https redirects. Returns the final URL and its response.
fn fetch_chain(url: String) -> Result<(String, marlowe_net::Fetched), ToolOutcome> {
    let mut current = url;
    let mut fetched = None;
    for _ in 0..=MAX_REDIRECTS {
        let target = match marlowe_net::Target::parse(&current) {
            Ok(t) => t,
            Err(e) => return Err(ToolOutcome::error(e.to_string())),
        };
        let res = match marlowe_net::fetch(&target) {
            Ok(r) => r,
            Err(e) => return Err(ToolOutcome::error(e.to_string())),
        };
        match res.redirect_to.clone() {
            Some(loc) => match resolve_location(&target.host, &loc) {
                Some(next) => current = next,
                None => {
                    return Err(ToolOutcome::error(format!(
                        "{current} redirected ({}) to a location that is not an https URL, so it \
                         was not followed.",
                        res.status
                    )))
                }
            },
            None => {
                fetched = Some(res);
                break;
            }
        }
    }
    match fetched {
        Some(res) => Ok((current, res)),
        None => Err(ToolOutcome::error(format!("too many redirects (more than {MAX_REDIRECTS})"))),
    }
}

fn resolve_location(host: &str, loc: &str) -> Option<String> {
    let loc = loc.trim();
    if loc.starts_with("https://") {
        Some(loc.to_string())
    } else if loc.starts_with('/') && !loc.starts_with("//") {
        Some(format!("https://{host}{loc}"))
    } else {
        None
    }
}

fn render_document(url: &str, status: u16, doc: &Document, offset: usize) -> ToolOutcome {
    let total = doc.text.chars().count();
    let start_byte = doc.text.char_indices().nth(offset).map_or(doc.text.len(), |(i, _)| i);
    let rest = &doc.text[start_byte..];
    let window_end = rest
        .char_indices()
        .nth(FETCH_WINDOW_CHARS)
        .map_or(rest.len(), |(i, _)| i);
    let window = &rest[..floor_boundary(rest, window_end)];
    let shown = window.chars().count();

    let mut out = String::new();
    if status >= 400 {
        out.push_str(&format!(
            "The server returned HTTP {status}. What follows is what came back with that error, \
             not the document you asked for.\n"
        ));
    }
    out.push_str(&format!("{url} · {}", doc.format.as_str()));
    if let Some(t) = &doc.title {
        out.push_str(&format!(" · {t}"));
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
    for w in &doc.warnings {
        out.push_str(&format!("\n[{w}]"));
    }
    let summary = format!("{status} · {total} chars");
    if status >= 400 {
        ToolOutcome { content: out, is_error: true, summary }
    } else {
        ToolOutcome::ok(out, summary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn locations_are_https_only() {
        assert_eq!(resolve_location("a.com", "/x").as_deref(), Some("https://a.com/x"));
        assert_eq!(resolve_location("a.com", "https://b.com/y").as_deref(), Some("https://b.com/y"));
        assert!(resolve_location("a.com", "http://b.com").is_none());
        assert!(resolve_location("a.com", "//b.com").is_none());
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
        let o = render_document("https://a.com", 200, &doc, 0);
        assert!(!o.is_error);
        assert!(o.content.contains(&format!("Continue with offset {FETCH_WINDOW_CHARS}")));
        let o = render_document("https://a.com", 200, &doc, FETCH_WINDOW_CHARS);
        assert!(!o.content.contains("Continue"));
        let o = render_document("https://a.com", 404, &doc, 0);
        assert!(o.is_error && o.content.contains("HTTP 404"));
    }
}
