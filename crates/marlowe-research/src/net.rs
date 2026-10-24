//! The real network behind [`HttpGet`]: https through marlowe-net's pooled client (public
//! addresses only, redirects followed and each hop re-parsed as https), and plaintext http only to
//! a loopback endpoint (a local SearXNG) through marlowe-provider's loopback client (ADR-035
//! §4.1). Every request carries the caller's User-Agent; nothing adds a credential.

use std::io::Read;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use marlowe_model::CancelToken;

use crate::channel::{HttpGet, HttpRequest, HttpResponse};

/// Redirect hops followed per request.
pub const MAX_REDIRECTS: usize = 5;
/// A request that has not completed by now is abandoned.
pub const DEADLINE: Duration = Duration::from_secs(45);
/// Loopback bodies above this are cut off.
const MAX_LOOPBACK_BYTES: u64 = 16 * 1024 * 1024;

/// HTTP for channels and the host gate.
#[derive(Debug, Clone)]
pub struct NetHttp {
    deadline: Duration,
}

impl Default for NetHttp {
    fn default() -> Self {
        Self::new()
    }
}

impl NetHttp {
    pub fn new() -> Self {
        Self { deadline: DEADLINE }
    }

    pub fn with_deadline(mut self, d: Duration) -> Self {
        self.deadline = d;
        self
    }
}

/// `Retry-After` in seconds (an HTTP-date is not interpreted; callers fall back to a default).
pub fn retry_after_secs(v: Option<&str>) -> Option<Duration> {
    v?.trim().parse::<u64>().ok().map(|s| Duration::from_secs(s.min(24 * 3600)))
}

/// `location` resolved against `current` (absolute, scheme-relative or root-relative).
fn join(current: &str, location: &str) -> String {
    if location.starts_with("https://") || location.starts_with("http://") {
        return location.to_string();
    }
    let (scheme, rest) = current.split_once("://").unwrap_or(("https", current));
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    if let Some(r) = location.strip_prefix("//") {
        return format!("{scheme}://{r}");
    }
    if location.starts_with('/') {
        return format!("{scheme}://{host}{location}");
    }
    let dir = rest.rfind('/').filter(|i| *i > host.len()).map_or(host, |i| &rest[..i]);
    format!("{scheme}://{dir}/{location}")
}

fn https_chain(req: &HttpRequest) -> Result<HttpResponse, String> {
    let headers = marlowe_net::Headers { user_agent: req.user_agent, accept: req.accept.unwrap_or("*/*") };
    let mut url = req.url.clone();
    for _ in 0..=MAX_REDIRECTS {
        let target = marlowe_net::Target::parse(&url).map_err(|e| e.to_string())?;
        let f = marlowe_net::fetch_with(&target, &headers).map_err(|e| e.to_string())?;
        if let Some(next) = f.redirect_to.as_deref() {
            url = join(&url, next);
            continue;
        }
        return Ok(HttpResponse {
            status: f.status,
            content_type: f.content_type,
            bytes: f.bytes,
            final_url: url,
            retry_after: retry_after_secs(f.retry_after.as_deref()),
        });
    }
    Err(format!("more than {MAX_REDIRECTS} redirects from {}", req.url))
}

fn loopback(req: &HttpRequest, cancel: &CancelToken, idle: Duration) -> Result<HttpResponse, String> {
    let rest = req.url.strip_prefix("http://").unwrap_or(&req.url);
    let i = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, path) = rest.split_at(i);
    let endpoint = marlowe_provider::LocalEndpoint::parse(authority)
        .ok_or_else(|| format!("plaintext http is only used for loopback, not {authority}"))?;
    let path = if path.is_empty() {
        "/".to_string()
    } else if path.starts_with('?') {
        format!("/{path}")
    } else {
        path.to_string()
    };
    let accept = req.accept.unwrap_or("*/*");
    let resp = marlowe_provider::http::send(&endpoint, "GET", &path, None, accept, Some(cancel), idle)
        .map_err(|e| e.to_string())?;
    let status = resp.status;
    let mut bytes = Vec::new();
    resp.body.take(MAX_LOOPBACK_BYTES).read_to_end(&mut bytes).map_err(|e| e.to_string())?;
    Ok(HttpResponse { status, content_type: None, bytes, final_url: req.url.clone(), retry_after: None })
}

/// The longest body a free search API answer may have; more is cut off.
const MAX_POST_BYTES: usize = 4 * 1024 * 1024;

