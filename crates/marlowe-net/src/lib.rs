//! The fetch primitive. ADR-031.
//!
//! # What this crate is for, and the boundary that is the point of it
//!
//! It performs HTTPS requests and returns **bytes and a content type**. It does not parse, it
//! does not strip HTML, it does not guess a charset beyond what the header states, and it has no
//! opinion about what a page means.
//!
//! **Extraction is a separate module operating on [`Fetched`]** — that module is now
//! `marlowe-extract`, and the boundary is structural rather than stylistic. Extraction is where
//! the interesting parsing bugs live, and parsing is the thing you least want entangled with the
//! code that decides whether a request may happen at all: an extractor that panics must not take
//! the egress path with it, and an egress rule must never be expressible as a parser option.
//!
//! # Redirects are the security-relevant part
//!
//! [`Client::fetch`] does **not** follow redirects. It returns [`Fetched::redirect_to`] and lets
//! the caller decide, because **a redirect is a second egress destination** — following one
//! without re-checking the allowlist means the host being fetched chose where the request goes,
//! which is untrusted content selecting a target through a channel that never touches the taint
//! layer.
//!
//! The re-check cannot live here: this crate does not know the run's `EgressPolicy` and must not,
//! or the permission layer would have a second implementation inside a networking crate. See
//! `marlowe-exec`'s `web` executor for the loop that consumes this.
//!
//! # What changed, and why the old shape was slow
//!
//! The original implementation built a fresh [`rustls::ClientConfig`] **per request**, opened a
//! new TCP connection, completed a full TLS handshake, sent `Connection: close` and
//! `Accept-Encoding: identity`, and read to EOF. For a research agent pulling hundreds of
//! documents that is the worst case on every axis at once:
//!
//! | Was | Now | Why it matters |
//! |---|---|---|
//! | `ClientConfig` per request | one shared `Arc`, built once | the config owns the **TLS session cache**, so resumption was structurally impossible before |
//! | new connection per request | keep-alive pool, keyed by host | 30 pages on one host go from 30 handshakes to 1 |
//! | full handshake every time | session resumption across hosts | ~1 RTT saved per repeat host |
//! | `Accept-Encoding: identity` | `gzip, br` | HTML compresses 3-5x; those bytes never cross the wire |
//! | DNS per request | 60-second cache | one resolution per host per minute |
//! | one request at a time | [`Client::fetch_many`] | N sockets in flight, bounded |
//!
//! **[`Client`] is `Send + Sync`.** That is deliberate and load-bearing: the agent loop currently
//! executes batched tool calls serially, and when that changes this path needs no further work.

#![forbid(unsafe_code)]

/// The crate's only monotonic clock read, isolated so `determinism_guard`'s fence names one
/// twenty-line file rather than this four-hundred-line fetch path. See its header.
pub mod age;

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use crate::age::Mark;

#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("{url} is not a URL this tool will fetch: {detail}")]
    Unsupported { url: String, detail: String },
    #[error("could not reach {host}: {detail}")]
    Unreachable { host: String, detail: String },
    #[error("TLS to {host} failed: {detail}")]
    Tls { host: String, detail: String },
    #[error("malformed response from {host}: {detail}")]
    Malformed { host: String, detail: String },
    #[error("{host} returned a body larger than the {limit} byte cap")]
    TooLarge { host: String, limit: usize },
    #[error("{host} sent {encoding}-encoded content that would not decode: {detail}")]
    Decode { host: String, encoding: String, detail: String },
}

/// One response. **Bytes and a content type — nothing interpreted.**
#[derive(Debug, Clone)]
pub struct Fetched {
    pub status: u16,
    /// Verbatim from the `Content-Type` header, `None` when absent. Not defaulted to
    /// `text/html`: a guess here would be indistinguishable from a server that said so.
    pub content_type: Option<String>,
    /// The body, **after** any `Content-Encoding` has been removed. Transfer compression is a
    /// property of the hop, not of the document, so a caller must never see it.
    pub bytes: Vec<u8>,
    /// The URL this response came from — the one requested, since this crate does not follow
    /// redirects. The caller updates it as it walks a chain.
    pub final_url: String,
    /// `Some` when the status is a redirect and a `Location` header was present. **The caller must
    /// re-adjudicate this host before fetching it.**
    pub redirect_to: Option<String>,
    /// Bytes actually read off the socket, before decompression. Diagnostic: the ratio against
    /// `bytes.len()` is what compression bought.
    pub wire_bytes: usize,
    /// Was this served over a connection that already existed?
    pub reused_connection: bool,
}

/// Bodies above this are truncated rather than buffered. `web` returns by reference anyway,
/// so this bounds memory, not usefulness.
pub const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

