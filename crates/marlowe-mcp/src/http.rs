//! HTTP for MCP: one small request/response abstraction and two backends.
//!
//! - Loopback (`localhost`, `127.0.0.0/8`, `::1`): plain HTTP/1.1 over a `TcpStream` that never
//!   leaves the machine. This is how local servers and the tests' fake servers are reached.
//! - Anything else: `https` through `marlowe-net`, the only road to a remote host. Plain `http`
//!   to a remote host is refused, never upgraded.
//!
//! `marlowe-net` today exposes no response headers on a POST and no form POST, so the remote
//! backend can carry JSON-RPC to servers that do not use sessions but cannot read
//! `Mcp-Session-Id` or `WWW-Authenticate`, nor exchange an OAuth code. The request this crate
//! needs from `marlowe-net` (a header-complete, method-generic `request_streaming`) is recorded in
//! CHANGELOG.md under M4.6 K; until it lands a remote server that needs either fails with that
//! reason rather than half working.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

use crate::util::Url;
use crate::McpError;

/// One request. `body` is sent with `content_type`; GET and DELETE leave both empty.
#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub method: String,
    pub url: Url,
    pub headers: Vec<(String, String)>,
    pub content_type: Option<String>,
    pub body: Vec<u8>,
}

impl HttpRequest {
    pub fn get(url: &Url) -> Self {
        Self { method: "GET".into(), url: url.clone(), headers: Vec::new(), content_type: None, body: Vec::new() }
    }

    pub fn post_json(url: &Url, body: &serde_json::Value) -> Self {
        Self {
            method: "POST".into(),
            url: url.clone(),
            headers: Vec::new(),
            content_type: Some("application/json".into()),
            body: body.to_string().into_bytes(),
        }
    }

    pub fn post_form(url: &Url, form: &str) -> Self {
        Self {
            method: "POST".into(),
            url: url.clone(),
            headers: Vec::new(),
            content_type: Some("application/x-www-form-urlencoded".into()),
            body: form.as_bytes().to_vec(),
        }
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }
}

/// A response whose body is read as it arrives.
pub struct HttpResponse {
    pub status: u16,
    /// Names lowercased.
    pub headers: Vec<(String, String)>,
    pub body: Box<dyn BufRead + Send>,
}

impl HttpResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        let n = name.to_ascii_lowercase();
        self.headers.iter().find(|(k, _)| *k == n).map(|(_, v)| v.as_str())
    }

    pub fn is_sse(&self) -> bool {
        self.header("content-type").is_some_and(|c| c.to_ascii_lowercase().starts_with("text/event-stream"))
    }

    /// At most `limit` bytes of the body as text.
    pub fn text(&mut self, limit: usize) -> String {
        let mut buf = Vec::new();
        let _ = Read::by_ref(&mut self.body).take(limit as u64).read_to_end(&mut buf);
        String::from_utf8_lossy(&buf).into_owned()
    }

    pub fn json(&mut self, limit: usize) -> Option<serde_json::Value> {
        serde_json::from_str(&self.text(limit)).ok()
    }
}

/// Sends requests. `read_timeout` bounds each read, not the whole response.
pub trait HttpBackend: Send + Sync {
    fn send(&self, req: &HttpRequest, read_timeout: Duration) -> Result<HttpResponse, McpError>;
}

/// Bodies above this are refused in the non-streaming helpers.
pub const MAX_BODY: usize = 8 * 1024 * 1024;

/// Loopback over plain HTTP, everything else through `marlowe-net`.
#[derive(Default)]
pub struct DefaultHttp;

impl HttpBackend for DefaultHttp {
    fn send(&self, req: &HttpRequest, read_timeout: Duration) -> Result<HttpResponse, McpError> {
        if req.url.is_loopback() {
            return send_plain(req, read_timeout);
        }
        if !req.url.https {
            return Err(http_err(&req.url, "plain http is only allowed to this machine; use https for a remote server"));
        }
        send_net(req, read_timeout)
    }
}

