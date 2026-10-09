//! A minimal blocking HTTP/1.1 client for a loopback endpoint.
//!
//! No TLS, no redirects, no keep-alive. Bodies are decoded with `Content-Length`,
//! `Transfer-Encoding: chunked` (incrementally, so streams arrive as they are produced), or by
//! reading to EOF.
//!
//! Cancellation: the socket carries a short read timeout and [`Conn`] retries timed-out reads
//! below the buffering layer, checking the [`CancelToken`] each time. On Windows a socket
//! `shutdown` does not wake a blocked `read`, so polling is the only prompt way out.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

use marlowe_model::{CancelHook, CancelToken};

/// How often a blocked read wakes to check for cancellation.
pub const POLL_INTERVAL: Duration = Duration::from_millis(200);

#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    #[error("could not reach {endpoint}: {detail}")]
    Unreachable { endpoint: String, detail: String },
    #[error("{endpoint} returned HTTP {status}: {body}")]
    Status { endpoint: String, status: u16, body: String },
    #[error("malformed response from {endpoint}: {detail}")]
    Malformed { endpoint: String, detail: String },
    #[error("cancelled")]
    Cancelled,
}

/// A loopback endpoint. Refuses any host that is not local.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalEndpoint {
    host: String,
    port: u16,
}

impl LocalEndpoint {
    pub const DEFAULT_PORT: u16 = 11434;

    /// `None` for a host that is not loopback.
    pub fn new(host: &str, port: u16) -> Option<Self> {
        let host = host.trim().trim_start_matches('[').trim_end_matches(']').to_ascii_lowercase();
        let loopback = host == "localhost" || host == "127.0.0.1" || host == "::1";
        loopback.then_some(Self { host, port })
    }

    /// Parses `http://host:port`, `host:port` or `host` (default port). Loopback only.
    pub fn parse(url: &str) -> Option<Self> {
        let s = url.trim();
        let s = s.strip_prefix("http://").unwrap_or(s);
        if s.contains("://") {
            return None;
        }
        let s = s.trim_end_matches('/');
        if let Some(rest) = s.strip_prefix('[') {
            let (host, tail) = rest.split_once(']')?;
            let port = match tail.strip_prefix(':') {
                Some(p) => p.parse().ok()?,
                None if tail.is_empty() => Self::DEFAULT_PORT,
                None => return None,
            };
            return Self::new(host, port);
        }
        match s.rsplit_once(':') {
            Some((host, port)) if !host.contains(':') => Self::new(host, port.parse().ok()?),
            Some(_) => Self::new(s, Self::DEFAULT_PORT),
            None => Self::new(s, Self::DEFAULT_PORT),
        }
    }

    pub fn default_ollama() -> Self {
        Self { host: "127.0.0.1".into(), port: Self::DEFAULT_PORT }
    }

    pub fn authority(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    fn addr(&self) -> io::Result<SocketAddr> {
        self.authority()
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no address"))
    }
}

impl std::fmt::Display for LocalEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "http://{}", self.authority())
    }
}

/// A socket whose reads poll for cancellation and give up after `idle` without data.
pub struct Conn {
    sock: TcpStream,
    cancel: Option<CancelToken>,
    idle: Duration,
    last: Instant,
    _hook: Option<CancelHook>,
}

const CANCELLED_MARKER: &str = "marlowe: cancelled";

impl Conn {
    fn open(
        endpoint: &LocalEndpoint,
        cancel: Option<&CancelToken>,
        idle: Duration,
    ) -> io::Result<Self> {
        let addr = endpoint.addr()?;
        let sock = TcpStream::connect_timeout(&addr, idle.min(Duration::from_secs(5)))?;
        sock.set_read_timeout(Some(POLL_INTERVAL.min(idle)))?;
        sock.set_write_timeout(Some(idle))?;
        let _ = sock.set_nodelay(true);
        let hook = match cancel {
            Some(c) => {
                let clone = sock.try_clone()?;
                Some(c.on_cancel(move || {
                    let _ = clone.shutdown(std::net::Shutdown::Both);
                }))
            }
            None => None,
        };
        Ok(Self { sock, cancel: cancel.cloned(), idle, last: Instant::now(), _hook: hook })
    }
}

