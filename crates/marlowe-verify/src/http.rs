//! `http_check`: request a URL and assert on the answer.
//!
//! Loopback (`localhost`, `127.0.0.0/8`, `::1`) is plain HTTP over a socket this crate opens
//! itself, because `marlowe-net` speaks https only; a name must resolve to loopback addresses
//! only, so a hostile name cannot turn a localhost check into a request elsewhere. Any other
//! host is https GET through `marlowe-net` (its pinned roots, no redirects followed), which the
//! tool layer treats as network egress: it asks for a new host and taints the session.
//!
//! The answer is checked against [`Checks`] (status, body substrings, JSON paths, headers), and
//! `wait` polls until they pass, which is how a check follows a service that is still starting.

use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

use regex::Regex;
use serde_json::Value;

/// Body bytes kept from a response.
pub const MAX_BODY: usize = 2 * 1024 * 1024;
/// Default budget for one request.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HttpError {
    #[error("{0}")]
    Url(String),
    #[error("nothing is listening on {0} (connection refused)")]
    Refused(String),
    #[error("{0}")]
    Io(String),
    #[error("timed out after {0:?}")]
    Timeout(Duration),
    #[error("malformed response: {0}")]
    Malformed(String),
    #[error("cancelled")]
    Cancelled,
}

/// A URL split into what the client needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Url {
    pub https: bool,
    pub host: String,
    pub port: u16,
    /// Path and query, starting with `/`.
    pub path: String,
}

impl Url {
    pub fn parse(url: &str) -> Result<Url, HttpError> {
        let url = url.trim();
        let (https, rest) = if let Some(r) = url.strip_prefix("https://") {
            (true, r)
        } else if let Some(r) = url.strip_prefix("http://") {
            (false, r)
        } else {
            return Err(HttpError::Url(format!("`{url}` is not an http:// or https:// URL")));
        };
        let (authority, path) = match rest.find(['/', '?', '#']) {
            Some(i) if rest.as_bytes()[i] == b'/' => (&rest[..i], rest[i..].to_string()),
            Some(i) => (&rest[..i], format!("/{}", &rest[i..])),
            None => (rest, "/".to_string()),
        };
        let path = path.split('#').next().unwrap_or("/").to_string();
        if authority.contains('@') {
            return Err(HttpError::Url("userinfo in a URL is not accepted".into()));
        }
        let default = if https { 443 } else { 80 };
        let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
            let (h, after) = rest.split_once(']').ok_or_else(|| HttpError::Url("unterminated [ in the host".into()))?;
            let port = match after.strip_prefix(':') {
                Some(p) => p.parse::<u16>().map_err(|_| HttpError::Url("the port is not a number".into()))?,
                None => default,
            };
            (h.to_string(), port)
        } else {
            match authority.rsplit_once(':') {
                Some((h, p)) => (h.to_string(), p.parse::<u16>().map_err(|_| HttpError::Url("the port is not a number".into()))?),
                None => (authority.to_string(), default),
            }
        };
        if host.is_empty() || host.contains(char::is_whitespace) {
            return Err(HttpError::Url("the host is empty or contains whitespace".into()));
        }
        Ok(Url { https, host: host.to_ascii_lowercase(), port, path })
    }

    pub fn is_loopback(&self) -> bool {
        is_loopback_host(&self.host)
    }

    /// `host:port` (IPv6 hosts bracketed).
    pub fn authority(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    pub fn display(&self) -> String {
        format!("{}://{}{}", if self.https { "https" } else { "http" }, self.authority(), self.path)
    }
}

/// `localhost`, `*.localhost`, and loopback IP literals.
pub fn is_loopback_host(host: &str) -> bool {
    let h = host.trim_matches(['[', ']']).to_ascii_lowercase();
    if h == "localhost" || h.ends_with(".localhost") {
        return true;
    }
    match h.parse::<IpAddr>() {
        Ok(IpAddr::V4(v4)) => v4.is_loopback(),
        Ok(IpAddr::V6(v6)) => v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v| v.is_loopback()),
        Err(_) => false,
    }
}