fn http_err(url: &Url, detail: &str) -> McpError {
    McpError::Http { server: url.host.clone(), status: 0, detail: detail.to_string() }
}

fn valid_header(name: &str, value: &str) -> bool {
    !name.is_empty()
        && !name.contains(|c: char| c.is_control() || c == ':' || c == ' ')
        && !value.contains(['\r', '\n', '\0'])
}

// ---------------------------------------------------------------- loopback

fn loopback_addr(url: &Url) -> Result<SocketAddr, McpError> {
    let ip: IpAddr = if url.host == "localhost" {
        IpAddr::V4(Ipv4Addr::LOCALHOST)
    } else {
        url.host.parse().map_err(|_| http_err(url, "not a loopback address"))?
    };
    Ok(SocketAddr::new(ip, url.port))
}

fn send_plain(req: &HttpRequest, read_timeout: Duration) -> Result<HttpResponse, McpError> {
    let url = &req.url;
    for (k, v) in &req.headers {
        if !valid_header(k, v) {
            return Err(http_err(url, &format!("header `{k}` is not a valid header")));
        }
    }
    let addr = loopback_addr(url)?;
    let sock = TcpStream::connect_timeout(&addr, Duration::from_secs(5)).map_err(|e| http_err(url, &format!("cannot connect to {addr}: {e}")))?;
    sock.set_read_timeout(Some(read_timeout)).ok();
    sock.set_write_timeout(Some(Duration::from_secs(15))).ok();
    sock.set_nodelay(true).ok();

    let host = if url.host.contains(':') { format!("[{}]:{}", url.host, url.port) } else { format!("{}:{}", url.host, url.port) };
    let mut head = format!(
        "{} {} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: marlowe\r\nAccept-Encoding: identity\r\nConnection: close\r\n",
        req.method, url.path
    );
    if let Some(ct) = &req.content_type {
        head.push_str(&format!("Content-Type: {ct}\r\n"));
    }
    if !req.body.is_empty() || req.method == "POST" {
        head.push_str(&format!("Content-Length: {}\r\n", req.body.len()));
    }
    for (k, v) in &req.headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    let mut w = &sock;
    w.write_all(head.as_bytes()).and_then(|()| w.write_all(&req.body)).and_then(|()| w.flush()).map_err(|e| http_err(url, &format!("write failed: {e}")))?;

    let mut reader = BufReader::new(sock);
    let (status, headers) = read_head(&mut reader).map_err(|e| http_err(url, &e))?;
    let find = |n: &str| headers.iter().find(|(k, _)| k == n).map(|(_, v)| v.as_str());
    let body: Box<dyn BufRead + Send> = if find("transfer-encoding").is_some_and(|v| v.to_ascii_lowercase().contains("chunked")) {
        Box::new(BufReader::new(Chunked { inner: reader, remaining: 0, done: false }))
    } else if let Some(len) = find("content-length").and_then(|v| v.trim().parse::<u64>().ok()) {
        Box::new(BufReader::new(reader.take(len)))
    } else if status == 204 || status == 202 || req.method == "HEAD" {
        Box::new(std::io::Cursor::new(Vec::new()))
    } else {
        Box::new(reader)
    };
    Ok(HttpResponse { status, headers, body })
}

fn read_head(r: &mut impl BufRead) -> Result<(u16, Vec<(String, String)>), String> {
    let mut line = String::new();
    r.read_line(&mut line).map_err(|e| format!("read failed: {e}"))?;
    let status = line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| format!("malformed status line {:?}", line.trim()))?;
    let mut headers = Vec::new();
    loop {
        line.clear();
        let n = r.read_line(&mut line).map_err(|e| format!("read failed: {e}"))?;
        let l = line.trim_end();
        if n == 0 || l.is_empty() {
            break;
        }
        if let Some((k, v)) = l.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    Ok((status, headers))
}

