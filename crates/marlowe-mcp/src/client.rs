//! One connected MCP server: the handshake, tools, resources, prompts, and `list_changed`
//! refresh. Built over a [`Peer`] on either wire.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use marlowe_tools::Description;
use serde_json::{json, Map, Value};

use crate::config::{ServerConfig, Transport};
use crate::http::{DefaultHttp, HttpBackend};
use crate::http_wire::{self, Authorizer};
use crate::oauth::{OAuth, TokenStore};
use crate::rpc::{Inbound, Peer, RpcFailure, Wire};
use crate::stdio;
use crate::util::Url;
use crate::{McpError, PROTOCOL_VERSION};

const SUPPORTED_PROTOCOLS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

/// What a connection needs from its surroundings.
#[derive(Clone)]
pub struct ConnectOpts {
    /// Working directory for a stdio server (the session's workspace).
    pub cwd: Option<PathBuf>,
    /// Directories offered to servers that ask for roots.
    pub roots: Vec<PathBuf>,
    pub http: Arc<dyn HttpBackend>,
    pub tokens: Arc<dyn TokenStore>,
    /// Time allowed for `initialize`.
    pub startup_timeout: Duration,
    /// Time allowed for listing calls.
    pub request_timeout: Duration,
    /// Time allowed for one tool call, unless the server's config sets `timeout`.
    pub tool_timeout: Duration,
}