impl Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.cancel.as_ref().is_some_and(CancelToken::is_cancelled) {
                return Err(io::Error::other(CANCELLED_MARKER));
            }
            match self.sock.read(buf) {
                Ok(n) => {
                    self.last = Instant::now();
                    return Ok(n);
                }
                Err(e) if matches!(e.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock) => {
                    if self.last.elapsed() >= self.idle {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            format!("no data for {}s", self.idle.as_secs()),
                        ));
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }
}

impl Write for Conn {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.sock.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.sock.flush()
    }
}

/// True when `e` came from a cancelled [`Conn`].
pub fn is_cancelled_io(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::Other && e.to_string() == CANCELLED_MARKER
}

/// A response with its body undrained.
pub struct Response {
    pub status: u16,
    pub body: Box<dyn BufRead + Send>,
}

/// Sends one request and returns the status and a decoded body reader.
pub fn send(
    endpoint: &LocalEndpoint,
    method: &str,
    path: &str,
    body: Option<&serde_json::Value>,
    accept: &str,
    cancel: Option<&CancelToken>,
    idle: Duration,
) -> Result<Response, HttpError> {
    let io_err = |e: io::Error| io_to_http(endpoint, e, cancel);
    let mut conn = Conn::open(endpoint, cancel, idle).map_err(io_err)?;

    let serialized = body.map(|b| b.to_string()).unwrap_or_default();
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nAccept: {accept}\r\n",
        endpoint.authority()
    );
    if body.is_some() {
        head.push_str("Content-Type: application/json\r\n");
        head.push_str(&format!("Content-Length: {}\r\n", serialized.len()));
    }
    head.push_str("\r\n");
    conn.write_all(head.as_bytes()).map_err(io_err)?;
    conn.write_all(serialized.as_bytes()).map_err(io_err)?;
    conn.flush().map_err(io_err)?;

    let mut reader = BufReader::new(conn);
    let mut status_line = String::new();
    reader.read_line(&mut status_line).map_err(io_err)?;
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| HttpError::Malformed {
            endpoint: endpoint.to_string(),
            detail: format!("no status in {status_line:?}"),
        })?;

    let mut content_length: Option<u64> = None;
    let mut chunked = false;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).map_err(io_err)? == 0 {
            break;
        }
        if line.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim();
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().ok();
            } else if name.eq_ignore_ascii_case("transfer-encoding")
                && value.trim().eq_ignore_ascii_case("chunked")
            {
                chunked = true;
            }
        }
    }

    let body: Box<dyn BufRead + Send> = if chunked {
        Box::new(BufReader::new(ChunkedBody::new(reader)))
    } else if let Some(n) = content_length {
        Box::new(BufReader::new(reader.take(n)))
    } else {
        Box::new(reader)
    };
    Ok(Response { status, body })
}

fn io_to_http(endpoint: &LocalEndpoint, e: io::Error, cancel: Option<&CancelToken>) -> HttpError {
    if is_cancelled_io(&e) || cancel.is_some_and(CancelToken::is_cancelled) {
        HttpError::Cancelled
    } else {
        HttpError::Unreachable { endpoint: endpoint.to_string(), detail: e.to_string() }
    }
}

/// Maps a body read error.
pub fn read_error(endpoint: &LocalEndpoint, e: io::Error, cancel: Option<&CancelToken>) -> HttpError {
    io_to_http(endpoint, e, cancel)
}