/// One request.
#[derive(Debug, Clone)]
pub struct Request {
    pub url: Url,
    pub method: String,
    /// `Name: value` pairs.
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
    pub timeout: Duration,
}

impl Request {
    pub fn get(url: &str) -> Result<Self, HttpError> {
        Ok(Self { url: Url::parse(url)?, method: "GET".into(), headers: Vec::new(), body: None, timeout: DEFAULT_TIMEOUT })
    }
}

/// One response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub reason: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub truncated: bool,
    pub elapsed: Duration,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

fn valid_token(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

/// Sends `req` once. `cancelled` is polled between reads.
pub fn send(req: &Request, cancelled: &dyn Fn() -> bool) -> Result<Response, HttpError> {
    if req.url.is_loopback() {
        if req.url.https {
            return Err(HttpError::Url(
                "https to localhost is not supported (the certificate would be self-signed); use http://localhost:PORT".into(),
            ));
        }
        send_loopback(req, cancelled)
    } else {
        send_remote(req)
    }
}

fn send_remote(req: &Request) -> Result<Response, HttpError> {
    if !req.url.https {
        return Err(HttpError::Url("only https is allowed off this machine; plaintext http is for localhost".into()));
    }
    if !matches!(req.method.as_str(), "GET" | "HEAD") || req.body.is_some() || !req.headers.is_empty() {
        return Err(HttpError::Url("a request to another host is a plain GET; send methods, bodies and headers to localhost only".into()));
    }
    if req.method == "HEAD" {
        return Err(HttpError::Url("HEAD to another host is not supported; use GET".into()));
    }
    let target = marlowe_net::Target::parse(&req.url.display()).map_err(|e| HttpError::Url(e.to_string()))?;
    let started = Instant::now();
    let f = marlowe_net::shared().fetch(&target).map_err(|e| HttpError::Io(e.to_string()))?;
    let mut headers = Vec::new();
    if let Some(ct) = f.content_type {
        headers.push(("content-type".to_string(), ct));
    }
    if let Some(loc) = f.redirect_to {
        headers.push(("location".to_string(), loc));
    }
    let truncated = f.bytes.len() > MAX_BODY;
    let mut body = f.bytes;
    body.truncate(MAX_BODY);
    Ok(Response { status: f.status, reason: reason(f.status).into(), headers, body, truncated, elapsed: started.elapsed() })
}

fn send_loopback(req: &Request, cancelled: &dyn Fn() -> bool) -> Result<Response, HttpError> {
    let started = Instant::now();
    let deadline = started + req.timeout;
    let addrs: Vec<SocketAddr> = (req.url.host.as_str(), req.url.port)
        .to_socket_addrs()
        .map_err(|e| HttpError::Io(format!("cannot resolve {}: {e}", req.url.host)))?
        .collect();
    if addrs.is_empty() {
        return Err(HttpError::Io(format!("{} did not resolve", req.url.host)));
    }
    if let Some(a) = addrs.iter().find(|a| !a.ip().is_loopback()) {
        return Err(HttpError::Url(format!("{} resolves to {}, which is not loopback; localhost checks stay on this machine", req.url.host, a.ip())));
    }
    if !valid_token(&req.method) {
        return Err(HttpError::Url(format!("`{}` is not an HTTP method", req.method)));
    }
    let mut head = format!("{} {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: marlowe-verify\r\nAccept: */*\r\nConnection: close\r\n", req.method, req.url.path, req.url.authority());
    for (k, v) in &req.headers {
        if !valid_token(k) || v.contains(['\r', '\n']) {
            return Err(HttpError::Url(format!("header `{k}` is not valid")));
        }
        if ["host", "connection", "content-length", "transfer-encoding"].iter().any(|r| k.eq_ignore_ascii_case(r)) {
            continue;
        }
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    if let Some(b) = &req.body {
        head.push_str(&format!("Content-Length: {}\r\n", b.len()));
    } else if matches!(req.method.as_str(), "POST" | "PUT" | "PATCH") {
        head.push_str("Content-Length: 0\r\n");
    }
    head.push_str("\r\n");

    let mut last_err = None;
    let mut stream = None;
    for a in &addrs {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(HttpError::Timeout(req.timeout));
        }
        match TcpStream::connect_timeout(a, left.min(Duration::from_secs(5))) {
            Ok(s) => {
                stream = Some(s);
                break;
            }
            Err(e) => last_err = Some((e, *a)),
        }
    }
    let Some(mut stream) = stream else {
        let (e, a) = last_err.expect("at least one address");
        return Err(if e.kind() == std::io::ErrorKind::ConnectionRefused {
            HttpError::Refused(req.url.authority())
        } else {
            HttpError::Io(format!("cannot connect to {a}: {e}"))
        });
    };
    let io = |e: std::io::Error| HttpError::Io(e.to_string());
    stream.set_write_timeout(Some(Duration::from_secs(10))).map_err(io)?;
    stream.write_all(head.as_bytes()).map_err(io)?;
    if let Some(b) = &req.body {
        stream.write_all(b.as_bytes()).map_err(io)?;
    }
    let mut raw: Vec<u8> = Vec::new();
    let mut buf = [0u8; 16 * 1024];
    let mut truncated = false;
    loop {
        if cancelled() {
            return Err(HttpError::Cancelled);
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(HttpError::Timeout(req.timeout));
        }
        stream.set_read_timeout(Some(left.min(Duration::from_millis(500)))).map_err(io)?;
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&buf[..n]);
                if raw.len() > MAX_BODY + 64 * 1024 {
                    truncated = true;
                    break;
                }
            }
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => continue,
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset && !raw.is_empty() => break,
            Err(e) => return Err(io(e)),
        }
    }
    parse_response(&raw, truncated, started.elapsed(), &req.method)
}

