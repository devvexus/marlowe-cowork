//! The socket behind a port, so the provider's real request, header, decoding and error paths
//! can be driven by [`ScriptedTransport`] in tests.

use std::io::{BufRead, Read};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// What a transport failed at.
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
    /// Whether trying again could plausibly help.
    pub fn is_transient(&self) -> bool {
        matches!(self, TransportError::Unreachable { .. } | TransportError::Malformed { .. })
    }
}

/// One response, body undrained.
pub struct Response {
    pub status: u16,
    /// `Retry-After`, verbatim.
    pub retry_after: Option<String>,
    pub body: Box<dyn BufRead + Send>,
}

impl Response {
    /// Drains at most `limit` bytes (error bodies can be whole HTML pages).
    pub fn read_capped(&mut self, limit: usize) -> String {
        let mut buf = Vec::new();
        let _ = Read::by_ref(&mut self.body).take(limit as u64).read_to_end(&mut buf);
        String::from_utf8_lossy(&buf).into_owned()
    }
}

/// HTTPS to the pinned host. `headers` are borrowed and must not be retained: one is a credential.
pub trait Transport: Send + Sync {
    fn post(&self, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Result<Response, TransportError>;

    /// A GET whose whole body is wanted (the model catalog).
    fn get(&self, path: &str) -> Result<Response, TransportError>;

    fn host(&self) -> &str;
}

/// The one host this crate talks to. Not configurable: a base-URL setting would let something
/// inside a run choose where model traffic (and the key) goes.
pub const OPENROUTER_HOST: &str = "openrouter.ai";

/// API root; paths passed to [`Transport`] are relative to it.
pub const API_ROOT: &str = "/api/v1";

/// How long one read may block before the connection is called dead. Per read, not per
/// response: OpenRouter sends keepalive comments while a request is queued.
pub const READ_TIMEOUT: Duration = Duration::from_secs(120);

/// HTTPS to `openrouter.ai` over `marlowe-net`.
pub struct TlsTransport {
    client: &'static marlowe_net::Client,
}

impl Default for TlsTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl TlsTransport {
    /// Uses the process-wide client (TLS session and DNS caches).
    pub fn new() -> Self {
        Self { client: marlowe_net::shared() }
    }

    fn target(path: &str) -> Result<marlowe_net::Target, TransportError> {
        let url = format!("https://{OPENROUTER_HOST}{API_ROOT}{path}");
        marlowe_net::Target::parse(&url).map_err(|e| TransportError::Refused { detail: e.to_string() })
    }
}

fn map_fetch(e: marlowe_net::FetchError) -> TransportError {
    match e {
        marlowe_net::FetchError::Malformed { host, detail } => TransportError::Malformed { host, detail },
        marlowe_net::FetchError::Unreachable { host, detail }
        | marlowe_net::FetchError::Tls { host, detail } => TransportError::Unreachable { host, detail },
        // `Unsupported` is the header-injection refusal, which is not transient.
        other => TransportError::Refused { detail: other.to_string() },
    }
}

impl Transport for TlsTransport {
    fn post(&self, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Result<Response, TransportError> {
        let target = Self::target(path)?;
        let stream = self.client.post_streaming(&target, headers, body, READ_TIMEOUT).map_err(map_fetch)?;
        let status = stream.status;
        let retry_after = stream.retry_after.clone();
        Ok(Response { status, retry_after, body: Box::new(StreamBody(stream)) })
    }

    fn get(&self, path: &str) -> Result<Response, TransportError> {
        let target = Self::target(path)?;
        let fetched = self.client.fetch(&target).map_err(map_fetch)?;
        Ok(Response {
            status: fetched.status,
            retry_after: None,
            body: Box::new(std::io::Cursor::new(fetched.bytes)),
        })
    }

    fn host(&self) -> &str {
        OPENROUTER_HOST
    }
}

/// Adapts `marlowe_net::ResponseStream` to `BufRead`.
struct StreamBody(marlowe_net::ResponseStream);

impl Read for StreamBody {
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

/// One scripted reply.
#[derive(Debug, Clone)]
pub struct Reply {
    pub status: u16,
    pub retry_after: Option<String>,
    pub body: String,
    /// After the body, block this long before EOF (a silent upstream).
    pub stall: Option<Duration>,
}

impl Reply {
    pub fn ok(body: &str) -> Self {
        Self { status: 200, retry_after: None, body: body.to_string(), stall: None }
    }
    pub fn status(status: u16, body: &str) -> Self {
        Self { status, retry_after: None, body: body.to_string(), stall: None }
    }
    pub fn rate_limited(retry_after: Option<&str>) -> Self {
        Self {
            status: 429,
            retry_after: retry_after.map(str::to_string),
            body: "{\"error\":{\"message\":\"rate limited\"}}".into(),
            stall: None,
        }
    }
    /// A 200 that sends `body` and then goes silent for `stall`.
    pub fn stalled(body: &str, stall: Duration) -> Self {
        Self { status: 200, retry_after: None, body: body.to_string(), stall: Some(stall) }
    }
}

/// What one call was handed. Headers are recorded so tests can prove where the key went.
#[derive(Debug, Clone, Default)]
pub struct Seen {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

/// Answers from a script and records what it was asked. Replaces only the socket.
pub struct ScriptedTransport {
    replies: Mutex<Vec<Reply>>,
    /// Returned once the script runs out; `None` means a transport fault instead.
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

    /// Answer with `reply` forever once the script is spent.
    pub fn then_forever(mut self, reply: Reply) -> Self {
        self.exhausted = Some(reply);
        self
    }

    /// What the provider sent, readable after the call.
    pub fn seen(&self) -> Arc<Mutex<Vec<Seen>>> {
        Arc::clone(&self.seen)
    }

    fn answer(&self, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Result<Response, TransportError> {
        self.seen.lock().unwrap_or_else(|e| e.into_inner()).push(Seen {
            method: method.to_string(),
            path: path.to_string(),
            headers: headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            body: String::from_utf8_lossy(body).into_owned(),
        });
        let next = self
            .replies
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop()
            .or_else(|| self.exhausted.clone());
        match next {
            Some(r) => Ok(Response {
                status: r.status,
                retry_after: r.retry_after,
                body: Box::new(std::io::BufReader::new(Stalling {
                    data: std::io::Cursor::new(r.body.into_bytes()),
                    stall: r.stall,
                })),
            }),
            None => Err(TransportError::Unreachable {
                host: OPENROUTER_HOST.into(),
                detail: "the script is exhausted".into(),
            }),
        }
    }
}

impl Transport for ScriptedTransport {
    fn post(&self, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Result<Response, TransportError> {
        self.answer("POST", path, headers, body)
    }

    fn get(&self, path: &str) -> Result<Response, TransportError> {
        self.answer("GET", path, &[], &[])
    }

    fn host(&self) -> &str {
        OPENROUTER_HOST
    }
}

/// A body that blocks once drained, like an upstream that stopped talking.
struct Stalling {
    data: std::io::Cursor<Vec<u8>>,
    stall: Option<Duration>,
}

impl Read for Stalling {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.data.read(buf)?;
        if n == 0 {
            if let Some(d) = self.stall.take() {
                std::thread::sleep(d);
            }
        }
        Ok(n)
    }
}