fn read_all(endpoint: &LocalEndpoint, mut resp: Response, cap: usize) -> Result<String, HttpError> {
    let mut buf = Vec::new();
    Read::by_ref(&mut resp.body)
        .take(cap as u64)
        .read_to_end(&mut buf)
        .map_err(|e| io_to_http(endpoint, e, None))?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

const MAX_JSON_BYTES: usize = 32 * 1024 * 1024;

/// Sends a request and parses a JSON response; non-2xx is [`HttpError::Status`].
pub fn json(
    endpoint: &LocalEndpoint,
    method: &str,
    path: &str,
    body: Option<&serde_json::Value>,
    timeout: Duration,
) -> Result<serde_json::Value, HttpError> {
    let resp = send(endpoint, method, path, body, "application/json", None, timeout)?;
    let status = resp.status;
    let text = read_all(endpoint, resp, MAX_JSON_BYTES)?;
    if !(200..300).contains(&status) {
        return Err(HttpError::Status {
            endpoint: endpoint.to_string(),
            status,
            body: text.chars().take(400).collect(),
        });
    }
    serde_json::from_str(&text).map_err(|e| HttpError::Malformed {
        endpoint: endpoint.to_string(),
        detail: format!("{e}; body began: {}", text.chars().take(200).collect::<String>()),
    })
}

pub fn get_json(
    endpoint: &LocalEndpoint,
    path: &str,
    timeout: Duration,
) -> Result<serde_json::Value, HttpError> {
    json(endpoint, "GET", path, None, timeout)
}

pub fn post_json(
    endpoint: &LocalEndpoint,
    path: &str,
    body: &serde_json::Value,
    timeout: Duration,
) -> Result<serde_json::Value, HttpError> {
    json(endpoint, "POST", path, Some(body), timeout)
}

/// POSTs a JSON body and returns the streaming body; non-2xx is [`HttpError::Status`].
pub fn post_stream(
    endpoint: &LocalEndpoint,
    path: &str,
    body: &serde_json::Value,
    accept: &str,
    cancel: &CancelToken,
    idle: Duration,
) -> Result<Box<dyn BufRead + Send>, HttpError> {
    let resp = send(endpoint, "POST", path, Some(body), accept, Some(cancel), idle)?;
    if !(200..300).contains(&resp.status) {
        let status = resp.status;
        let text = read_all(endpoint, resp, 4_000).unwrap_or_default();
        return Err(HttpError::Status {
            endpoint: endpoint.to_string(),
            status,
            body: text.chars().take(400).collect(),
        });
    }
    Ok(resp.body)
}

/// `Transfer-Encoding: chunked`, decoded on the fly.
struct ChunkedBody<R: BufRead> {
    inner: R,
    remaining: usize,
    finished: bool,
    header: Vec<u8>,
}

impl<R: BufRead> ChunkedBody<R> {
    fn new(inner: R) -> Self {
        Self { inner, remaining: 0, finished: false, header: Vec::new() }
    }
}

impl<R: BufRead> Read for ChunkedBody<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.finished || buf.is_empty() {
            return Ok(0);
        }
        while self.remaining == 0 {
            // `header` survives an error, so a retried read resumes the same line.
            if self.inner.read_until(b'\n', &mut self.header)? == 0 {
                self.finished = true;
                return Ok(0);
            }
            if !self.header.ends_with(b"\n") {
                continue;
            }
            let line = String::from_utf8_lossy(&self.header).trim().to_string();
            self.header.clear();
            if line.is_empty() {
                // The CRLF that ended the previous chunk.
                continue;
            }
            let size = usize::from_str_radix(line.split(';').next().unwrap_or("").trim(), 16)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            if size == 0 {
                self.finished = true;
                return Ok(0);
            }
            self.remaining = size;
        }
        let want = buf.len().min(self.remaining);
        let n = self.inner.read(&mut buf[..want])?;
        if n == 0 {
            self.finished = true;
        }
        self.remaining -= n;
        Ok(n)
    }
}

/// One JSON value per line, read as the bytes land.
pub struct NdjsonStream {
    reader: Box<dyn BufRead + Send>,
    line: Vec<u8>,
    done: bool,
}