fn parse_response(raw: &[u8], truncated: bool, elapsed: Duration, method: &str) -> Result<Response, HttpError> {
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").ok_or_else(|| {
        HttpError::Malformed(if raw.is_empty() { "the server closed the connection without answering".into() } else { "no end of headers".into() })
    })?;
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let mut lines = head.lines();
    let status_line = lines.next().unwrap_or("");
    let mut parts = status_line.splitn(3, ' ');
    let version = parts.next().unwrap_or("");
    if !version.starts_with("HTTP/") {
        return Err(HttpError::Malformed(format!("bad status line `{}`", status_line.chars().take(60).collect::<String>())));
    }
    let status: u16 = parts.next().and_then(|s| s.parse().ok()).ok_or_else(|| HttpError::Malformed("no status code".into()))?;
    let reason_text = parts.next().unwrap_or("").to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    let body_raw = &raw[split + 4..];
    let chunked = headers.iter().any(|(k, v)| k.eq_ignore_ascii_case("transfer-encoding") && v.to_ascii_lowercase().contains("chunked"));
    let mut body = if method == "HEAD" || status == 204 || status == 304 || (100..200).contains(&status) {
        Vec::new()
    } else if chunked {
        dechunk(body_raw)
    } else {
        body_raw.to_vec()
    };
    let mut truncated = truncated;
    if body.len() > MAX_BODY {
        body.truncate(MAX_BODY);
        truncated = true;
    }
    Ok(Response { status, reason: reason_text, headers, body, truncated, elapsed })
}

/// Chunked transfer decoding; a short or malformed tail keeps what was decoded.
fn dechunk(mut data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let Some(eol) = data.windows(2).position(|w| w == b"\r\n") else { break };
        let size_text = String::from_utf8_lossy(&data[..eol]);
        let size_text = size_text.split(';').next().unwrap_or("").trim();
        let Ok(size) = usize::from_str_radix(size_text, 16) else { break };
        data = &data[eol + 2..];
        if size == 0 {
            break;
        }
        let take = size.min(data.len());
        out.extend_from_slice(&data[..take]);
        if take < size || data.len() < size + 2 {
            break;
        }
        data = &data[size + 2..];
    }
    out
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "",
    }
}

// ---- assertions ------------------------------------------------------------------------------