/// `Transfer-Encoding: chunked`, decoded as it arrives.
struct Chunked<R: BufRead> {
    inner: R,
    remaining: usize,
    done: bool,
}

impl<R: BufRead> Read for Chunked<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.done || buf.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            let mut size = String::new();
            self.inner.read_line(&mut size)?;
            if size.trim().is_empty() {
                size.clear();
                self.inner.read_line(&mut size)?;
            }
            if size.is_empty() {
                self.done = true;
                return Ok(0);
            }
            let n = usize::from_str_radix(size.trim().split(';').next().unwrap_or("").trim(), 16)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            if n == 0 {
                self.done = true;
                return Ok(0);
            }
            self.remaining = n;
        }
        let want = buf.len().min(self.remaining);
        let n = self.inner.read(&mut buf[..want])?;
        self.remaining -= n;
        Ok(n)
    }
}

// ---------------------------------------------------------------- remote (marlowe-net)

fn send_net(req: &HttpRequest, read_timeout: Duration) -> Result<HttpResponse, McpError> {
    use marlowe_net::{shared, Headers, Target};
    let url = &req.url;
    let target = Target { host: url.host.clone(), port: url.port, path: url.path.clone(), tls: true };
    match (req.method.as_str(), req.content_type.as_deref()) {
        ("GET", _) if req.headers.iter().all(|(k, _)| k.eq_ignore_ascii_case("accept")) => {
            let accept = req.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("accept")).map_or("application/json", |(_, v)| v.as_str());
            let got = shared().fetch_with(&target, &Headers { user_agent: "marlowe", accept }).map_err(|e| http_err(url, &e.to_string()))?;
            let mut headers = Vec::new();
            if let Some(ct) = got.content_type {
                headers.push(("content-type".to_string(), ct));
            }
            if let Some(loc) = got.redirect_to {
                headers.push(("location".to_string(), loc));
            }
            Ok(HttpResponse { status: got.status, headers, body: Box::new(std::io::Cursor::new(got.bytes)) })
        }
        ("POST", Some("application/json")) => {
            let pairs: Vec<(&str, &str)> = req.headers.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
            let mut stream =
                shared().post_streaming(&target, &pairs, &req.body, read_timeout).map_err(|e| http_err(url, &e.to_string()))?;
            let mut headers = Vec::new();
            if let Some(ct) = stream.content_type.clone() {
                headers.push(("content-type".to_string(), ct));
            }
            if let Some(ra) = stream.retry_after.clone() {
                headers.push(("retry-after".to_string(), ra));
            }
            let status = stream.status;
            // `ResponseStream` borrows its body; read it into memory so the response owns it.
            // MCP responses over a remote POST are one JSON message or a short SSE stream.
            let text = stream.read_capped(MAX_BODY);
            Ok(HttpResponse { status, headers, body: Box::new(std::io::Cursor::new(text.into_bytes())) })
        }
        _ => Err(http_err(
            url,
            "this request needs marlowe-net's header-complete request_streaming (form POST, GET with headers, DELETE); see CHANGELOG M4.6 K",
        )),
    }
}

// ---------------------------------------------------------------- server-sent events

/// One server-sent event.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
    pub id: Option<String>,
}

/// Reads server-sent events from a body.
pub struct SseReader<R: BufRead> {
    inner: R,
}

impl<R: BufRead> SseReader<R> {
    pub fn new(inner: R) -> Self {
        Self { inner }
    }
}

impl<R: BufRead> Iterator for SseReader<R> {
    type Item = SseEvent;