impl NdjsonStream {
    pub fn new(reader: Box<dyn BufRead + Send>) -> Self {
        Self { reader, line: Vec::new(), done: false }
    }

    /// The next value, `None` at EOF. Errors are I/O (`Err(Ok(io))`) or JSON (`Err(Err(json))`).
    pub fn next_value(&mut self) -> Option<Result<serde_json::Value, NdjsonError>> {
        if self.done {
            return None;
        }
        loop {
            self.line.clear();
            match self.reader.read_until(b'\n', &mut self.line) {
                Ok(0) => {
                    self.done = true;
                    return None;
                }
                Ok(_) => {
                    let text = String::from_utf8_lossy(&self.line);
                    let t = text.trim();
                    if t.is_empty() {
                        continue;
                    }
                    return Some(serde_json::from_str(t).map_err(|e| {
                        NdjsonError::Json(format!(
                            "{e}; line began: {}",
                            t.chars().take(120).collect::<String>()
                        ))
                    }));
                }
                Err(e) => {
                    self.done = true;
                    return Some(Err(NdjsonError::Io(e)));
                }
            }
        }
    }
}

#[derive(Debug)]
pub enum NdjsonError {
    Io(io::Error),
    Json(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn only_loopback_endpoints_can_be_constructed() {
        assert!(LocalEndpoint::new("127.0.0.1", 11434).is_some());
        assert!(LocalEndpoint::new("localhost", 11434).is_some());
        assert!(LocalEndpoint::new("::1", 11434).is_some());
        assert!(LocalEndpoint::new("example.com", 11434).is_none());
        assert!(LocalEndpoint::new("10.0.0.5", 11434).is_none());
        assert!(LocalEndpoint::new("127.0.0.1.evil.com", 11434).is_none());
    }

    #[test]
    fn base_urls_parse_to_loopback_endpoints_only() {
        assert_eq!(LocalEndpoint::parse("http://127.0.0.1:11434"), LocalEndpoint::new("127.0.0.1", 11434));
        assert_eq!(LocalEndpoint::parse("localhost:9000/"), LocalEndpoint::new("localhost", 9000));
        assert_eq!(LocalEndpoint::parse("127.0.0.1"), LocalEndpoint::new("127.0.0.1", 11434));
        assert_eq!(LocalEndpoint::parse("http://[::1]:8080"), LocalEndpoint::new("::1", 8080));
        assert!(LocalEndpoint::parse("https://127.0.0.1:11434").is_none());
        assert!(LocalEndpoint::parse("http://example.com:11434").is_none());
    }

    #[test]
    fn an_absent_endpoint_is_unreachable_rather_than_a_panic() {
        let e = get_json(
            &LocalEndpoint::new("127.0.0.1", 1).unwrap(),
            "/api/tags",
            Duration::from_millis(500),
        )
        .unwrap_err();
        assert!(matches!(e, HttpError::Unreachable { .. }), "{e}");
    }

    #[test]
    fn chunked_bodies_decode_across_chunks() {
        let wire = b"5\r\nhello\r\n7\r\n, world\r\n0\r\n\r\n";
        let mut body = ChunkedBody::new(BufReader::new(Cursor::new(&wire[..])));
        let mut out = String::new();
        body.read_to_string(&mut out).unwrap();
        assert_eq!(out, "hello, world");
    }

    #[test]
    fn ndjson_skips_blank_lines_and_reports_bad_json() {
        let wire = "{\"a\":1}\n\nnot json\n{\"b\":2}";
        let mut s = NdjsonStream::new(Box::new(BufReader::new(Cursor::new(wire.as_bytes().to_vec()))));
        assert_eq!(s.next_value().unwrap().unwrap()["a"], 1);
        assert!(matches!(s.next_value().unwrap(), Err(NdjsonError::Json(_))));
        assert_eq!(s.next_value().unwrap().unwrap()["b"], 2);
        assert!(s.next_value().is_none());
    }
}
