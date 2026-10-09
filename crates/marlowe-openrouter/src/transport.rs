//! The socket, behind a port — so the driver's own code path can be driven without one.
//!
//! # Why a trait rather than a direct call into `marlowe-net`
//!
//! ADR-046 requires a test that **the API key cannot reach model context, the journal, or an
//! error string**, asserted where it would leak. Every one of those sites is inside
//! [`crate::driver::OpenRouterDriver`]: the request body it builds, the `--dev` sink it feeds, and
//! the `ProviderError` it returns when an upstream refuses. A test that reimplemented any of them
//! would be asserting on a copy — the family CLAUDE.md logs as *a property asserted where it is
//! declared rather than where it is enforced*.
//!
//! Replacing the socket and nothing else means the test drives **the real driver, the real header
//! assembly, the real error path**, and can then assert on what the transport was handed and on
//! what came back out. [`ScriptedTransport`] is what does that, and it is deliberately `pub`
//! rather than `#[cfg(test)]`: the integration tests that matter live in `tests/`, which compiles
//! against this crate as an external consumer.

use std::io::BufRead;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// What a transport failed at. Kept separate from `ProviderError` because a transport knows
/// nothing about runs, budgets or failover.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TransportError {
    #[error("could not reach {host}: {detail}")]
    Unreachable { host: String, detail: String },
    #[error("malformed response from {host}: {detail}")]
    Malformed { host: String, detail: String },
    #[error("{detail}")]
    Refused { detail: String },
}

impl TransportError {
    /// Whether trying again could plausibly help. A DNS failure or a dropped connection, yes; a
    /// refusal to construct the request, no.
    pub fn is_transient(&self) -> bool {
        matches!(self, TransportError::Unreachable { .. } | TransportError::Malformed { .. })
    }
}

/// One response, body undrained.
pub struct Response {
    pub status: u16,
    /// `Retry-After`, verbatim. Interpreted by [`crate::retry`], not here.
    pub retry_after: Option<String>,
    pub body: Box<dyn BufRead + Send>,
}

impl Response {
    /// Drain at most `limit` bytes, for an error body that is wanted whole and small.
    ///
    /// **Bounded rather than read to end.** A provider's error body can be an HTML page, and an
    /// unbounded read here would put it in a `ProviderError`, on the screen, and in the journal.
    pub fn read_capped(&mut self, limit: usize) -> String {
        use std::io::Read;
        let mut buf = Vec::new();
        let _ = Read::by_ref(&mut self.body).take(limit as u64).read_to_end(&mut buf);
        String::from_utf8_lossy(&buf).into_owned()
    }
}

/// The port. **Send**, because a driver is moved into the daemon's turn.
pub trait Transport: Send {
    /// POST to `path` under the transport's own pinned host.
    ///
    /// `headers` is borrowed and **must not be retained**: one of its values is a credential.
    fn post(
        &self,
        path: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Result<Response, TransportError>;

    /// The host requests go to. For the disclosure line, which announces the destination rather
    /// than letting a reader infer it.
    fn host(&self) -> &str;
}

// ─────────────────────────────────────────────────────────────────────────────────────────
// The real one
// ─────────────────────────────────────────────────────────────────────────────────────────

/// The one host this crate will talk to.
///
/// **Pinned as a constant, not configurable, and that is ADR-046's egress ruling made
/// structural.** The ruling is that a model call is the harness's own infrastructure rather than
/// tool-initiated egress — which holds *only* while the destination cannot be chosen by anything
/// inside a run. A base-URL setting would make it choosable, and the first thing that would reach
/// it is a config file, which is the surface untrusted content gets written into.
///
/// This is the same shape as [`marlowe_provider::LocalEndpoint`], which refuses any host that is
/// not loopback for the mirror-image reason.
pub const OPENROUTER_HOST: &str = "openrouter.ai";

/// The API root. Paths passed to [`Transport::post`] are relative to it.
pub const API_ROOT: &str = "/api/v1";

/// How long one read may block before the connection is called dead.
///
/// Per read, not per response. A frontier model thinking for ninety seconds is working; a
/// whole-response deadline would kill exactly the long calls this adapter exists to make.
/// OpenRouter's keepalive comments arrive well inside this while a request is queued.
pub const READ_TIMEOUT: Duration = Duration::from_secs(120);

/// HTTPS to `openrouter.ai`, over `marlowe-net`.
pub struct TlsTransport {
    client: &'static marlowe_net::Client,
}

impl Default for TlsTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl TlsTransport {
    /// **The process-wide client, not a fresh one.** It owns the TLS session cache and the DNS
    /// cache; a per-call client makes every handshake a full one forever, and the cost is
    /// invisible because nothing reports it. A benchmark is thousands of calls to one host.
    pub fn new() -> Self {
        Self { client: marlowe_net::shared() }
    }
}

impl Transport for TlsTransport {
    fn post(
        &self,
        path: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Result<Response, TransportError> {
        let url = format!("https://{OPENROUTER_HOST}{API_ROOT}{path}");
        let target = marlowe_net::Target::parse(&url).map_err(|e| TransportError::Refused {
            detail: e.to_string(),
        })?;
        let stream = self
            .client
            .post_streaming(&target, headers, body, READ_TIMEOUT)
            .map_err(|e| match e {
                marlowe_net::FetchError::Malformed { host, detail } => {
                    TransportError::Malformed { host, detail }
                }
                marlowe_net::FetchError::Unreachable { host, detail }
                | marlowe_net::FetchError::Tls { host, detail } => {
                    TransportError::Unreachable { host, detail }
                }
                // `Unsupported` here is the header-injection refusal, which is not transient.
                other => TransportError::Refused { detail: other.to_string() },
            })?;
        let status = stream.status;
        let retry_after = stream.retry_after.clone();
        Ok(Response { status, retry_after, body: Box::new(StreamBody(stream)) })
    }

    fn host(&self) -> &str {
        OPENROUTER_HOST
    }
}

/// Adapts `marlowe_net::ResponseStream` to `BufRead`. It already buffers; this only re-exposes it.
struct StreamBody(marlowe_net::ResponseStream);

impl std::io::Read for StreamBody {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.body().read(buf)
    }
}

impl BufRead for StreamBody {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        self.0.body().fill_buf()
    }
    fn consume(&mut self, amt: usize) {
        self.0.body().consume(amt)
    }
}

