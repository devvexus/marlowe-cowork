//! A scripted MCP server for tests (feature `fake-server`): the JSON-RPC logic ([`FakeMcp`]), a
//! stdio binary around it (`marlowe-mcp-fake`) and an in-process loopback HTTP server
//! ([`FakeHttp`]) that also plays the OAuth authorization server. Nothing here is compiled into
//! the product.
//!
//! Tools: `echo{text}`, `add{a,b}`, `big{chars}` (long output), `fail` (isError), `image`,
//! `slow{ms}`, `approve{tool_name,input}` (with `gate`), `add_tool{name}` (adds a tool and sends `tools/list_changed`), `pid`. Resources:
//! `file:///notes.txt`, `file:///data.json`, template `file:///{name}.md`. Prompts: `greet{name}`,
//! `review`. `extra_tools` adds that many filler tools `tool_000...`.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossbeam_channel::{unbounded, Receiver, Sender};
use serde_json::{json, Value};

use crate::util::{base64url, form_decode, sha256_bytes};

/// A 1x1 PNG, the `image` tool's answer.
pub const TINY_PNG_BASE64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

#[derive(Debug, Clone, Default)]
pub struct FakeOpts {
    pub extra_tools: usize,
    /// Protocol version the server answers `initialize` with.
    pub protocol: Option<String>,
    /// Advertise `listChanged` for tools, resources and prompts.
    pub list_changed: bool,
    /// Replaces the `echo` tool's description.
    pub echo_description: Option<String>,
    /// Adds `approve{tool_name,input}`, a permission prompt tool: it denies a `Bash` input whose
    /// command contains `rm ` and allows everything else.
    pub gate: bool,
}

struct FakeState {
    tools: Vec<Value>,
    initialized: bool,
}

pub struct FakeMcp {
    opts: FakeOpts,
    state: Mutex<FakeState>,
}

fn tool(name: &str, description: &str, props: Value, required: &[&str]) -> Value {
    json!({"name": name, "description": description, "inputSchema": {"type": "object", "properties": props, "required": required}})
}

impl FakeMcp {
    pub fn new(opts: FakeOpts) -> Self {
        let mut tools = vec![
            tool("echo", opts.echo_description.as_deref().unwrap_or("Echo the text back."), json!({"text": {"type": "string", "description": "what to echo"}}), &["text"]),
            tool("add", "Add two numbers.", json!({"a": {"type": "number"}, "b": {"type": "number"}}), &["a", "b"]),
            tool("big", "Return a long string of the requested size.", json!({"chars": {"type": "integer"}}), &["chars"]),
            tool("fail", "Always reports an error result.", json!({}), &[]),
            tool("image", "Return a tiny image.", json!({}), &[]),
            tool("slow", "Sleep for the given milliseconds, then answer.", json!({"ms": {"type": "integer"}}), &["ms"]),
            tool("add_tool", "Add a tool with the given name and notify listeners.", json!({"name": {"type": "string"}}), &["name"]),
            tool("pid", "Return this server's process id.", json!({}), &[]),
        ];
        if opts.gate {
            tools.push(tool("approve", "Decide whether a tool call may run.", json!({"tool_name": {"type": "string"}, "input": {"type": "object"}}), &["tool_name", "input"]));
        }
        for i in 0..opts.extra_tools {
            tools.push(tool(
                &format!("tool_{i:03}"),
                &format!("Filler tool number {i}: reads record {i} from the {} table and returns its fields.", ["users", "orders", "invoices", "events"][i % 4]),
                json!({"id": {"type": "string", "description": "record id"}, "fields": {"type": "array", "items": {"type": "string"}, "description": "fields to return"}, "limit": {"type": "integer"}}),
                &["id"],
            ));
        }
        Self { opts, state: Mutex::new(FakeState { tools, initialized: false }) }
    }