impl ConnectOpts {
    pub fn new(tokens: Arc<dyn TokenStore>) -> Self {
        Self {
            cwd: None,
            roots: Vec::new(),
            http: Arc::new(DefaultHttp),
            tokens,
            startup_timeout: Duration::from_secs(30),
            request_timeout: Duration::from_secs(30),
            tool_timeout: Duration::from_secs(300),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Capabilities {
    pub tools: bool,
    pub resources: bool,
    pub prompts: bool,
}

/// One tool as the server describes it.
#[derive(Debug, Clone, PartialEq)]
pub struct Tool {
    pub name: String,
    /// Sanitised for display and bounded (`marlowe_tools::Description`).
    pub description: String,
    pub input_schema: Value,
    /// `annotations.readOnlyHint`.
    pub read_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resource {
    pub uri: String,
    pub name: String,
    pub description: String,
    pub mime: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceTemplate {
    pub uri_template: String,
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceContent {
    pub uri: String,
    pub mime: Option<String>,
    pub text: Option<String>,
    /// Size of a binary payload that is not shown.
    pub blob_bytes: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptArg {
    pub name: String,
    pub description: String,
    pub required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prompt {
    pub name: String,
    pub description: String,
    pub arguments: Vec<PromptArg>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptMessage {
    pub role: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Content {
    Text(String),
    Image { mime: String, base64_len: usize },
    Audio { mime: String },
    Resource(ResourceContent),
    Link { uri: String, name: String },
}

#[derive(Debug, Clone, PartialEq)]
pub struct CallResult {
    pub content: Vec<Content>,
    pub is_error: bool,
    pub structured: Option<Value>,
}

impl CallResult {
    /// The result as text. Non-text parts are named, not dropped.
    pub fn text(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        for c in &self.content {
            parts.push(match c {
                Content::Text(t) => t.clone(),
                Content::Image { mime, base64_len } => format!("[{mime} image, {base64_len} base64 chars: this build cannot show MCP images]"),
                Content::Audio { mime } => format!("[{mime} audio: this build cannot play MCP audio]"),
                Content::Resource(r) => match &r.text {
                    Some(t) => format!("[resource {}]\n{t}", r.uri),
                    None => format!("[resource {}: {} bytes of binary]", r.uri, r.blob_bytes.unwrap_or(0)),
                },
                Content::Link { uri, name } => format!("[link {name}: {uri}]"),
            });
        }
        if parts.is_empty() {
            if let Some(s) = &self.structured {
                return s.to_string();
            }
        }
        parts.join("\n")
    }
}

#[derive(Default)]
struct Flags {
    tools: AtomicBool,
    resources: AtomicBool,
    prompts: AtomicBool,
    generation: AtomicU64,
}

#[derive(Default)]
struct Cache {
    tools: Option<Vec<Tool>>,
    resources: Option<Vec<Resource>>,
    templates: Option<Vec<ResourceTemplate>>,
    prompts: Option<Vec<Prompt>>,
}

struct Handler {
    flags: Arc<Flags>,
    roots: Vec<PathBuf>,
}

impl Inbound for Handler {
    fn notification(&self, method: &str, _params: &Value) {
        let f = &self.flags;
        let hit = match method {
            "notifications/tools/list_changed" => &f.tools,
            "notifications/resources/list_changed" => &f.resources,
            "notifications/prompts/list_changed" => &f.prompts,
            _ => return,
        };
        hit.store(true, Ordering::SeqCst);
        f.generation.fetch_add(1, Ordering::SeqCst);
    }

    fn request(&self, method: &str, _params: &Value) -> Result<Value, RpcFailure> {
        match method {
            "ping" => Ok(json!({})),
            "roots/list" => Ok(json!({"roots": self.roots.iter().map(|r| json!({"uri": file_uri(r), "name": r.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()})).collect::<Vec<_>>()})),
            // A tool server asking the user a question has nobody to ask here.
            "elicitation/create" => Ok(json!({"action": "decline"})),
            other => Err(RpcFailure::method_not_found(other)),
        }
    }
}

/// `file:///C:/x/y` for a path.
pub fn file_uri(p: &std::path::Path) -> String {
    let s = p.display().to_string().replace('\\', "/");
    let s = s.strip_prefix("//?/").unwrap_or(&s).to_string();
    format!("file:///{}", s.trim_start_matches('/'))
}

pub struct Client {
    name: String,
    peer: Peer,
    caps: Capabilities,
    server_info: String,
    protocol: String,
    instructions: Option<String>,
    flags: Arc<Flags>,
    cache: Mutex<Cache>,
    request_timeout: Duration,
    tool_timeout: Duration,
    oauth: Option<Arc<OAuth>>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client").field("name", &self.name).finish_non_exhaustive()
    }
}

fn never() -> bool {
    false
}

impl Client {
    /// Connects and completes the handshake. `cfg` must already be expanded.
    pub fn connect(cfg: &ServerConfig, opts: &ConnectOpts) -> Result<Client, McpError> {
        let flags = Arc::new(Flags::default());
        let handler = Arc::new(Handler { flags: flags.clone(), roots: opts.roots.clone() });
        let mut oauth = None;
        let peer = match &cfg.transport {
            Transport::Stdio(spec) => {
                let (wire, rx) = stdio::start(&cfg.name, spec, opts.cwd.as_deref())?;
                Peer::start(&cfg.name, Box::new(wire), rx, handler)
            }
            Transport::Http { url, headers, oauth: oauth_cfg } => {
                let u = Url::parse(url).map_err(|e| McpError::Config(format!("{}: {e}", cfg.name)))?;
                if !u.https && !u.is_loopback() {
                    return Err(McpError::Config(format!(
                        "{}: {url} is plain http to a remote host; use https (plain http is only allowed to this machine)",
                        cfg.name
                    )));
                }
                let has_static_auth = headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("authorization"));
                let auth: Option<Arc<dyn Authorizer>> = if has_static_auth {
                    None
                } else {
                    let o = Arc::new(OAuth::new(&cfg.name, &u, oauth_cfg.clone().unwrap_or_default(), opts.http.clone(), opts.tokens.clone()));
                    oauth = Some(o.clone());
                    Some(o)
                };
                let (wire, rx) = http_wire::connect(&cfg.name, &u, headers.clone(), opts.http.clone(), auth, opts.tool_timeout.max(Duration::from_secs(60)));
                let wire: Box<dyn Wire> = Box::new(Arc::new(wire));
                Peer::start(&cfg.name, wire, rx, handler)
            }
            Transport::Sse { .. } => {
                return Err(McpError::Config(format!("{}: the SSE transport is deprecated and not supported; use \"type\": \"http\"", cfg.name)));
            }
        };

        let init = peer.request(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {"roots": {"listChanged": false}},
                "clientInfo": {"name": "marlowe", "version": env!("CARGO_PKG_VERSION")},
            }),
            opts.startup_timeout,
            &never,
        );
        let init = match init {
            Ok(v) => v,
            Err(e) => {
                let e = match (&e, peer.exit_note()) {
                    (McpError::Closed { server, .. }, Some(note)) => McpError::Closed { server: server.clone(), detail: note },
                    _ => e,
                };
                peer.close();
                return Err(e);
            }
        };
        let protocol = init.get("protocolVersion").and_then(Value::as_str).unwrap_or(PROTOCOL_VERSION).to_string();
        if !SUPPORTED_PROTOCOLS.contains(&protocol.as_str()) {
            peer.close();
            return Err(McpError::Protocol { server: cfg.name.clone(), detail: format!("the server speaks protocol {protocol}, which this client does not") });
        }
        peer.set_protocol(&protocol);
        let c = init.get("capabilities").cloned().unwrap_or(Value::Null);
        let caps = Capabilities { tools: c.get("tools").is_some(), resources: c.get("resources").is_some(), prompts: c.get("prompts").is_some() };
        let info = init.get("serverInfo");
        let server_info = format!(
            "{} {}",
            info.and_then(|i| i.get("name")).and_then(Value::as_str).unwrap_or(&cfg.name),
            info.and_then(|i| i.get("version")).and_then(Value::as_str).unwrap_or("")
        )
        .trim()
        .to_string();
        peer.notify("notifications/initialized", json!({}))?;
        peer.started();
        Ok(Client {
            name: cfg.name.clone(),
            peer,
            caps,
            server_info,
            protocol,
            instructions: init.get("instructions").and_then(Value::as_str).map(|s| Description::new(s).text().to_string()),
            flags,
            cache: Mutex::new(Cache::default()),
            request_timeout: cfg.timeout.unwrap_or(opts.request_timeout),
            tool_timeout: cfg.timeout.unwrap_or(opts.tool_timeout),
            oauth,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn capabilities(&self) -> &Capabilities {
        &self.caps
    }

    pub fn server_info(&self) -> &str {
        &self.server_info
    }

    pub fn protocol(&self) -> &str {
        &self.protocol
    }

    pub fn instructions(&self) -> Option<&str> {
        self.instructions.as_deref()
    }

    pub fn is_alive(&self) -> bool {
        !self.peer.is_closed()
    }

    pub fn exit_note(&self) -> Option<String> {
        self.peer.exit_note()
    }

    pub fn oauth(&self) -> Option<&Arc<OAuth>> {
        self.oauth.as_ref()
    }

    /// Changes whenever the server announced a list change; compare to know a cache is stale.
    pub fn generation(&self) -> u64 {
        self.flags.generation.load(Ordering::SeqCst)
    }

    pub fn close(&self) {
        self.peer.close();
    }

    fn ask(&self, method: &str, params: Value, cancelled: &dyn Fn() -> bool) -> Result<Value, McpError> {
        self.peer.request(method, params, self.request_timeout, cancelled)
    }

    /// Follows `nextCursor` and collects `field` from every page.
    fn paged(&self, method: &str, field: &str) -> Result<Vec<Value>, McpError> {
        let mut items = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..50 {
            let params = cursor.as_ref().map_or_else(|| json!({}), |c| json!({"cursor": c}));
            let page = self.ask(method, params, &never)?;
            if let Some(a) = page.get(field).and_then(Value::as_array) {
                items.extend(a.iter().cloned());
            }
            cursor = page.get("nextCursor").and_then(Value::as_str).map(str::to_string);
            if cursor.is_none() {
                break;
            }
        }
        Ok(items)
    }

    // ------------------------------------------------------------ tools

    /// The server's tools, cached until it says the list changed.
    pub fn tools(&self) -> Result<Vec<Tool>, McpError> {
        if !self.caps.tools {
            return Ok(Vec::new());
        }
        if !self.flags.tools.swap(false, Ordering::SeqCst) {
            if let Some(t) = &self.lock().tools {
                return Ok(t.clone());
            }
        }
        let tools: Vec<Tool> = self
            .paged("tools/list", "tools")?
            .iter()
            .filter_map(|t| {
                let name = t.get("name")?.as_str()?.to_string();
                Some(Tool {
                    description: Description::new(t.get("description").and_then(Value::as_str).unwrap_or("")).text().to_string(),
                    input_schema: t.get("inputSchema").cloned().filter(Value::is_object).unwrap_or_else(|| json!({"type": "object", "properties": {}})),
                    read_only: t.pointer("/annotations/readOnlyHint").and_then(Value::as_bool).unwrap_or(false),
                    name,
                })
            })
            .collect();
        self.lock().tools = Some(tools.clone());
        Ok(tools)
    }

    pub fn call_tool(&self, tool: &str, args: Value, cancelled: &dyn Fn() -> bool) -> Result<CallResult, McpError> {
        let args = if args.is_null() { json!({}) } else { args };
        let reply = self.peer.request("tools/call", json!({"name": tool, "arguments": args}), self.tool_timeout, cancelled)?;
        let content = reply.get("content").and_then(Value::as_array).map(|a| a.iter().map(parse_content).collect()).unwrap_or_default();
        Ok(CallResult {
            content,
            is_error: reply.get("isError").and_then(Value::as_bool).unwrap_or(false),
            structured: reply.get("structuredContent").cloned(),
        })
    }

    // ------------------------------------------------------------ resources

    pub fn resources(&self) -> Result<Vec<Resource>, McpError> {
        if !self.caps.resources {
            return Ok(Vec::new());
        }
        let dirty = self.flags.resources.swap(false, Ordering::SeqCst);
        if !dirty {
            if let Some(r) = &self.lock().resources {
                return Ok(r.clone());
            }
        }
        let list: Vec<Resource> = self
            .paged("resources/list", "resources")?
            .iter()
            .filter_map(|r| {
                Some(Resource {
                    uri: r.get("uri")?.as_str()?.to_string(),
                    name: r.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
                    description: r.get("description").and_then(Value::as_str).unwrap_or("").to_string(),
                    mime: r.get("mimeType").and_then(Value::as_str).map(str::to_string),
                })
            })
            .collect();
        let mut c = self.lock();
        c.resources = Some(list.clone());
        c.templates = None;
        Ok(list)
    }

    pub fn resource_templates(&self) -> Result<Vec<ResourceTemplate>, McpError> {
        if !self.caps.resources {
            return Ok(Vec::new());
        }
        if let Some(t) = &self.lock().templates {
            return Ok(t.clone());
        }
        // Templates are optional: a server without them answers -32601.
        let raw = match self.paged("resources/templates/list", "resourceTemplates") {
            Ok(r) => r,
            Err(McpError::Remote { .. }) => Vec::new(),
            Err(e) => return Err(e),
        };
        let list: Vec<ResourceTemplate> = raw
            .iter()
            .filter_map(|r| {
                Some(ResourceTemplate {
                    uri_template: r.get("uriTemplate")?.as_str()?.to_string(),
                    name: r.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
                    description: r.get("description").and_then(Value::as_str).unwrap_or("").to_string(),
                })
            })
            .collect();
        self.lock().templates = Some(list.clone());
        Ok(list)
    }

    pub fn read_resource(&self, uri: &str, cancelled: &dyn Fn() -> bool) -> Result<Vec<ResourceContent>, McpError> {
        let reply = self.ask("resources/read", json!({"uri": uri}), cancelled)?;
        Ok(reply.get("contents").and_then(Value::as_array).map(|a| a.iter().filter_map(parse_resource_content).collect()).unwrap_or_default())
    }

    // ------------------------------------------------------------ prompts

    pub fn prompts(&self) -> Result<Vec<Prompt>, McpError> {
        if !self.caps.prompts {
            return Ok(Vec::new());
        }
        if !self.flags.prompts.swap(false, Ordering::SeqCst) {
            if let Some(p) = &self.lock().prompts {
                return Ok(p.clone());
            }
        }
        let list: Vec<Prompt> = self
            .paged("prompts/list", "prompts")?
            .iter()
            .filter_map(|p| {
                Some(Prompt {
                    name: p.get("name")?.as_str()?.to_string(),
                    description: p.get("description").and_then(Value::as_str).unwrap_or("").to_string(),
                    arguments: p
                        .get("arguments")
                        .and_then(Value::as_array)
                        .map(|a| {
                            a.iter()
                                .filter_map(|x| {
                                    Some(PromptArg {
                                        name: x.get("name")?.as_str()?.to_string(),
                                        description: x.get("description").and_then(Value::as_str).unwrap_or("").to_string(),
                                        required: x.get("required").and_then(Value::as_bool).unwrap_or(false),
                                    })
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                })
            })
            .collect();
        self.lock().prompts = Some(list.clone());
        Ok(list)
    }

    pub fn get_prompt(&self, name: &str, args: &Map<String, Value>, cancelled: &dyn Fn() -> bool) -> Result<Vec<PromptMessage>, McpError> {
        let reply = self.ask("prompts/get", json!({"name": name, "arguments": args}), cancelled)?;
        Ok(reply
            .get("messages")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .map(|m| PromptMessage {
                        role: m.get("role").and_then(Value::as_str).unwrap_or("user").to_string(),
                        text: CallResult { content: vec![parse_content(m.get("content").unwrap_or(&Value::Null))], is_error: false, structured: None }.text(),
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Cache> {
        self.cache.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.peer.close();
    }
}

fn parse_resource_content(r: &Value) -> Option<ResourceContent> {
    Some(ResourceContent {
        uri: r.get("uri")?.as_str()?.to_string(),
        mime: r.get("mimeType").and_then(Value::as_str).map(str::to_string),
        text: r.get("text").and_then(Value::as_str).map(str::to_string),
        blob_bytes: r.get("blob").and_then(Value::as_str).map(|b| b.len() / 4 * 3),
    })
}

fn parse_content(c: &Value) -> Content {
    let s = |k: &str| c.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    match c.get("type").and_then(Value::as_str) {
        Some("text") => Content::Text(s("text")),
        Some("image") => Content::Image { mime: s("mimeType"), base64_len: s("data").len() },
        Some("audio") => Content::Audio { mime: s("mimeType") },
        Some("resource") => c.get("resource").and_then(parse_resource_content).map_or_else(|| Content::Text(String::new()), Content::Resource),
        Some("resource_link") => Content::Link { uri: s("uri"), name: s("name") },
        Some(other) => Content::Text(format!("[{other} content this build cannot read]")),
        None => Content::Text(String::new()),
    }
}