/// Accepted statuses: exact codes and classes (`2xx`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StatusSpec(pub Vec<(u16, u16)>);

impl StatusSpec {
    /// From a JSON value: `200`, `"200"`, `"2xx"`, or a list of those.
    pub fn from_json(v: &Value) -> Result<Self, String> {
        let mut ranges = Vec::new();
        let mut one = |v: &Value| -> Result<(), String> {
            match v {
                Value::Number(n) => {
                    let c = n.as_u64().filter(|c| (100..600).contains(c)).ok_or("a status code is 100-599")? as u16;
                    ranges.push((c, c));
                }
                Value::String(s) => {
                    let s = s.trim().to_ascii_lowercase();
                    if let Some(d) = s.strip_suffix("xx").and_then(|d| d.parse::<u16>().ok()).filter(|d| (1..=5).contains(d)) {
                        ranges.push((d * 100, d * 100 + 99));
                    } else if let Ok(c) = s.parse::<u16>() {
                        ranges.push((c, c));
                    } else {
                        return Err(format!("`{s}` is not a status (200 or 2xx)"));
                    }
                }
                _ => return Err("a status is a number or a string like 2xx".into()),
            }
            Ok(())
        };
        match v {
            Value::Array(a) => a.iter().try_for_each(&mut one)?,
            other => one(other)?,
        }
        Ok(StatusSpec(ranges))
    }

    pub fn accepts(&self, status: u16) -> bool {
        self.0.iter().any(|(lo, hi)| (*lo..=*hi).contains(&status))
    }

    pub fn describe(&self) -> String {
        self.0.iter().map(|(lo, hi)| if lo == hi { lo.to_string() } else { format!("{}xx", lo / 100) }).collect::<Vec<_>>().join(" or ")
    }
}

/// What the answer must satisfy.
#[derive(Debug, Clone, Default)]
pub struct Checks {
    pub status: Option<StatusSpec>,
    pub contains: Vec<String>,
    pub not_contains: Vec<String>,
    /// `path.to.value=expected`, or `path` alone to require that it exists.
    pub json: Vec<String>,
    /// `Name: substring`.
    pub headers: Vec<String>,
}

impl Checks {
    pub fn is_empty(&self) -> bool {
        self.status.is_none() && self.contains.is_empty() && self.not_contains.is_empty() && self.json.is_empty() && self.headers.is_empty()
    }
}

/// One assertion's outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assertion {
    pub ok: bool,
    pub what: String,
    pub got: String,
}

/// Evaluates `checks` against `resp`. With no status given, the status must be below 400.
pub fn evaluate(checks: &Checks, resp: &Response) -> Vec<Assertion> {
    let mut out = Vec::new();
    match &checks.status {
        Some(spec) => out.push(Assertion { ok: spec.accepts(resp.status), what: format!("status is {}", spec.describe()), got: resp.status.to_string() }),
        None => out.push(Assertion { ok: resp.status < 400, what: "status is below 400".into(), got: resp.status.to_string() }),
    }
    let body = resp.text();
    for s in &checks.contains {
        out.push(Assertion { ok: body.contains(s.as_str()), what: format!("body contains \"{}\"", clip(s, 60)), got: String::new() });
    }
    for s in &checks.not_contains {
        out.push(Assertion { ok: !body.contains(s.as_str()), what: format!("body does not contain \"{}\"", clip(s, 60)), got: String::new() });
    }
    if !checks.json.is_empty() {
        let parsed: Option<Value> = serde_json::from_slice(&resp.body).ok();
        for spec in &checks.json {
            let (path, expected) = match spec.split_once('=') {
                Some((p, e)) => (p.trim(), Some(e.trim())),
                None => (spec.trim(), None),
            };
            let found = parsed.as_ref().and_then(|v| json_at(v, path));
            let (ok, got) = match (&parsed, found, expected) {
                (None, _, _) => (false, "the body is not JSON".to_string()),
                (Some(_), None, _) => (false, "no such path".to_string()),
                (Some(_), Some(v), None) => (true, show(v)),
                (Some(_), Some(v), Some(e)) => (show(v) == e.trim_matches('"'), show(v)),
            };
            out.push(Assertion { ok, what: format!("json {spec}"), got });
        }
    }
    for spec in &checks.headers {
        let (name, want) = spec.split_once(':').map(|(n, w)| (n.trim(), w.trim())).unwrap_or((spec.trim(), ""));
        let got = resp.header(name).unwrap_or("");
        out.push(Assertion { ok: resp.header(name).is_some() && got.to_ascii_lowercase().contains(&want.to_ascii_lowercase()), what: format!("header {name} contains \"{want}\""), got: got.to_string() });
    }
    out
}