// ─────────────────────────────────────────────────────────────────────────────────────────
// The scripted one
// ─────────────────────────────────────────────────────────────────────────────────────────

/// One scripted reply.
#[derive(Debug, Clone)]
pub struct Reply {
    pub status: u16,
    pub retry_after: Option<String>,
    pub body: String,
}

impl Reply {
    pub fn ok(body: &str) -> Self {
        Self { status: 200, retry_after: None, body: body.to_string() }
    }
    pub fn status(status: u16, body: &str) -> Self {
        Self { status, retry_after: None, body: body.to_string() }
    }
    pub fn rate_limited(retry_after: Option<&str>) -> Self {
        Self {
            status: 429,
            retry_after: retry_after.map(str::to_string),
            body: "{\"error\":{\"message\":\"rate limited\"}}".into(),
        }
    }
}

/// What one call was handed. **Headers are recorded on purpose**: the key-leak test's whole job is
/// to assert that the credential is in exactly one of them and in nothing else.
#[derive(Debug, Clone, Default)]
pub struct Seen {
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

/// A transport that answers from a script and records what it was asked.
///
/// Replaces **only** the socket: the driver's request assembly, header construction, SSE decoding,
/// retry policy and error mapping all run for real above it.
pub struct ScriptedTransport {
    replies: Mutex<Vec<Reply>>,
    /// Returned once the script runs out. `None` means a transport fault instead, which is how
    /// the retry tests drive the network-down path.
    exhausted: Option<Reply>,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl ScriptedTransport {
    pub fn new(replies: Vec<Reply>) -> Self {
        Self {
            replies: Mutex::new(replies.into_iter().rev().collect()),
            exhausted: None,
            seen: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Answer with `reply` forever once the script is spent. Used where the point is *how many*
    /// attempts happen, not what the last one returns.
    pub fn then_forever(mut self, reply: Reply) -> Self {
        self.exhausted = Some(reply);
        self
    }

    /// A handle onto what the driver sent, readable after the call.
    pub fn seen(&self) -> Arc<Mutex<Vec<Seen>>> {
        Arc::clone(&self.seen)
    }
}

impl Transport for ScriptedTransport {
    fn post(
        &self,
        path: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Result<Response, TransportError> {
        self.seen.lock().expect("seen").push(Seen {
            path: path.to_string(),
            headers: headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            body: String::from_utf8_lossy(body).into_owned(),
        });
        let next = self.replies.lock().expect("script").pop().or_else(|| self.exhausted.clone());
        match next {
            Some(r) => Ok(Response {
                status: r.status,
                retry_after: r.retry_after,
                body: Box::new(std::io::Cursor::new(r.body.into_bytes())),
            }),
            None => Err(TransportError::Unreachable {
                host: OPENROUTER_HOST.into(),
                detail: "the script is exhausted".into(),
            }),
        }
    }

    fn host(&self) -> &str {
        OPENROUTER_HOST
    }
}