    /// Answers one inbound message: the response (for a request) followed by any notifications
    /// the call caused.
    pub fn handle(&self, msg: &Value) -> Vec<Value> {
        let Some(method) = msg.get("method").and_then(Value::as_str) else { return Vec::new() };
        let id = msg.get("id").cloned();
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let mut out = Vec::new();
        let reply = |result: Value| json!({"jsonrpc": "2.0", "id": id, "result": result});
        let error = |code: i64, message: &str| json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}});
        let Some(_) = id else {
            if method == "notifications/initialized" {
                self.lock().initialized = true;
            }
            return out;
        };
        match method {
            "initialize" => {
                let lc = self.opts.list_changed;
                out.push(reply(json!({
                    "protocolVersion": self.opts.protocol.clone().unwrap_or_else(|| crate::PROTOCOL_VERSION.to_string()),
                    "capabilities": {"tools": {"listChanged": lc}, "resources": {"listChanged": lc}, "prompts": {"listChanged": lc}},
                    "serverInfo": {"name": "marlowe-mcp-fake", "version": "1.0.0"},
                })));
            }
            "ping" => out.push(reply(json!({}))),
            "tools/list" => out.push(reply(json!({"tools": self.lock().tools.clone()}))),
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(Value::Null);
                if !self.lock().tools.iter().any(|t| t["name"] == name) {
                    out.push(error(-32602, &format!("unknown tool {name}")));
                    return out;
                }
                let text = |t: String| json!({"content": [{"type": "text", "text": t}]});
                match name {
                    "echo" => out.push(reply(text(args["text"].as_str().unwrap_or("").to_string()))),
                    "add" => out.push(reply(text(format!("{}", args["a"].as_f64().unwrap_or(0.0) + args["b"].as_f64().unwrap_or(0.0))))),
                    "big" => out.push(reply(text("x".repeat(args["chars"].as_u64().unwrap_or(0) as usize)))),
                    "fail" => out.push(reply(json!({"content": [{"type": "text", "text": "it failed"}], "isError": true}))),
                    "image" => out.push(reply(json!({"content": [{"type": "image", "data": TINY_PNG_BASE64, "mimeType": "image/png"}]}))),
                    "slow" => {
                        std::thread::sleep(Duration::from_millis(args["ms"].as_u64().unwrap_or(0)));
                        out.push(reply(text("done".into())));
                    }
                    "pid" => out.push(reply(text(std::process::id().to_string()))),
                    "approve" => {
                        let bad = args["tool_name"] == "Bash" && args["input"]["command"].as_str().is_some_and(|c| c.contains("rm "));
                        let answer = if bad { json!({"behavior": "deny", "message": "the gate refuses rm"}) } else { json!({"behavior": "allow", "updatedInput": args["input"]}) };
                        out.push(reply(text(answer.to_string())));
                    }
                    "add_tool" => {
                        let n = args["name"].as_str().unwrap_or("extra").to_string();
                        self.lock().tools.push(tool(&n, "A tool added at run time.", json!({}), &[]));
                        out.push(reply(text(format!("added {n}"))));
                        out.push(json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"}));
                    }
                    _ => out.push(reply(text(format!("{name} called with {args}")))),
                }
            }
            "resources/list" => out.push(reply(json!({"resources": [
                {"uri": "file:///notes.txt", "name": "notes", "description": "Plain notes", "mimeType": "text/plain"},
                {"uri": "file:///data.json", "name": "data", "mimeType": "application/json"},
            ]}))),
            "resources/templates/list" => out.push(reply(json!({"resourceTemplates": [
                {"uriTemplate": "file:///{name}.md", "name": "markdown", "description": "A markdown file by name"},
            ]}))),
            "resources/read" => {
                let uri = params.get("uri").and_then(Value::as_str).unwrap_or("");
                match uri {
                    "file:///notes.txt" => out.push(reply(json!({"contents": [{"uri": uri, "mimeType": "text/plain", "text": "remember the milk"}]}))),
                    "file:///data.json" => out.push(reply(json!({"contents": [{"uri": uri, "mimeType": "application/json", "text": "{\"k\":1}"}]}))),
                    u if u.starts_with("file:///") && u.ends_with(".md") => {
                        out.push(reply(json!({"contents": [{"uri": u, "mimeType": "text/markdown", "text": format!("# {}", &u[8..u.len() - 3])}]})))
                    }
                    _ => out.push(error(-32002, "resource not found")),
                }
            }
            "prompts/list" => out.push(reply(json!({"prompts": [
                {"name": "greet", "description": "Greet someone", "arguments": [{"name": "name", "description": "who", "required": true}]},
                {"name": "review", "description": "Review the diff"},
            ]}))),
            "prompts/get" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let who = params["arguments"]["name"].as_str().unwrap_or("there");
                match name {
                    "greet" => out.push(reply(json!({"description": "Greet someone", "messages": [
                        {"role": "user", "content": {"type": "text", "text": format!("Say hello to {who}.")}}
                    ]}))),
                    "review" => out.push(reply(json!({"messages": [
                        {"role": "user", "content": {"type": "text", "text": "Review the staged changes."}}
                    ]}))),
                    _ => out.push(error(-32602, "unknown prompt")),
                }
            }
            "logging/setLevel" => out.push(reply(json!({}))),
            other => out.push(error(-32601, &format!("method not found: {other}"))),
        }
        out
    }

    pub fn tool_names(&self) -> Vec<String> {
        self.lock().tools.iter().filter_map(|t| t["name"].as_str().map(str::to_string)).collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FakeState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Runs the stdio server: one JSON message per line on stdin, answers on stdout.
pub fn run_stdio(opts: FakeOpts, args: &[String]) {
    let fake = FakeMcp::new(opts);
    // `--dump-env PATH`: proof of what the process started with (and that it started).
    if let Some(path) = args.iter().position(|a| a == "--dump-env").and_then(|i| args.get(i + 1)) {
        let dump: String = std::env::vars().map(|(k, v)| format!("{k}={v}
")).collect();
        let _ = std::fs::write(path, dump);
    }
    if args.iter().any(|a| a == "--stderr-note") {
        eprintln!("fake server booting");
    }
    let exit_after_init = args.iter().any(|a| a == "--exit-after-init");
    let stdout = std::io::stdout();
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
        for out in fake.handle(&msg) {
            let mut o = stdout.lock();
            let _ = writeln!(o, "{out}");
            let _ = o.flush();
        }
        if exit_after_init && msg["method"] == "notifications/initialized" {
            eprintln!("fake server: exiting as asked");
            std::process::exit(3);
        }
    }
}

// ---------------------------------------------------------------- HTTP

struct Req {
    method: String,
    path: String,
    query: String,
    headers: HashMap<String, String>,
    body: String,
}

enum Resp {
    Plain(u16, Vec<(String, String)>, String),
    Sse(Vec<(String, String)>, Receiver<String>),
}

fn read_request(s: &TcpStream) -> Option<Req> {
    let mut r = BufReader::new(s.try_clone().ok()?);
    let mut line = String::new();
    r.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();
    let (path, query) = target.split_once('?').map_or((target.clone(), String::new()), |(p, q)| (p.to_string(), q.to_string()));
    let mut headers = HashMap::new();
    loop {
        let mut l = String::new();
        if r.read_line(&mut l).ok()? == 0 || l.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = l.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let len: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).ok()?;
    Some(Req { method, path, query, headers, body: String::from_utf8_lossy(&body).into_owned() })
}

fn write_response(mut s: TcpStream, resp: Resp) {
    match resp {
        Resp::Plain(status, headers, body) => {
            let reason = match status {
                200 => "OK",
                202 => "Accepted",
                302 => "Found",
                400 => "Bad Request",
                401 => "Unauthorized",
                404 => "Not Found",
                405 => "Method Not Allowed",
                _ => "Status",
            };
            let mut head = format!("HTTP/1.1 {status} {reason}\r\n");
            for (k, v) in headers {
                head.push_str(&format!("{k}: {v}\r\n"));
            }
            head.push_str(&format!("Content-Length: {}\r\nConnection: close\r\n\r\n", body.len()));
            let _ = s.write_all(head.as_bytes()).and_then(|()| s.write_all(body.as_bytes()));
        }
        Resp::Sse(headers, rx) => {
            let mut head = String::from("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n");
            for (k, v) in headers {
                head.push_str(&format!("{k}: {v}\r\n"));
            }
            head.push_str("\r\n");
            let _ = s.write_all(head.as_bytes());
            let _ = s.flush();
            while let Ok(ev) = rx.recv() {
                if s.write_all(format!("event: message\ndata: {ev}\n\n").as_bytes()).and_then(|()| s.flush()).is_err() {
                    break;
                }
            }
        }
    }
}

/// How the fake HTTP server behaves.
#[derive(Debug, Clone, Default)]
pub struct HttpOpts {
    pub mcp: FakeOpts,
    /// Require `Authorization: Bearer` from the OAuth flow.
    pub oauth: bool,
    /// Answer POSTs as an event stream instead of a JSON body.
    pub sse: bool,
    /// Hand out a session id on `initialize` and require it afterwards.
    pub sessions: bool,
    /// Offer the GET stream for server-initiated messages.
    pub push_stream: bool,
    /// Access-token lifetime in seconds.
    pub token_ttl: i64,
    /// What the authorization-server metadata claims as the authorization endpoint (a hostile
    /// server's trick), instead of this server's own.
    pub authorization_endpoint: Option<String>,
}

#[derive(Default)]
struct OAuthState {
    clients: HashSet<String>,
    codes: HashMap<String, (String, String, String)>, // code -> (client_id, challenge, redirect_uri)
    access: HashSet<String>,
    refresh: HashMap<String, String>, // refresh token -> client_id
    next: u64,
}

pub struct FakeHttp {
    pub port: u16,
    inner: Arc<HttpInner>,
}

struct HttpInner {
    opts: HttpOpts,
    mcp: FakeMcp,
    oauth: Mutex<OAuthState>,
    sessions: Mutex<HashSet<String>>,
    log: Mutex<Vec<String>>,
    push: Mutex<Vec<Sender<String>>>,
    stop: AtomicBool,
    seq: AtomicU64,
}

impl FakeHttp {
    pub fn start(opts: HttpOpts) -> FakeHttp {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let inner = Arc::new(HttpInner {
            mcp: FakeMcp::new(opts.mcp.clone()),
            opts,
            oauth: Mutex::new(OAuthState::default()),
            sessions: Mutex::new(HashSet::new()),
            log: Mutex::new(Vec::new()),
            push: Mutex::new(Vec::new()),
            stop: AtomicBool::new(false),
            seq: AtomicU64::new(1),
        });
        let i = inner.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                if i.stop.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(s) = conn else { continue };
                let i = i.clone();
                std::thread::spawn(move || {
                    if let Some(req) = read_request(&s) {
                        let resp = i.route(req, port);
                        write_response(s, resp);
                    }
                });
            }
        });
        FakeHttp { port, inner }
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/mcp", self.port)
    }

    /// Request lines the server has seen, as `METHOD /path`.
    pub fn log(&self) -> Vec<String> {
        self.inner.log.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn tool_names(&self) -> Vec<String> {
        self.inner.mcp.tool_names()
    }

    /// Sends a server-initiated message on every open GET stream.
    pub fn push(&self, msg: &Value) {
        for tx in self.inner.push.lock().unwrap_or_else(|e| e.into_inner()).iter() {
            let _ = tx.send(msg.to_string());
        }
    }

    /// Forgets every issued access token (the next call is a 401).
    pub fn expire_access_tokens(&self) {
        self.inner.oauth.lock().unwrap_or_else(|e| e.into_inner()).access.clear();
    }

    /// Forgets refresh tokens too (the authorization is revoked).
    pub fn revoke_all(&self) {
        let mut o = self.inner.oauth.lock().unwrap_or_else(|e| e.into_inner());
        o.access.clear();
        o.refresh.clear();
    }

    pub fn stop(&self) {
        self.inner.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
    }
}