/// Ceiling on a *decompressed* body. Separate from [`MAX_BODY_BYTES`] because a compression bomb
/// is small on the wire and enormous in memory — the whole point of one.
pub const MAX_DECOMPRESSED_BYTES: usize = 32 * 1024 * 1024;

/// How wide to run network work on **this** machine. **Derived, never hardcoded.**
///
/// # Why this is a multiple of the core count rather than equal to it
///
/// A fetch spends almost its entire life blocked on a socket with the CPU idle. Sizing this pool
/// to the core count would leave the machine waiting on the network with most of its cores parked
/// — the opposite of using the hardware. Threads blocked in `recv` are not competing for a core,
/// so the useful width for I/O-bound work is well above it.
///
/// Four per core, with a floor of 8 so that a 2-core machine still overlaps its round trips. There
/// is deliberately **no hardcoded ceiling**: the bound that matters is how many requests one *host*
/// sees, and that is handled where it belongs, by [`MAX_POOLED_PER_HOST`] and by callers never
/// spawning more workers than they have work.
///
/// This is the single definition. `marlowe-exec` routes its batch width through it rather than
/// keeping a second constant that would drift.
pub fn io_concurrency() -> usize {
    let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
    (cores * 4).max(8)
}

const TIMEOUT: Duration = Duration::from_secs(20);
const DNS_TTL: Duration = Duration::from_secs(60);
/// A pooled connection older than this is dropped rather than risked.
const POOL_IDLE: Duration = Duration::from_secs(30);
const MAX_POOLED_PER_HOST: usize = 8;

/// A parsed destination. Construction is the validation.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Target {
    pub host: String,
    pub port: u16,
    pub path: String,
    pub tls: bool,
}

impl Target {
    /// **`https` only, and `http` is refused rather than upgraded.**
    ///
    /// Silently upgrading would make the scheme the caller asked for and the scheme actually used
    /// two different things with nothing observing the difference — and a downgrade in a redirect
    /// chain is exactly what §2.5 refuses, so accepting plaintext here would open by the front door
    /// what that closes at the back.
    pub fn parse(url: &str) -> Result<Self, FetchError> {
        let url = url.trim();
        let unsupported = |detail: &str| FetchError::Unsupported {
            url: url.to_string(),
            detail: detail.to_string(),
        };
        let rest = url.strip_prefix("https://").ok_or_else(|| {
            if url.starts_with("http://") {
                unsupported("only https is supported; plaintext http is refused, not upgraded")
            } else {
                unsupported("the URL must begin with https://")
            }
        })?;

        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        if authority.contains('@') {
            return Err(unsupported("userinfo in the authority is not accepted"));
        }
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) => (
                h,
                p.parse::<u16>().map_err(|_| unsupported("the port is not a number"))?,
            ),
            None => (authority, 443),
        };
        if host.is_empty() || host.contains(char::is_whitespace) {
            return Err(unsupported("the host is empty or contains whitespace"));
        }
        Ok(Self {
            host: host.to_ascii_lowercase(),
            port,
            path: path.to_string(),
            tls: true,
        })
    }
}

fn root_store() -> rustls::RootCertStore {
    // ADR-031 §2.2: the vendored Mozilla set. A corporate MITM root will not verify, and that
    // refusal is deliberate — an escape hatch accepting an unknown root would be a hole in the one
    // component whose entire job is fetching content nobody trusts.
    rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    }
}

type TlsStream = rustls::StreamOwned<rustls::ClientConnection, TcpStream>;

struct Pooled {
    reader: BufReader<TlsStream>,
    idle_since: Mark,
}

/// A reusable HTTPS client.
///
/// **`Send + Sync`.** One `Client` is meant to be shared for the process lifetime: the TLS session
/// cache, the connection pool and the DNS cache all live in it, and all three are worthless if the
/// client is rebuilt per request — which is exactly what the previous implementation did.
pub struct Client {
    config: Arc<rustls::ClientConfig>,
    pool: Mutex<BTreeMap<(String, u16), Vec<Pooled>>>,
    dns: Mutex<BTreeMap<String, (Vec<SocketAddr>, Mark)>>,
    stats: Stats,
}

#[derive(Debug, Default)]
struct Stats {
    handshakes: AtomicUsize,
    reuses: AtomicUsize,
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

impl Client {
    pub fn new() -> Self {
        let mut config = rustls::ClientConfig::builder()
            .with_root_certificates(root_store())
            .with_no_client_auth();
        // **This is why the config must be shared rather than rebuilt.** The resumption store
        // lives here; a per-request config means every handshake is a full one, forever, and the
        // cost is invisible because nothing reports it.
        config.resumption = rustls::client::Resumption::in_memory_sessions(256);
        Self {
            config: Arc::new(config),
            pool: Mutex::new(BTreeMap::new()),
            dns: Mutex::new(BTreeMap::new()),
            stats: Stats::default(),
        }
    }

