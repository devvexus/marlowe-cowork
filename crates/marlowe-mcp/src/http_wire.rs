//! The streamable HTTP transport (MCP 2025-06-18): every JSON-RPC message is a POST to one
//! endpoint; the reply is a JSON body or a server-sent-event stream carrying the response and any
//! notifications. After `initialize` the server may name a session (`Mcp-Session-Id`); a GET with
//! `Accept: text/event-stream` carries server-initiated messages when the server offers it.
//!
//! Each POST runs on its own short-lived thread, so a long tool call never holds `send`. A failure
//! becomes a synthesized error response for the request, so the caller fails instead of waiting.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossbeam_channel::{unbounded, Receiver, Sender};
use serde_json::{json, Value};

use crate::http::{HttpBackend, HttpRequest, HttpResponse, SseReader, MAX_BODY};
use crate::rpc::Wire;
use crate::util::Url;
use crate::McpError;

/// Marker the synthesized error carries so the dispatcher can turn it into [`McpError::NeedsAuth`].
pub const NEEDS_AUTH_CODE: i64 = -32001;
/// Same for a session the server no longer knows.
pub const SESSION_GONE_CODE: i64 = -32002;

/// Supplies and renews the `Authorization` header (OAuth, or a static token).
pub trait Authorizer: Send + Sync {
    /// The header value for the next request (`Bearer ...`), if there is a token.
    fn authorization(&self) -> Option<String>;
    /// A 401 came back. Tries to recover (a refresh); true means retry once now.
    fn unauthorized(&self, www_authenticate: Option<&str>) -> bool;
    /// What to tell the user when recovery failed.
    fn needs_auth_note(&self) -> String;
}

pub struct HttpWire {
    server: String,
    url: Url,
    backend: Arc<dyn HttpBackend>,
    headers: Vec<(String, String)>,
    auth: Option<Arc<dyn Authorizer>>,
    session: Arc<Mutex<Option<String>>>,
    protocol: Arc<Mutex<Option<String>>>,
    inbound: Sender<Value>,
    closed: Arc<AtomicBool>,
    read_timeout: Duration,
}