impl HttpInner {
    fn route(&self, req: Req, port: u16) -> Resp {
        self.log.lock().unwrap_or_else(|e| e.into_inner()).push(format!("{} {}", req.method, req.path));
        let origin = format!("http://127.0.0.1:{port}");
        let json = |status: u16, v: Value| Resp::Plain(status, vec![("Content-Type".into(), "application/json".into())], v.to_string());
        match (req.method.as_str(), req.path.as_str()) {
            ("GET", "/.well-known/oauth-protected-resource" | "/.well-known/oauth-protected-resource/mcp") if self.opts.oauth => {
                json(200, json!({"resource": format!("{origin}/mcp"), "authorization_servers": [origin]}))
            }
            ("GET", "/.well-known/oauth-authorization-server") if self.opts.oauth => json(
                200,
                json!({
                    "issuer": origin,
                    "authorization_endpoint": self.opts.authorization_endpoint.clone().unwrap_or_else(|| format!("{origin}/authorize")),
                    "token_endpoint": format!("{origin}/token"),
                    "registration_endpoint": format!("{origin}/register"),
                    "code_challenge_methods_supported": ["S256"],
                    "scopes_supported": ["mcp"],
                }),
            ),
            ("POST", "/register") if self.opts.oauth => {
                let n = self.seq.fetch_add(1, Ordering::SeqCst);
                let id = format!("client-{n}");
                self.oauth.lock().unwrap_or_else(|e| e.into_inner()).clients.insert(id.clone());
                json(201, json!({"client_id": id}))
            }
            ("GET", "/authorize") if self.opts.oauth => {
                let q: HashMap<String, String> = form_decode(&req.query).into_iter().collect();
                let ok = q.get("response_type").is_some_and(|v| v == "code")
                    && q.get("code_challenge_method").is_some_and(|v| v == "S256")
                    && q.get("resource").is_some_and(|r| r.ends_with("/mcp"));
                let client = q.get("client_id").cloned().unwrap_or_default();
                let known = self.oauth.lock().unwrap_or_else(|e| e.into_inner()).clients.contains(&client);
                let Some(redirect) = q.get("redirect_uri").filter(|_| ok && known) else {
                    return Resp::Plain(400, vec![], "bad authorization request".into());
                };
                let code = format!("code-{}", self.seq.fetch_add(1, Ordering::SeqCst));
                self.oauth
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .codes
                    .insert(code.clone(), (client, q["code_challenge"].clone(), redirect.clone()));
                let state = q.get("state").cloned().unwrap_or_default();
                Resp::Plain(302, vec![("Location".into(), format!("{redirect}?code={code}&state={state}"))], String::new())
            }
            ("POST", "/token") if self.opts.oauth => self.token(&req, &json),
            ("POST", "/mcp") => self.mcp_post(&req, port),
            ("GET", "/mcp") if self.opts.push_stream => {
                if let Err(r) = self.authorize(&req, port) {
                    return r;
                }
                let (tx, rx) = unbounded();
                self.push.lock().unwrap_or_else(|e| e.into_inner()).push(tx);
                Resp::Sse(Vec::new(), rx)
            }
            ("GET", "/mcp") => Resp::Plain(405, vec![], String::new()),
            ("DELETE", "/mcp") => {
                if let Some(s) = req.headers.get("mcp-session-id") {
                    self.sessions.lock().unwrap_or_else(|e| e.into_inner()).remove(s);
                }
                Resp::Plain(200, vec![], String::new())
            }
            _ => Resp::Plain(404, vec![], "not found".into()),
        }
    }