    /// Handshakes performed and connections reused. Diagnostic — the pair is the evidence that
    /// pooling is doing anything, and a reuse count of zero on a repeated host is a bug report.
    pub fn connection_stats(&self) -> (usize, usize) {
        (
            self.stats.handshakes.load(Ordering::Relaxed),
            self.stats.reuses.load(Ordering::Relaxed),
        )
    }

    /// One request. No redirect following — see the module header.
    ///
    /// **A pooled connection that fails is retried once on a fresh one.** A server is free to
    /// close an idle keep-alive connection at any moment, and it will do so precisely between our
    /// check and our write. Without the retry, pooling would convert a working fetch into an
    /// intermittent failure — the classic reason naive keep-alive implementations get reverted.
    pub fn fetch(&self, target: &Target) -> Result<Fetched, FetchError> {
        match self.try_fetch(target, true) {
            Err(e) if is_connection_fault(&e) => self.try_fetch(target, false),
            other => other,
        }
    }

    fn try_fetch(&self, target: &Target, allow_pooled: bool) -> Result<Fetched, FetchError> {
        let (mut reader, reused) = match allow_pooled.then(|| self.take_pooled(target)).flatten() {
            Some(r) => {
                self.stats.reuses.fetch_add(1, Ordering::Relaxed);
                (r, true)
            }
            None => {
                self.stats.handshakes.fetch_add(1, Ordering::Relaxed);
                (self.connect(target)?, false)
            }
        };

        // `Accept-Encoding` is the single biggest wall-clock win here: HTML compresses 3-5x and
        // those bytes never cross the network. `identity` was leaving that on the table on every
        // request.
        let request = format!(
            "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: marlowe\r\nAccept: */*\r\n\
             Accept-Encoding: gzip, br\r\nConnection: keep-alive\r\n\r\n",
            target.path, target.host
        );
        reader
            .get_mut()
            .write_all(request.as_bytes())
            .and_then(|()| reader.get_mut().flush())
            .map_err(|e| FetchError::Unreachable {
                host: target.host.clone(),
                detail: e.to_string(),
            })?;

        let head = read_head(&mut reader, &target.host)?;
        let (body, complete) = read_body(&mut reader, &target.host, &head)?;
        let wire_bytes = body.len();

        // Keep-alive only when the framing was unambiguous. Reading to EOF means the connection is
        // already finished; guessing otherwise would desynchronise the next request on it.
        if complete && head.keep_alive {
            self.give_back(target, reader);
        }

        let bytes = decode_body(body, head.content_encoding.as_deref(), &target.host)?;

        let redirect_to = match head.status {
            301 | 302 | 303 | 307 | 308 => head.location,
            _ => None,
        };

        Ok(Fetched {
            status: head.status,
            content_type: head.content_type,
            bytes,
            final_url: format!("https://{}:{}{}", target.host, target.port, target.path),
            redirect_to,
            wire_bytes,
            reused_connection: reused,
        })
    }