    fn next(&mut self) -> Option<SseEvent> {
        let mut ev = SseEvent::default();
        let mut any = false;
        let mut has_data = false;
        let mut line = String::new();
        loop {
            line.clear();
            match self.inner.read_line(&mut line) {
                Ok(0) | Err(_) => return any.then_some(ev),
                Ok(_) => {}
            }
            let l = line.trim_end_matches(['\r', '\n']);
            if l.is_empty() {
                if any {
                    return Some(ev);
                }
                continue;
            }
            if l.starts_with(':') {
                continue;
            }
            let (field, value) = match l.split_once(':') {
                Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
                None => (l, ""),
            };
            match field {
                "event" => ev.event = Some(value.to_string()),
                "data" => {
                    if has_data {
                        ev.data.push('\n');
                    }
                    has_data = true;
                    ev.data.push_str(value);
                }
                "id" => ev.id = Some(value.to_string()),
                _ => {}
            }
            any = true;
        }
    }
}

/// Resolves `host:port` for diagnostics only.
pub fn resolves(host: &str, port: u16) -> bool {
    (host, port).to_socket_addrs().is_ok_and(|mut a| a.next().is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// Serves one canned response to the first connection and returns what it was sent.
    fn serve_once(response: &'static [u8]) -> (u16, std::thread::JoinHandle<String>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let h = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut head = String::new();
            let mut len = 0usize;
            loop {
                let mut line = String::new();
                r.read_line(&mut line).unwrap();
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap();
                }
                if line.trim().is_empty() {
                    break;
                }
                head.push_str(&line);
            }
            let mut body = vec![0u8; len];
            r.read_exact(&mut body).unwrap();
            head.push_str(&String::from_utf8_lossy(&body));
            s.write_all(response).unwrap();
            head
        });
        (port, h)
    }

    #[test]
    fn a_loopback_post_round_trips_json_and_headers() {
        let (port, seen) = serve_once(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nMcp-Session-Id: abc\r\nContent-Length: 11\r\n\r\n{\"ok\":true}");
        let url = Url::parse(&format!("http://127.0.0.1:{port}/mcp")).unwrap();
        let req = HttpRequest::post_json(&url, &serde_json::json!({"a": 1})).header("Authorization", "Bearer t");
        let mut resp = DefaultHttp.send(&req, Duration::from_secs(5)).unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.header("mcp-session-id"), Some("abc"));
        assert_eq!(resp.json(1000).unwrap()["ok"], true);
        let head = seen.join().unwrap();
        assert!(head.starts_with("POST /mcp HTTP/1.1"), "{head}");
        assert!(head.contains("Authorization: Bearer t") && head.contains("{\"a\":1}"), "{head}");
    }

    #[test]
    fn chunked_bodies_are_decoded() {
        let (port, _h) = serve_once(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n");
        let url = Url::parse(&format!("http://localhost:{port}/")).unwrap();
        let mut resp = DefaultHttp.send(&HttpRequest::get(&url), Duration::from_secs(5)).unwrap();
        assert_eq!(resp.text(100), "hello world");
    }

    #[test]
    fn plain_http_to_a_remote_host_is_refused_not_upgraded() {
        let url = Url::parse("http://example.com/mcp").unwrap();
        let e = DefaultHttp.send(&HttpRequest::get(&url), Duration::from_secs(1)).err().unwrap();
        assert!(e.to_string().contains("plain http"), "{e}");
    }

    #[test]
    fn a_header_with_a_line_break_is_refused() {
        let url = Url::parse("http://127.0.0.1:1/").unwrap();
        let req = HttpRequest::get(&url).header("X-A", "b\r\nX-Evil: 1");
        assert!(DefaultHttp.send(&req, Duration::from_secs(1)).is_err());
    }

    #[test]
    fn sse_events_are_parsed_with_multiline_data_comments_and_ids() {
        let text = ": hi\nid: 1\nevent: message\ndata: {\"a\":\ndata: 1}\n\ndata: second\n\n";
        let evs: Vec<SseEvent> = SseReader::new(text.as_bytes()).collect();
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0], SseEvent { event: Some("message".into()), data: "{\"a\":\n1}".into(), id: Some("1".into()) });
        assert_eq!(evs[1].data, "second");
        // A final event without the closing blank line still arrives.
        assert_eq!(SseReader::new("data: tail".as_bytes()).next().unwrap().data, "tail");
    }
}