    fn token(&self, req: &Req, json: &dyn Fn(u16, Value) -> Resp) -> Resp {
        let f: HashMap<String, String> = form_decode(&req.body).into_iter().collect();
        let mut o = self.oauth.lock().unwrap_or_else(|e| e.into_inner());
        let ttl = if self.opts.token_ttl == 0 { 3600 } else { self.opts.token_ttl };
        let issue = |o: &mut OAuthState, client: String| {
            o.next += 1;
            let (at, rt) = (format!("at-{}", o.next), format!("rt-{}", o.next));
            o.access.insert(at.clone());
            o.refresh.insert(rt.clone(), client);
            json(200, json!({"access_token": at, "refresh_token": rt, "token_type": "Bearer", "expires_in": ttl, "scope": "mcp"}))
        };
        match f.get("grant_type").map(String::as_str) {
            Some("authorization_code") => {
                let Some((client, challenge, redirect)) = f.get("code").and_then(|c| o.codes.remove(c)) else {
                    return json(400, json!({"error": "invalid_grant"}));
                };
                let verifier = f.get("code_verifier").cloned().unwrap_or_default();
                let good = base64url(&sha256_bytes(verifier.as_bytes())) == challenge
                    && f.get("client_id") == Some(&client)
                    && f.get("redirect_uri") == Some(&redirect);
                if !good {
                    return json(400, json!({"error": "invalid_grant", "error_description": "PKCE or redirect mismatch"}));
                }
                issue(&mut o, client)
            }
            Some("refresh_token") => {
                let Some(client) = f.get("refresh_token").and_then(|r| o.refresh.remove(r)) else {
                    return json(400, json!({"error": "invalid_grant"}));
                };
                issue(&mut o, client)
            }
            _ => json(400, json!({"error": "unsupported_grant_type"})),
        }
    }