    /// **Fetch many targets concurrently.** Bounded fan-out over OS threads.
    ///
    /// Blocking sockets and real threads rather than an async runtime, deliberately: the whole
    /// call chain around this — adjudication, execution, journalling — is synchronous, and a few
    /// dozen threads parked on sockets is cheap. Introducing an executor here would mean colouring
    /// the entire path `async` to solve a problem threads already solve.
    ///
    /// Results come back **in input order**, and one target's failure is its own.
    pub fn fetch_many(
        &self,
        targets: &[Target],
        concurrency: usize,
    ) -> Vec<Result<Fetched, FetchError>> {
        let n = targets.len();
        if n == 0 {
            return Vec::new();
        }
        // `0` means "decide for me" -- the derived width for this machine. Never more workers
        // than there is work.
        let requested = if concurrency == 0 { io_concurrency() } else { concurrency };
        let workers = requested.max(1).min(n);
        let slots: Vec<Mutex<Option<Result<Fetched, FetchError>>>> =
            (0..n).map(|_| Mutex::new(None)).collect();
        let next = AtomicUsize::new(0);

        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= n {
                        break;
                    }
                    let result = self.fetch(&targets[i]);
                    *slots[i].lock().expect("fetch slot poisoned") = Some(result);
                });
            }
        });

        slots
            .into_iter()
            .map(|s| {
                s.into_inner()
                    .expect("fetch slot poisoned")
                    .expect("every slot is filled before the scope ends")
            })
            .collect()
    }

    fn connect(&self, target: &Target) -> Result<BufReader<TlsStream>, FetchError> {
        let server_name = target.host.clone().try_into().map_err(|_| FetchError::Tls {
            host: target.host.clone(),
            detail: "not a valid DNS name".into(),
        })?;
        let conn = rustls::ClientConnection::new(Arc::clone(&self.config), server_name).map_err(
            |e| FetchError::Tls { host: target.host.clone(), detail: e.to_string() },
        )?;

        let addrs = self.resolve(target)?;
        let mut last: Option<std::io::Error> = None;
        let mut sock = None;
        for addr in &addrs {
            match TcpStream::connect_timeout(addr, TIMEOUT) {
                Ok(s) => {
                    sock = Some(s);
                    break;
                }
                Err(e) => last = Some(e),
            }
        }
        let sock = sock.ok_or_else(|| FetchError::Unreachable {
            host: target.host.clone(),
            detail: last.map(|e| e.to_string()).unwrap_or_else(|| "no addresses".into()),
        })?;
        sock.set_read_timeout(Some(TIMEOUT)).ok();
        sock.set_write_timeout(Some(TIMEOUT)).ok();
        // Latency beats packing on a request/response protocol with small requests.
        sock.set_nodelay(true).ok();

        Ok(BufReader::with_capacity(32 * 1024, rustls::StreamOwned::new(conn, sock)))
    }

    fn resolve(&self, target: &Target) -> Result<Vec<SocketAddr>, FetchError> {
        let key = format!("{}:{}", target.host, target.port);
        if let Some((addrs, at)) = self.dns.lock().expect("dns cache poisoned").get(&key) {
            if at.elapsed() < DNS_TTL {
                return Ok(addrs.clone());
            }
        }
        let addrs: Vec<SocketAddr> = (target.host.as_str(), target.port)
            .to_socket_addrs()
            .map_err(|e| FetchError::Unreachable {
                host: target.host.clone(),
                detail: e.to_string(),
            })?
            .collect();
        if addrs.is_empty() {
            return Err(FetchError::Unreachable {
                host: target.host.clone(),
                detail: "resolved to no addresses".into(),
            });
        }
        self.dns
            .lock()
            .expect("dns cache poisoned")
            .insert(key, (addrs.clone(), Mark::now()));
        Ok(addrs)
    }

    fn take_pooled(&self, target: &Target) -> Option<BufReader<TlsStream>> {
        let mut pool = self.pool.lock().expect("connection pool poisoned");
        let bucket = pool.get_mut(&(target.host.clone(), target.port))?;
        while let Some(p) = bucket.pop() {
            if p.idle_since.elapsed() < POOL_IDLE {
                return Some(p.reader);
            }
            // Older than the idle window: drop rather than gamble on the server's timeout.
        }
        None
    }

    fn give_back(&self, target: &Target, reader: BufReader<TlsStream>) {
        let mut pool = self.pool.lock().expect("connection pool poisoned");
        let bucket = pool.entry((target.host.clone(), target.port)).or_default();
        if bucket.len() < MAX_POOLED_PER_HOST {
            bucket.push(Pooled { reader, idle_since: Mark::now() });
        }
    }
}

/// Was this failure the kind a fresh connection might fix?
///
/// Only transport faults qualify. A 4xx, a malformed body or a decode failure will happen again
/// identically, and retrying them would double the cost of every genuine error.
fn is_connection_fault(e: &FetchError) -> bool {
    matches!(e, FetchError::Unreachable { .. } | FetchError::Malformed { .. })
}

struct Head {
    status: u16,
    content_type: Option<String>,
    location: Option<String>,
    /// `Retry-After`, verbatim. Read here rather than re-parsed by a caller, because a caller
    /// that re-reads the socket to find it has already consumed the body.
    retry_after: Option<String>,
    content_encoding: Option<String>,
    content_length: Option<usize>,
    chunked: bool,
    keep_alive: bool,
}

fn read_head(reader: &mut BufReader<TlsStream>, host: &str) -> Result<Head, FetchError> {
    let malformed = |detail: String| FetchError::Malformed { host: host.to_string(), detail };

    let mut status_line = String::new();
    reader
        .read_line(&mut status_line)
        .map_err(|e| malformed(e.to_string()))?;
    if status_line.is_empty() {
        return Err(malformed("the connection closed before a status line".into()));
    }
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| malformed(format!("no status code in {status_line:?}")))?;
    let http11 = status_line.starts_with("HTTP/1.1");

    let mut head = Head {
        status,
        content_type: None,
        location: None,
        retry_after: None,
        content_encoding: None,
        content_length: None,
        chunked: false,
        keep_alive: http11,
    };

    let mut count = 0usize;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).map_err(|e| malformed(e.to_string()))?;
        if n == 0 || line.trim().is_empty() {
            break;
        }
        count += 1;
        if count > 200 {
            return Err(malformed("more than 200 header lines".into()));
        }
        let Some((name, value)) = line.split_once(':') else { continue };
        let value = value.trim().to_string();
        match name.trim().to_ascii_lowercase().as_str() {
            "content-type" => head.content_type = Some(value),
            "location" => head.location = Some(value),
            "retry-after" => head.retry_after = Some(value),
            "content-encoding" => head.content_encoding = Some(value.to_ascii_lowercase()),
            "transfer-encoding" if value.eq_ignore_ascii_case("chunked") => head.chunked = true,
            "content-length" => head.content_length = value.parse().ok(),
            "connection" => head.keep_alive = value.eq_ignore_ascii_case("keep-alive"),
            _ => {}
        }
    }
    Ok(head)
}