/// One JSON POST over https through marlowe-net (no redirect followed; the body read to
/// [`MAX_POST_BYTES`]).
fn https_post(req: &HttpRequest, body: &[u8], read_timeout: Duration) -> Result<HttpResponse, String> {
    let target = marlowe_net::Target::parse(&req.url).map_err(|e| e.to_string())?;
    let accept = req.accept.unwrap_or("application/json");
    let mut r = marlowe_net::shared()
        .post_streaming(&target, &[("Accept", accept)], body, read_timeout)
        .map_err(|e| e.to_string())?;
    let text = r.read_capped(MAX_POST_BYTES);
    Ok(HttpResponse {
        status: r.status,
        content_type: r.content_type.clone(),
        bytes: text.into_bytes(),
        final_url: req.url.clone(),
        retry_after: retry_after_secs(r.retry_after.as_deref()),
    })
}

impl NetHttp {
    /// Runs `work` on a worker, so a cancel or the deadline returns promptly (marlowe-net blocks).
    fn on_worker(
        &self,
        cancel: &CancelToken,
        name: &str,
        work: impl FnOnce() -> Result<HttpResponse, String> + Send + 'static,
    ) -> Result<HttpResponse, String> {
        let (tx, rx) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                let _ = tx.send(work());
            })
            .map_err(|e| format!("could not start the request: {e}"))?;
        let until = Instant::now() + self.deadline;
        loop {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(r) => return r,
                Err(mpsc::RecvTimeoutError::Disconnected) => return Err("the request failed unexpectedly".into()),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if cancel.is_cancelled() {
                        return Err("cancelled".into());
                    }
                    if Instant::now() >= until {
                        return Err(format!("no complete response within {} s", self.deadline.as_secs()));
                    }
                }
            }
        }
    }
}

impl HttpGet for NetHttp {
    fn get(&self, req: &HttpRequest, cancel: &CancelToken) -> Result<HttpResponse, String> {
        if cancel.is_cancelled() {
            return Err("cancelled".into());
        }
        if req.url.starts_with("http://") {
            return loopback(req, cancel, self.deadline.min(Duration::from_secs(20)));
        }
        if !req.url.starts_with("https://") {
            return Err(format!("not an http(s) URL: {}", req.url));
        }
        let owned = req.clone();
        self.on_worker(cancel, "marlowe-research-get", move || https_chain(&owned))
    }

    fn post_json(&self, req: &HttpRequest, body: &[u8], cancel: &CancelToken) -> Result<HttpResponse, String> {
        if cancel.is_cancelled() {
            return Err("cancelled".into());
        }
        if !req.url.starts_with("https://") {
            return Err(format!("a POST goes only to https, not {}", req.url));
        }
        let (owned, body, read) = (req.clone(), body.to_vec(), self.deadline);
        self.on_worker(cancel, "marlowe-research-post", move || https_post(&owned, &body, read))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirects_resolve_and_retry_after_reads_seconds() {
        assert_eq!(join("https://a.org/x/y?z", "/p"), "https://a.org/p");
        assert_eq!(join("https://a.org/x/y", "q"), "https://a.org/x/q");
        assert_eq!(join("https://a.org", "q"), "https://a.org/q");
        assert_eq!(join("https://a.org/x", "//b.org/r"), "https://b.org/r");
        assert_eq!(join("https://a.org/x", "https://c.org/"), "https://c.org/");
        assert_eq!(retry_after_secs(Some(" 30 ")), Some(Duration::from_secs(30)));
        assert_eq!(retry_after_secs(Some("Wed, 21 Oct 2015 07:28:00 GMT")), None);
    }

    #[test]
    fn plain_http_is_loopback_only_and_other_schemes_are_refused() {
        let c = CancelToken::new();
        let e = NetHttp::new().get(&HttpRequest::get("http://example.com/x"), &c).unwrap_err();
        assert!(e.contains("only used for loopback"), "{e}");
        assert!(NetHttp::new().get(&HttpRequest::get("ftp://x.org/"), &c).is_err());
        // Nothing listens on port 9 on loopback: a prompt connection error, not a hang.
        let t = Instant::now();
        assert!(NetHttp::new().get(&HttpRequest::get("http://127.0.0.1:9/search"), &c).is_err());
        assert!(t.elapsed() < Duration::from_secs(10));
    }
}