    fn authorize(&self, req: &Req, port: u16) -> Result<(), Resp> {
        if !self.opts.oauth {
            return Ok(());
        }
        let token = req.headers.get("authorization").and_then(|a| a.strip_prefix("Bearer ")).unwrap_or("");
        if self.oauth.lock().unwrap_or_else(|e| e.into_inner()).access.contains(token) {
            return Ok(());
        }
        Err(Resp::Plain(
            401,
            vec![("WWW-Authenticate".into(), format!("Bearer resource_metadata=\"http://127.0.0.1:{port}/.well-known/oauth-protected-resource\""))],
            String::new(),
        ))
    }

    fn mcp_post(&self, req: &Req, port: u16) -> Resp {
        if let Err(r) = self.authorize(req, port) {
            return r;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&req.body) else { return Resp::Plain(400, vec![], "bad json".into()) };
        let is_init = msg["method"] == "initialize";
        let mut headers = Vec::new();
        if self.opts.sessions {
            if is_init {
                let s = format!("sess-{}", self.seq.fetch_add(1, Ordering::SeqCst));
                self.sessions.lock().unwrap_or_else(|e| e.into_inner()).insert(s.clone());
                headers.push(("Mcp-Session-Id".to_string(), s));
            } else {
                let known = req.headers.get("mcp-session-id").is_some_and(|s| self.sessions.lock().unwrap_or_else(|e| e.into_inner()).contains(s));
                if !known {
                    return Resp::Plain(404, vec![], "unknown session".into());
                }
            }
        }
        let outs = self.mcp.handle(&msg);
        if msg.get("id").is_none() {
            return Resp::Plain(202, headers, String::new());
        }
        if self.opts.sse {
            let (tx, rx) = unbounded();
            for o in outs {
                let _ = tx.send(o.to_string());
            }
            drop(tx);
            return Resp::Sse(headers, rx);
        }
        // A plain reply carries one message; notifications ride the push stream when there is one.
        let mut it = outs.into_iter();
        let first = it.next().unwrap_or(Value::Null);
        for extra in it {
            self.push.lock().unwrap_or_else(|e| e.into_inner()).iter().for_each(|tx| {
                let _ = tx.send(extra.to_string());
            });
        }
        headers.push(("Content-Type".into(), "application/json".into()));
        Resp::Plain(200, headers, first.to_string())
    }
}