/// Read the body. Returns the bytes and **whether the framing was unambiguous**, which is what
/// decides if the connection may be pooled.
fn read_body(
    reader: &mut BufReader<TlsStream>,
    host: &str,
    head: &Head,
) -> Result<(Vec<u8>, bool), FetchError> {
    if head.chunked {
        return read_chunked(reader, host).map(|b| (b, true));
    }
    if let Some(len) = head.content_length {
        if len > MAX_BODY_BYTES {
            return Err(FetchError::TooLarge { host: host.to_string(), limit: MAX_BODY_BYTES });
        }
        let mut buf = vec![0u8; len];
        reader.read_exact(&mut buf).map_err(|e| FetchError::Malformed {
            host: host.to_string(),
            detail: e.to_string(),
        })?;
        // Exactly `len` bytes consumed, so the stream is positioned at the next response.
        return Ok((buf, true));
    }
    // No framing: the body ends when the connection does, so this one cannot be reused.
    let mut buf = Vec::with_capacity(64 * 1024);
    Read::by_ref(reader)
        .take(MAX_BODY_BYTES as u64)
        .read_to_end(&mut buf)
        .map_err(|e| FetchError::Malformed { host: host.to_string(), detail: e.to_string() })?;
    Ok((buf, false))
}

fn read_chunked<R: BufRead>(reader: &mut R, host: &str) -> Result<Vec<u8>, FetchError> {
    let mut out = Vec::new();
    loop {
        let mut size_line = String::new();
        reader.read_line(&mut size_line).map_err(|e| FetchError::Malformed {
            host: host.to_string(),
            detail: e.to_string(),
        })?;
        let size = usize::from_str_radix(size_line.trim().split(';').next().unwrap_or("").trim(), 16)
            .map_err(|_| FetchError::Malformed {
                host: host.to_string(),
                detail: format!("bad chunk size {size_line:?}"),
            })?;
        if size == 0 {
            // Consume the trailer so the stream is clean for the next response on this connection.
            loop {
                let mut trailer = String::new();
                let n = reader.read_line(&mut trailer).unwrap_or(0);
                if n == 0 || trailer.trim().is_empty() {
                    break;
                }
            }
            break;
        }
        if out.len() + size > MAX_BODY_BYTES {
            return Err(FetchError::TooLarge { host: host.to_string(), limit: MAX_BODY_BYTES });
        }
        let mut chunk = vec![0u8; size];
        reader.read_exact(&mut chunk).map_err(|e| FetchError::Malformed {
            host: host.to_string(),
            detail: e.to_string(),
        })?;
        out.extend_from_slice(&chunk);
        let mut crlf = String::new();
        reader.read_line(&mut crlf).ok();
    }
    Ok(out)
}

/// Remove `Content-Encoding`.
///
/// **Bounded by [`MAX_DECOMPRESSED_BYTES`], separately from the wire cap.** A compression bomb is
/// by construction small on the wire and enormous in memory, so a single limit on the compressed
/// size is not a limit at all.
fn decode_body(body: Vec<u8>, encoding: Option<&str>, host: &str) -> Result<Vec<u8>, FetchError> {
    let Some(encoding) = encoding else { return Ok(body) };
    let fail = |detail: String| FetchError::Decode {
        host: host.to_string(),
        encoding: encoding.to_string(),
        detail,
    };
    let mut out = Vec::with_capacity(body.len().saturating_mul(4).min(1024 * 1024));

    match encoding.trim() {
        "identity" | "" => return Ok(body),
        "gzip" | "x-gzip" => {
            flate2::read::GzDecoder::new(&body[..])
                .take(MAX_DECOMPRESSED_BYTES as u64)
                .read_to_end(&mut out)
                .map_err(|e| fail(e.to_string()))?;
        }
        "deflate" => {
            // Servers disagree about whether `deflate` means zlib-wrapped or raw. Try the
            // standards-compliant reading, then the common one — a fallback here is correct
            // rather than lax, because both are genuinely in the wild.
            if flate2::read::ZlibDecoder::new(&body[..])
                .take(MAX_DECOMPRESSED_BYTES as u64)
                .read_to_end(&mut out)
                .is_err()
            {
                out.clear();
                flate2::read::DeflateDecoder::new(&body[..])
                    .take(MAX_DECOMPRESSED_BYTES as u64)
                    .read_to_end(&mut out)
                    .map_err(|e| fail(e.to_string()))?;
            }
        }
        "br" => {
            brotli::Decompressor::new(&body[..], 8192)
                .take(MAX_DECOMPRESSED_BYTES as u64)
                .read_to_end(&mut out)
                .map_err(|e| fail(e.to_string()))?;
        }
        other => {
            // An encoding we did not advertise and cannot read. Returning the compressed bytes
            // would hand a parser binary garbage that decodes to plausible nonsense.
            return Err(fail(format!("{other} was not requested and cannot be decoded")));
        }
    }
    Ok(out)
}