fn json_at<'a>(v: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = v;
    for seg in path.split(['.', '[', ']']).filter(|s| !s.is_empty()) {
        cur = match cur {
            Value::Array(a) => a.get(seg.parse::<usize>().ok()?)?,
            Value::Object(o) => o.get(seg)?,
            _ => return None,
        };
    }
    Some(cur)
}

fn show(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}...", s.chars().take(max).collect::<String>())
    }
}

/// The result of [`poll`].
#[derive(Debug)]
pub struct Checked {
    pub response: Result<Response, HttpError>,
    pub assertions: Vec<Assertion>,
    pub attempts: u32,
    pub waited: Duration,
}

impl Checked {
    pub fn ok(&self) -> bool {
        self.response.is_ok() && !self.assertions.is_empty() && self.assertions.iter().all(|a| a.ok)
    }
}

/// Requests and checks, repeating every 250 ms for up to `wait` while it fails.
pub fn poll(req: &Request, checks: &Checks, wait: Duration, cancelled: &dyn Fn() -> bool) -> Checked {
    let started = Instant::now();
    let mut attempts = 0;
    loop {
        attempts += 1;
        let response = send(req, cancelled);
        let assertions = response.as_ref().map(|r| evaluate(checks, r)).unwrap_or_default();
        let done = matches!(&response, Ok(_) if assertions.iter().all(|a| a.ok))
            || matches!(&response, Err(HttpError::Url(_) | HttpError::Cancelled))
            || started.elapsed() >= wait
            || cancelled();
        if done {
            return Checked { response, assertions, attempts, waited: started.elapsed() };
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

// ---- rendering -------------------------------------------------------------------------------

/// Body text for the model: JSON and text as is (bounded); HTML reduced to its title and visible text.
pub fn body_excerpt(resp: &Response, max: usize) -> String {
    let ct = resp.header("content-type").unwrap_or("").to_ascii_lowercase();
    if resp.body.is_empty() {
        return String::new();
    }
    if resp.body.contains(&0) {
        return format!("(binary, {} bytes, {})", resp.body.len(), if ct.is_empty() { "no content type" } else { &ct });
    }
    let text = resp.text();
    if ct.contains("html") || text.trim_start().starts_with("<!") || text.trim_start().starts_with("<html") {
        let strip = |re: &str, s: &str| Regex::new(re).expect("static regex").replace_all(s, " ").into_owned();
        let title = Regex::new(r"(?is)<title[^>]*>(.*?)</title>").expect("static regex").captures(&text).map(|c| c[1].trim().to_string());
        let mut visible = strip(r"(?is)<head\b.*?</head>", &text);
        for tag in ["script", "style", "noscript"] {
            visible = strip(&format!(r"(?is)<{tag}\b.*?</{tag}>"), &visible);
        }
        let visible = strip(r"(?s)<[^>]*>", &visible);
        let visible = visible.split_whitespace().collect::<Vec<_>>().join(" ");
        let mut out = String::new();
        if let Some(t) = title {
            out.push_str(&format!("<title> {t}\n"));
        }
        out.push_str(&clip_bytes(&visible, max));
        out.push_str("\n(HTML shown as text; `browser` renders it and reports console errors)");
        return out;
    }
    clip_bytes(&text, max)
}

fn clip_bytes(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut cut = max;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n... [+{} bytes]", &s[..cut], s.len() - cut)
}

/// The tool result for a finished check.
pub fn render(req: &Request, c: &Checked) -> String {
    let line = format!("{} {}", req.method, req.url.display());
    let mut out = String::new();
    match &c.response {
        Err(e) => {
            out.push_str(&format!("{line} -> FAILED: {e}\n"));
            if c.attempts > 1 {
                out.push_str(&format!("tried {} times over {:.1} s\n", c.attempts, c.waited.as_secs_f64()));
            }
        }
        Ok(r) => {
            let verdict = if c.ok() { "ok" } else { "CHECK FAILED" };
            out.push_str(&format!("{line} -> {} {} ({verdict}, {} ms, {} bytes{})\n", r.status, r.reason, r.elapsed.as_millis(), r.body.len(), if r.truncated { ", truncated" } else { "" }));
            if c.attempts > 1 {
                out.push_str(&format!("passed on try {} after {:.1} s\n", c.attempts, c.waited.as_secs_f64()));
            }
            for h in ["content-type", "location", "set-cookie", "content-length", "cache-control"] {
                if let Some(v) = r.header(h) {
                    out.push_str(&format!("{h}: {}\n", clip(v, 160)));
                }
            }
            let body = body_excerpt(r, 4000);
            if !body.is_empty() {
                out.push_str("body:\n");
                out.push_str(&body);
                out.push('\n');
            }
            out.push_str(&format!("checks: {}/{} passed\n", c.assertions.iter().filter(|a| a.ok).count(), c.assertions.len()));
            for a in &c.assertions {
                out.push_str(&format!("  {} {}{}\n", if a.ok { "ok  " } else { "FAIL" }, a.what, if a.got.is_empty() { String::new() } else { format!("  (got {})", clip(&a.got, 100)) }));
            }
        }
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn urls_parse_and_loopback_is_recognised() {
        let u = Url::parse("http://localhost:5173/a/b?x=1#frag").unwrap();
        assert_eq!((u.https, u.host.as_str(), u.port, u.path.as_str()), (false, "localhost", 5173, "/a/b?x=1"));
        assert_eq!(Url::parse("https://Example.org").unwrap().path, "/");
        assert_eq!(Url::parse("http://[::1]:8080/x").unwrap().authority(), "[::1]:8080");
        assert_eq!(Url::parse("http://host?q=1").unwrap().path, "/?q=1");
        assert!(Url::parse("ftp://x").is_err() && Url::parse("http://u:p@h/").is_err() && Url::parse("http://h:abc/").is_err());
        for h in ["localhost", "127.0.0.1", "127.1.2.3", "::1", "[::1]", "app.localhost", "::ffff:127.0.0.1"] {
            assert!(is_loopback_host(h), "{h}");
        }
        for h in ["example.org", "192.168.1.5", "10.0.0.1", "0.0.0.0", "localhost.evil.com", "127.0.0.1.nip.io"] {
            assert!(!is_loopback_host(h), "{h}");
        }
    }

    fn serve_once(response: &'static [u8]) -> (u16, std::thread::JoinHandle<String>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let h = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let mut buf = [0u8; 4096];
            let n = s.read(&mut buf).unwrap();
            s.write_all(response).unwrap();
            String::from_utf8_lossy(&buf[..n]).into_owned()
        });
        (port, h)
    }

    #[test]
    fn a_loopback_get_reads_status_headers_and_a_chunked_body() {
        let (port, srv) = serve_once(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n7\r\n{\"ok\":t\r\n5\r\nrue}\n\r\n0\r\n\r\n");
        let mut req = Request::get(&format!("http://127.0.0.1:{port}/health?x=1")).unwrap();
        req.headers.push(("X-Test".into(), "yes".into()));
        let c = poll(&req, &Checks { json: vec!["ok=true".into()], headers: vec!["content-type: json".into()], contains: vec!["\"ok\"".into()], ..Checks::default() }, Duration::ZERO, &|| false);
        let sent = srv.join().unwrap();
        assert!(sent.starts_with("GET /health?x=1 HTTP/1.1\r\n") && sent.contains("X-Test: yes") && sent.contains("Connection: close"), "{sent}");
        assert!(c.ok(), "{:?}", c.assertions);
        let text = render(&req, &c);
        assert!(text.contains("-> 200 OK (ok,") && text.contains("checks: 4/4 passed"), "{text}");
    }

    #[test]
    fn failed_checks_are_listed_and_a_refused_connection_is_named() {
        let (port, srv) = serve_once(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 4\r\n\r\nboom");
        let req = Request::get(&format!("http://127.0.0.1:{port}/")).unwrap();
        let c = poll(&req, &Checks { status: Some(StatusSpec::from_json(&serde_json::json!("2xx")).unwrap()), contains: vec!["fine".into()], ..Checks::default() }, Duration::ZERO, &|| false);
        srv.join().unwrap();
        assert!(!c.ok());
        let t = render(&req, &c);
        assert!(t.contains("CHECK FAILED") && t.contains("FAIL status is 2xx  (got 500)") && t.contains("FAIL body contains \"fine\""), "{t}");
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        let req = Request::get(&format!("http://127.0.0.1:{port}/")).unwrap();
        let c = poll(&req, &Checks::default(), Duration::ZERO, &|| false);
        assert!(matches!(c.response, Err(HttpError::Refused(_))) && !c.ok());
        assert!(render(&req, &c).contains("nothing is listening on"));
    }

    #[test]
    fn wait_polls_until_a_late_server_answers() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        let srv = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(1500));
            let l = TcpListener::bind(("127.0.0.1", port)).unwrap();
            let (mut s, _) = l.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = s.read(&mut buf);
            s.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\r\nready").unwrap();
        });
        let req = Request::get(&format!("http://127.0.0.1:{port}/")).unwrap();
        let c = poll(&req, &Checks { contains: vec!["ready".into()], ..Checks::default() }, Duration::from_secs(10), &|| false);
        srv.join().unwrap();
        assert!(c.ok() && c.waited >= Duration::from_millis(1000), "{c:?}");
    }

    #[test]
    fn a_name_that_resolves_off_loopback_and_remote_misuse_are_refused() {
        let r = Request { method: "POST".into(), body: Some("x".into()), ..Request::get("https://example.org/").unwrap() };
        assert!(matches!(send(&r, &|| false), Err(HttpError::Url(m)) if m.contains("plain GET")));
        let r = Request::get("http://example.org/").unwrap();
        assert!(matches!(send(&r, &|| false), Err(HttpError::Url(m)) if m.contains("only https")));
        let r = Request::get("https://localhost:3000/").unwrap();
        assert!(matches!(send(&r, &|| false), Err(HttpError::Url(m)) if m.contains("self-signed")));
    }

    #[test]
    fn status_specs_and_json_paths() {
        let s = StatusSpec::from_json(&serde_json::json!([200, "3xx", "404"])).unwrap();
        assert!(s.accepts(200) && s.accepts(302) && s.accepts(404) && !s.accepts(500));
        assert!(StatusSpec::from_json(&serde_json::json!("ok")).is_err());
        let v: Value = serde_json::json!({"data": {"items": [{"id": 7}, {"id": 8}]}, "n": null});
        assert_eq!(json_at(&v, "data.items.1.id"), Some(&serde_json::json!(8)));
        assert_eq!(json_at(&v, "data.items[0].id"), Some(&serde_json::json!(7)));
        assert!(json_at(&v, "data.nope").is_none());
    }

    #[test]
    fn html_is_shown_as_its_title_and_text() {
        let r = Response {
            status: 200,
            reason: "OK".into(),
            headers: vec![("Content-Type".into(), "text/html".into())],
            body: b"<html><head><title>Hi</title><style>p{}</style></head><body><script>var x=1</script><p>Hello <b>world</b></p></body></html>".to_vec(),
            truncated: false,
            elapsed: Duration::ZERO,
        };
        let t = body_excerpt(&r, 1000);
        assert!(t.starts_with("<title> Hi\nHi Hello world") || t.contains("Hello world"), "{t}");
        assert!(!t.contains("var x"));
    }
}