/// Builds the wire and the channel of messages the server sends back.
pub fn connect(
    server: &str,
    url: &Url,
    headers: Vec<(String, String)>,
    backend: Arc<dyn HttpBackend>,
    auth: Option<Arc<dyn Authorizer>>,
    read_timeout: Duration,
) -> (HttpWire, Receiver<Value>) {
    let (tx, rx) = unbounded();
    let wire = HttpWire {
        server: server.to_string(),
        url: url.clone(),
        backend,
        headers,
        auth,
        session: Arc::new(Mutex::new(None)),
        protocol: Arc::new(Mutex::new(None)),
        inbound: tx,
        closed: Arc::new(AtomicBool::new(false)),
        read_timeout,
    };
    (wire, rx)
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl HttpWire {
    pub fn session_id(&self) -> Option<String> {
        lock(&self.session).clone()
    }

    fn request(&self, method: &str, accept: &str, body: Option<&Value>) -> HttpRequest {
        let mut req = match body {
            Some(b) => HttpRequest::post_json(&self.url, b),
            None => HttpRequest::get(&self.url),
        };
        req.method = method.to_string();
        req = req.header("Accept", accept);
        for (k, v) in &self.headers {
            req = req.header(k, v);
        }
        if let Some(a) = self.auth.as_ref().and_then(|a| a.authorization()) {
            req = req.header("Authorization", &a);
        }
        if let Some(s) = lock(&self.session).clone() {
            req = req.header("Mcp-Session-Id", &s);
        }
        if let Some(p) = lock(&self.protocol).clone() {
            req = req.header("MCP-Protocol-Version", &p);
        }
        req
    }

    fn fail(&self, id: Option<&Value>, code: i64, message: String) {
        if let Some(id) = id {
            let _ = self.inbound.send(json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}));
        }
    }

    /// One POST and the handling of its reply.
    fn post(&self, msg: &Value) {
        let id = msg.get("id").filter(|v| !v.is_null()).cloned();
        let is_request = id.is_some() && msg.get("method").is_some();
        for attempt in 0..2 {
            let req = self.request("POST", "application/json, text/event-stream", Some(msg));
            let mut resp = match self.backend.send(&req, self.read_timeout) {
                Ok(r) => r,
                Err(e) => return self.fail(id.as_ref(), -32000, e.to_string()),
            };
            match resp.status {
                200 => {
                    if let Some(s) = resp.header("mcp-session-id") {
                        *lock(&self.session) = Some(s.to_string());
                    }
                    return self.deliver(resp, id.as_ref(), is_request);
                }
                202 | 204 => return,
                401 => {
                    let challenge = resp.header("www-authenticate").map(str::to_string);
                    let retry = attempt == 0 && self.auth.as_ref().is_some_and(|a| a.unauthorized(challenge.as_deref()));
                    if retry {
                        continue;
                    }
                    let note = self.auth.as_ref().map_or_else(|| "the server requires authentication".to_string(), |a| a.needs_auth_note());
                    return self.fail(id.as_ref(), NEEDS_AUTH_CODE, note);
                }
                404 if lock(&self.session).is_some() => {
                    *lock(&self.session) = None;
                    return self.fail(id.as_ref(), SESSION_GONE_CODE, "the server no longer knows this session".into());
                }
                status => {
                    let body = resp.text(500);
                    return self.fail(id.as_ref(), -32000, format!("HTTP {status}: {}", body.trim()));
                }
            }
        }
    }

    fn deliver(&self, mut resp: HttpResponse, id: Option<&Value>, is_request: bool) {
        let mut answered = false;
        let mut forward = |v: Value| {
            if v.get("id") == id && v.get("method").is_none() {
                answered = true;
            }
            let _ = self.inbound.send(v);
        };
        if resp.is_sse() {
            for ev in SseReader::new(&mut resp.body) {
                if ev.event.as_deref().is_some_and(|e| e != "message") || ev.data.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<Value>(&ev.data) {
                    Ok(Value::Array(items)) => items.into_iter().for_each(&mut forward),
                    Ok(v) => forward(v),
                    Err(_) => {}
                }
            }
        } else {
            match resp.json(MAX_BODY) {
                Some(Value::Array(items)) => items.into_iter().for_each(&mut forward),
                Some(v) => forward(v),
                None => {}
            }
        }
        if is_request && !answered {
            self.fail(id, -32000, "the server ended the response without answering".into());
        }
    }

    /// The optional server-to-client stream. A server that offers none answers 405 and this ends.
    fn listen(&self) {
        let mut failures = 0;
        while !self.closed.load(Ordering::SeqCst) && failures < 5 {
            let req = self.request("GET", "text/event-stream", None);
            match self.backend.send(&req, Duration::from_secs(300)) {
                Ok(mut resp) if resp.status == 200 && resp.is_sse() => {
                    failures = 0;
                    for ev in SseReader::new(&mut resp.body) {
                        if self.closed.load(Ordering::SeqCst) {
                            return;
                        }
                        if let Ok(v) = serde_json::from_str::<Value>(&ev.data) {
                            let _ = self.inbound.send(v);
                        }
                    }
                }
                Ok(resp) if matches!(resp.status, 404 | 405 | 501) => return,
                Ok(_) | Err(_) => failures += 1,
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }
}

impl Wire for Arc<HttpWire> {
    fn send(&self, msg: &Value) -> Result<(), McpError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(McpError::Closed { server: self.server.clone(), detail: "the connection is closed".into() });
        }
        let wire = self.clone();
        let msg = msg.clone();
        std::thread::Builder::new()
            .name(format!("mcp-{}-post", self.server))
            .spawn(move || wire.post(&msg))
            .map(|_| ())
            .map_err(|e| McpError::Closed { server: self.server.clone(), detail: e.to_string() })
    }

    fn close(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        // Tell a server that keeps sessions we are done (best effort, local servers only).
        let has_session = lock(&self.session).is_some();
        if has_session && self.url.is_loopback() {
            let req = self.request("DELETE", "application/json", None);
            let _ = self.backend.send(&req, Duration::from_secs(1));
        }
    }

    fn set_protocol(&self, version: &str) {
        *lock(&self.protocol) = Some(version.to_string());
    }

    fn started(&self) {
        let wire = self.clone();
        std::thread::Builder::new().name(format!("mcp-{}-sse", self.server)).spawn(move || wire.listen()).ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::DefaultHttp;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;

    /// A scripted loopback server: each connection is answered by `handle(request_head_and_body)`.
    pub(crate) fn serve(handle: impl Fn(&str, &str) -> Vec<u8> + Send + Sync + 'static) -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let handle = Arc::new(handle);
        std::thread::spawn(move || {
            for conn in l.incoming() {
                let Ok(mut s) = conn else { break };
                let handle = handle.clone();
                std::thread::spawn(move || {
                    let mut r = BufReader::new(s.try_clone().unwrap());
                    let mut head = String::new();
                    let mut len = 0usize;
                    loop {
                        let mut line = String::new();
                        if r.read_line(&mut line).unwrap_or(0) == 0 {
                            return;
                        }
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
                    let out = handle(&head, &String::from_utf8_lossy(&body));
                    let _ = s.write_all(&out);
                });
            }
        });
        port
    }

    fn reply(status: &str, headers: &str, body: &str) -> Vec<u8> {
        format!("HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).into_bytes()
    }

    fn wire(port: u16, auth: Option<Arc<dyn Authorizer>>) -> (Arc<HttpWire>, Receiver<Value>) {
        let url = Url::parse(&format!("http://127.0.0.1:{port}/mcp")).unwrap();
        let (w, rx) = connect("t", &url, vec![("X-Custom".into(), "1".into())], Arc::new(DefaultHttp), auth, Duration::from_secs(5));
        (Arc::new(w), rx)
    }

    #[test]
    fn a_json_reply_and_the_session_header_are_taken_and_reused() {
        let port = serve(|head, body| {
            if body.contains("initialize") {
                reply("200 OK", "Content-Type: application/json\r\nMcp-Session-Id: s1\r\n", r#"{"jsonrpc":"2.0","id":1,"result":{"ok":1}}"#)
            } else {
                assert!(head.contains("Mcp-Session-Id: s1"), "{head}");
                assert!(head.contains("X-Custom: 1"), "{head}");
                reply("200 OK", "Content-Type: application/json\r\n", r#"{"jsonrpc":"2.0","id":2,"result":{"ok":2}}"#)
            }
        });
        let (w, rx) = wire(port, None);
        w.send(&json!({"jsonrpc":"2.0","id":1,"method":"initialize"})).unwrap();
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap()["result"]["ok"], 1);
        assert_eq!(w.session_id().as_deref(), Some("s1"));
        w.send(&json!({"jsonrpc":"2.0","id":2,"method":"tools/list"})).unwrap();
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap()["result"]["ok"], 2);
    }

    #[test]
    fn an_sse_reply_delivers_notifications_then_the_response() {
        let body = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/tools/list_changed\"}\n\nevent: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{}}\n\n";
        let port = serve(move |_, _| reply("200 OK", "Content-Type: text/event-stream\r\n", body));
        let (w, rx) = wire(port, None);
        w.send(&json!({"jsonrpc":"2.0","id":3,"method":"tools/call"})).unwrap();
        let first = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(first["method"], "notifications/tools/list_changed");
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap()["id"], 3);
    }

    struct Token(Mutex<String>, AtomicBool);
    impl Authorizer for Token {
        fn authorization(&self) -> Option<String> {
            Some(format!("Bearer {}", lock(&self.0)))
        }
        fn unauthorized(&self, _: Option<&str>) -> bool {
            *lock(&self.0) = "fresh".into();
            self.1.store(true, Ordering::SeqCst);
            true
        }
        fn needs_auth_note(&self) -> String {
            "run `marlowe mcp auth t`".into()
        }
    }

    #[test]
    fn a_401_is_retried_once_with_the_renewed_token() {
        let port = serve(|head, _| {
            if head.contains("Bearer fresh") {
                reply("200 OK", "Content-Type: application/json\r\n", r#"{"jsonrpc":"2.0","id":1,"result":{"authed":true}}"#)
            } else {
                reply("401 Unauthorized", "WWW-Authenticate: Bearer resource_metadata=\"http://x/.well-known/oauth-protected-resource\"\r\n", "")
            }
        });
        let auth = Arc::new(Token(Mutex::new("stale".into()), AtomicBool::new(false)));
        let (w, rx) = wire(port, Some(auth.clone()));
        w.send(&json!({"jsonrpc":"2.0","id":1,"method":"tools/list"})).unwrap();
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap()["result"]["authed"], true);
        assert!(auth.1.load(Ordering::SeqCst));
    }

    #[test]
    fn an_unrecoverable_401_becomes_a_needs_auth_error_for_the_request() {
        struct Never;
        impl Authorizer for Never {
            fn authorization(&self) -> Option<String> {
                None
            }
            fn unauthorized(&self, _: Option<&str>) -> bool {
                false
            }
            fn needs_auth_note(&self) -> String {
                "run `marlowe mcp auth t`".into()
            }
        }
        let port = serve(|_, _| reply("401 Unauthorized", "", ""));
        let (w, rx) = wire(port, Some(Arc::new(Never)));
        w.send(&json!({"jsonrpc":"2.0","id":9,"method":"initialize"})).unwrap();
        let m = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(m["id"], 9);
        assert_eq!(m["error"]["code"], NEEDS_AUTH_CODE);
        assert!(m["error"]["message"].as_str().unwrap().contains("marlowe mcp auth t"));
    }

    #[test]
    fn a_server_error_fails_the_request_with_its_body() {
        let port = serve(|_, _| reply("500 Internal Server Error", "", "kaput"));
        let (w, rx) = wire(port, None);
        w.send(&json!({"jsonrpc":"2.0","id":4,"method":"tools/list"})).unwrap();
        let m = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(m["error"]["message"].as_str().unwrap().contains("HTTP 500: kaput"), "{m}");
    }

    #[test]
    fn a_notification_gets_no_synthesized_error() {
        let port = serve(|_, _| reply("202 Accepted", "", ""));
        let (w, rx) = wire(port, None);
        w.send(&json!({"jsonrpc":"2.0","method":"notifications/initialized"})).unwrap();
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());
    }
}