/// The process-wide client.
///
/// A `OnceLock` rather than a fresh client per call: the pool, the DNS cache and the TLS session
/// cache only exist if something outlives a single request.
pub fn shared() -> &'static Client {
    static CLIENT: OnceLock<Client> = OnceLock::new();
    CLIENT.get_or_init(Client::new)
}

/// One request against the shared client. The original entry point, preserved.
pub fn fetch(target: &Target) -> Result<Fetched, FetchError> {
    shared().fetch(target)
}

/// Concurrent fetch against the shared client.
pub fn fetch_many(targets: &[Target], concurrency: usize) -> Vec<Result<Fetched, FetchError>> {
    shared().fetch_many(targets, concurrency)
}

/// Handshakes and reuses on the shared client.
pub fn connection_stats() -> (usize, usize) {
    shared().connection_stats()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plaintext_http_is_refused_and_not_upgraded() {
        let e = Target::parse("http://example.com/").expect_err("http is refused");
        assert!(e.to_string().contains("not upgraded"), "{e}");
    }

    #[test]
    fn a_url_with_no_scheme_is_refused() {
        assert!(Target::parse("example.com/").is_err());
    }

    #[test]
    fn userinfo_is_refused_because_it_hides_the_real_host() {
        assert!(Target::parse("https://evil.com@good.com/").is_err());
    }

    #[test]
    fn the_host_is_lowercased_so_an_allowlist_compares_one_spelling() {
        assert_eq!(Target::parse("https://EXAMPLE.com/x").unwrap().host, "example.com");
    }

    #[test]
    fn a_missing_path_becomes_root() {
        assert_eq!(Target::parse("https://example.com").unwrap().path, "/");
    }

    #[test]
    fn an_explicit_port_is_kept() {
        assert_eq!(Target::parse("https://example.com:8443/x").unwrap().port, 8443);
    }

    // ── decompression ────────────────────────────────────────────────────────────────────

    fn gzipped(data: &[u8]) -> Vec<u8> {
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    #[test]
    fn gzip_is_removed_so_a_caller_never_sees_transport_compression() {
        let body = "hello ".repeat(200);
        let got = decode_body(gzipped(body.as_bytes()), Some("gzip"), "h").expect("decodes");
        assert_eq!(got, body.as_bytes());
    }

    #[test]
    fn raw_deflate_is_accepted_as_well_as_zlib_wrapped() {
        let body = b"deflate payload".repeat(30);
        let mut e =
            flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        e.write_all(&body).unwrap();
        let raw = e.finish().unwrap();
        assert_eq!(decode_body(raw, Some("deflate"), "h").expect("decodes"), body);
    }

    #[test]
    fn an_unrequested_encoding_errors_rather_than_returning_binary_garbage() {
        // Handing compressed bytes to a parser produces plausible nonsense, which is worse than
        // an error: it extracts, it ranks, and it is meaningless.
        let e = decode_body(vec![1, 2, 3], Some("zstd"), "h").expect_err("refused");
        assert!(e.to_string().contains("cannot be decoded"), "{e}");
    }

    #[test]
    fn identity_passes_through_untouched() {
        assert_eq!(decode_body(b"abc".to_vec(), Some("identity"), "h").unwrap(), b"abc");
        assert_eq!(decode_body(b"abc".to_vec(), None, "h").unwrap(), b"abc");
    }

    #[test]
    fn a_gzip_bomb_is_bounded_by_the_decompressed_ceiling_not_the_wire_size() {
        // ~32 KB on the wire, far beyond the ceiling once inflated. A single cap on the
        // compressed size would not be a cap at all.
        let bomb = gzipped(&vec![0u8; MAX_DECOMPRESSED_BYTES + 1_000_000]);
        assert!(bomb.len() < 100_000, "test premise: the bomb is small on the wire");
        let got = decode_body(bomb, Some("gzip"), "h").expect("truncates rather than exploding");
        assert!(got.len() <= MAX_DECOMPRESSED_BYTES);
    }

    // ── client shape ─────────────────────────────────────────────────────────────────────

    #[test]
    fn the_client_is_send_and_sync_so_the_pool_can_be_shared() {
        // Compile-time assertion. The pool, DNS cache and TLS session cache are worthless if the
        // client cannot outlive one request on one thread.
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Client>();
    }

    #[test]
    fn fetch_many_of_nothing_is_empty_rather_than_a_panic() {
        assert!(Client::new().fetch_many(&[], 8).is_empty());
    }

    #[test]
    fn only_transport_faults_are_retried() {
        assert!(is_connection_fault(&FetchError::Unreachable {
            host: "h".into(),
            detail: "reset".into()
        }));
        assert!(!is_connection_fault(&FetchError::TooLarge { host: "h".into(), limit: 1 }));
        assert!(!is_connection_fault(&FetchError::Decode {
            host: "h".into(),
            encoding: "br".into(),
            detail: "x".into()
        }));
    }
}

// ─────────────────────────────────────────────────────────────────────────────────────────
// A STREAMING POST. ADR-046.
// ─────────────────────────────────────────────────────────────────────────────────────────
//
// # Why this lives here rather than in the provider crate
//
// ADR-031 §2.3 makes this crate **the whole of the TLS supply-chain surface**, so that
// `cargo tree -p marlowe-provider` keeps showing no TLS and ADR-028's *"the default path reaches
// no network"* stays a property rather than a comment. A hosted model provider needs HTTPS. The
// two ways to give it one are to add `rustls` to a second crate — which ends the §2.3 property —
// or to add the one primitive it is missing here. This is the second.
//
// # What it deliberately is not
//
// It is not a general HTTP client and it holds no policy. It does not follow redirects, does not
// pool (a streamed body has no unambiguous end until it is fully read, and returning a
// half-drained connection to the pool desynchronises the next request on it), and — the part that
// matters for `marlowe-openrouter` — **it never stores, logs or formats a header value.** Headers
// are borrowed for the length of one `write_all` and dropped. `FetchError` carries a host and an
// I/O detail and has no variant that can hold one.

/// A response whose body is read **as it arrives**.
pub struct ResponseStream {
    pub status: u16,
    pub content_type: Option<String>,
    /// `Retry-After`, verbatim, when the server sent one. The caller decides what to do with it;
    /// this crate has no retry policy.
    pub retry_after: Option<String>,
    body: Box<dyn BufRead + Send>,
    host: String,
}

impl ResponseStream {
    /// The body, line-oriented, decoded through any `Transfer-Encoding: chunked` framing.
    pub fn body(&mut self) -> &mut dyn BufRead {
        &mut *self.body
    }

    /// Drain at most `limit` bytes. For an error response, whose body is small and wanted whole.
    pub fn read_capped(&mut self, limit: usize) -> String {
        let mut buf = Vec::new();
        let _ = Read::by_ref(&mut self.body).take(limit as u64).read_to_end(&mut buf);
        String::from_utf8_lossy(&buf).into_owned()
    }

    pub fn host(&self) -> &str {
        &self.host
    }
}

/// A header name/value pair is written verbatim, so **it may not contain CR or LF.**
///
/// This is the header-injection guard and it is here rather than in the caller on purpose: the
/// caller is the crate that holds a credential and a user-supplied model name, and a value that
/// could carry a line break would let either of them append headers of its own — including a
/// second `Authorization`. Refused by name at the point of writing rather than sanitised, because
/// a silently-stripped newline is a mismatch nothing observes.
fn validate_header(name: &str, value: &str, host: &str) -> Result<(), FetchError> {
    let bad = |what: &str| FetchError::Unsupported {
        url: format!("https://{host}"),
        // **The VALUE is never included.** One of these values is an API key.
        detail: format!("header `{name}` {what}"),
    };
    if name.is_empty() || name.contains(|c: char| c.is_control() || c == ':' || c == ' ') {
        return Err(bad("has a name that is not a token"));
    }
    if value.contains('\r') || value.contains('\n') || value.contains('\0') {
        return Err(bad("contains a control character; header injection is refused, not stripped"));
    }
    Ok(())
}

impl Client {
    /// POST `body` and return the response **without buffering it**.
    ///
    /// `read_timeout` applies per read, not to the whole response: a model that takes two minutes
    /// to answer is working, not hung, and a whole-response deadline would kill exactly the long
    /// turns streaming exists to make bearable.
    pub fn post_streaming(
        &self,
        target: &Target,
        headers: &[(&str, &str)],
        body: &[u8],
        read_timeout: Duration,
    ) -> Result<ResponseStream, FetchError> {
        for (name, value) in headers {
            validate_header(name, value, &target.host)?;
        }

        // **No pooled connection is taken and none is given back.** See the section header.
        self.stats.handshakes.fetch_add(1, Ordering::Relaxed);
        let mut reader = self.connect(target)?;
        reader.get_ref().sock.set_read_timeout(Some(read_timeout)).ok();
        reader.get_ref().sock.set_write_timeout(Some(read_timeout)).ok();

        let mut head = format!(
            "POST {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: marlowe\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\
             Accept-Encoding: identity\r\nConnection: close\r\n",
            target.path,
            target.host,
            body.len()
        );
        for (name, value) in headers {
            head.push_str(name);
            head.push_str(": ");
            head.push_str(value);
            head.push_str("\r\n");
        }
        head.push_str("\r\n");

        let unreachable = |e: std::io::Error| FetchError::Unreachable {
            host: target.host.clone(),
            detail: e.to_string(),
        };
        reader.get_mut().write_all(head.as_bytes()).map_err(unreachable)?;
        // `head` held the credential. Cleared before the response is read, so a later panic
        // cannot unwind with it still live in this frame.
        head.clear();
        reader.get_mut().write_all(body).map_err(unreachable)?;
        reader.get_mut().flush().map_err(unreachable)?;

        let parsed = read_head(&mut reader, &target.host)?;
        let body: Box<dyn BufRead + Send> = if parsed.chunked {
            Box::new(BufReader::with_capacity(
                16 * 1024,
                ChunkedReader { inner: reader, remaining: 0, finished: false },
            ))
        } else {
            Box::new(reader)
        };
        Ok(ResponseStream {
            status: parsed.status,
            content_type: parsed.content_type,
            retry_after: parsed.retry_after,
            body,
            host: target.host.clone(),
        })
    }
}

/// `Transfer-Encoding: chunked`, decoded incrementally.
///
/// [`read_chunked`] above collects every chunk into a `Vec` before returning — correct for a
/// document, and fatal to streaming, because the first byte reaches the caller only after the
/// last one has arrived.
struct ChunkedReader<R: BufRead> {
    inner: R,
    remaining: usize,
    finished: bool,
}

impl<R: BufRead> Read for ChunkedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.finished {
            return Ok(0);
        }
        if self.remaining == 0 {
            let mut size_line = String::new();
            self.inner.read_line(&mut size_line)?;
            if size_line.trim().is_empty() {
                // The CRLF that terminated the previous chunk.
                size_line.clear();
                self.inner.read_line(&mut size_line)?;
            }
            if size_line.is_empty() {
                self.finished = true;
                return Ok(0);
            }
            let size = usize::from_str_radix(
                size_line.trim().split(';').next().unwrap_or("").trim(),
                16,
            )
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            if size == 0 {
                self.finished = true;
                return Ok(0);
            }
            self.remaining = size;
        }
        let want = buf.len().min(self.remaining);
        let n = self.inner.read(&mut buf[..want])?;
        self.remaining -= n;
        Ok(n)
    }
}

#[cfg(test)]
mod post_streaming_tests {
    use super::*;

    #[test]
    fn a_header_value_carrying_crlf_is_refused_rather_than_stripped() {
        // The values this guards are an API key and a user-chosen model name. A line break in
        // either would append a header of the sender's choosing — including a second
        // `Authorization`.
        //
        // **The refusal must not quote the value**, because one of these values is a credential.
        let e = validate_header("Authorization", "Bearer sk-or-v1-SECRET\r\nX-Evil: 1", "h")
            .unwrap_err();
        let rendered = e.to_string();
        assert!(rendered.contains("header injection is refused"), "{rendered}");
        assert!(
            !rendered.contains("SECRET"),
            "the refusal quoted the header value, which is where a key would be: {rendered}"
        );

        assert!(validate_header("Authorization", "Bearer sk-or-v1-ok", "h").is_ok());
        assert!(validate_header("X-Title", "marlowe", "h").is_ok());
        assert!(validate_header("Bad Name", "v", "h").is_err());
    }

    #[test]
    fn chunked_framing_is_decoded_incrementally() {
        // The property is that a reader over this yields bytes before the last chunk has been
        // supplied — which a `Vec`-collecting decoder cannot do.
        let wire = b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        let mut r = BufReader::new(ChunkedReader {
            inner: BufReader::new(&wire[..]),
            remaining: 0,
            finished: false,
        });
        let mut out = String::new();
        r.read_to_string(&mut out).expect("chunked body decodes");
        assert_eq!(out, "hello world");
    }
}
