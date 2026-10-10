//! The set of MCP servers one workspace uses: configuration merged from every scope, a connection
//! thread per server, status, the model-visible tool list, calls, resources and prompts.
//!
//! Safety gates, in the order a server meets them:
//! 1. **Trust.** An entry from the workspace's own files (`.mcp.json`, `.marlowe\mcp.local.json`)
//!    does not start until the user trusted that exact entry.
//! 2. **Credentials.** Variable expansion refuses credential variables (see `config`), and the
//!    child starts through `marlowe_os::child`.
//! 3. **Pinned descriptions** (ADR-052 section 4). The first connect records a digest of every
//!    tool's description and schema. Later, a tool whose text changed, or that is new, is withheld
//!    from the model until the user approves it, because the text the user inspected must be the
//!    text the model reads.
//! 4. **Output** is capped and, by the caller, treated as untrusted content (ADR-052).

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use marlowe_tools::ConsequenceLevel;
use serde_json::{json, Map, Value};

use crate::client::{Client, ConnectOpts, Prompt, PromptMessage, Resource, ResourceContent, ResourceTemplate, Tool};
use crate::config::{self, Loaded, Paths, Scope, ServerConfig, Transport, Trust};
use crate::names;
use crate::oauth::OAuth;
use crate::output::{self, Capped};
use crate::search::{self, Entry};
use crate::util::{sha256_hex, Url};
use crate::McpError;

/// A crashed server is restarted by the next call at most this often.
const RECONNECT_GAP: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct HubOpts {
    pub paths: Paths,
    /// `--mcp-config` entries.
    pub session: Vec<ServerConfig>,
    /// Use only `session` (`--strict-mcp-config`).
    pub strict: bool,
    pub env: Arc<dyn Fn(&str) -> Option<String> + Send + Sync>,
    pub connect: ConnectOpts,
    pub max_output_tokens: usize,
    /// Where over-long output is saved in full.
    pub spill_dir: Option<PathBuf>,
}

impl HubOpts {
    pub fn new(paths: Paths, connect: ConnectOpts) -> Self {
        Self {
            paths,
            session: Vec::new(),
            strict: false,
            env: Arc::new(|k| std::env::var(k).ok()),
            connect,
            max_output_tokens: output::DEFAULT_MAX_TOKENS,
            spill_dir: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlotStatus {
    Connecting,
    Connected,
    Failed(String),
    NeedsAuth(String),
    NeedsTrust,
    Disabled,
    /// Connected, but these tools changed or are new since the user last approved them.
    NeedsReview(Vec<String>),
}

impl SlotStatus {
    /// The words `McpServerInfo.status` carries.
    pub fn label(&self) -> String {
        match self {
            SlotStatus::Connecting => "connecting".into(),
            SlotStatus::Connected => "connected".into(),
            SlotStatus::Failed(why) => format!("failed: {why}"),
            SlotStatus::NeedsAuth(_) => "needs auth".into(),
            SlotStatus::NeedsTrust => "needs trust".into(),
            SlotStatus::Disabled => "disabled".into(),
            SlotStatus::NeedsReview(t) => format!("connected; {} tool(s) need review: {}", t.len(), t.join(", ")),
        }
    }
}

enum State {
    Connecting,
    Ready(Arc<Client>),
    Failed(String),
    NeedsAuth(String),
    NeedsTrust,
    Disabled,
}

struct Slot {
    cfg: ServerConfig,
    digest: String,
    state: Mutex<State>,
    withheld: Mutex<Vec<String>>,
    last_attempt: Mutex<Option<Instant>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// One tool as the model sees it.
#[derive(Debug, Clone)]
pub struct ExposedTool {
    pub exposed: String,
    pub server: String,
    pub tool: Tool,
    pub consequence: ConsequenceLevel,
}

impl ExposedTool {
    fn entry(&self) -> Entry {
        Entry {
            exposed: self.exposed.clone(),
            server: self.server.clone(),
            tool: self.tool.name.clone(),
            description: self.tool.description.clone(),
            schema: self.tool.input_schema.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerStatus {
    pub name: String,
    pub scope: Scope,
    pub transport: &'static str,
    pub tools: u32,
    pub status: SlotStatus,
    pub host: Option<String>,
}

/// The result of a tool call, already capped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    pub text: String,
    pub is_error: bool,
    pub capped: Capped,
}

pub struct Hub {
    opts: HubOpts,
    slots: Mutex<Vec<Arc<Slot>>>,
    problems: Mutex<Vec<String>>,
}

impl Hub {
    /// Loads the configuration and starts connecting. Returns at once.
    pub fn start(opts: HubOpts) -> Arc<Hub> {
        let hub = Arc::new(Hub { opts, slots: Mutex::new(Vec::new()), problems: Mutex::new(Vec::new()) });
        hub.reload();
        hub
    }

    pub fn opts(&self) -> &HubOpts {
        &self.opts
    }

    /// Re-reads the configuration. Servers whose entry is unchanged and that are connected (or
    /// connecting) are kept; everything else is replaced, so a retry after `marlowe mcp auth` or
    /// `trust` is just a reload.
    pub fn reload(self: &Arc<Self>) {
        let loaded: Loaded = if self.opts.strict {
            Loaded { servers: self.opts.session.clone(), problems: Vec::new() }
        } else {
            config::load_all(&self.opts.paths, &self.opts.session)
        };
        *lock(&self.problems) = loaded.problems;
        let mut slots = lock(&self.slots);
        let old = std::mem::take(&mut *slots);
        let mut fresh = Vec::new();
        for cfg in loaded.servers {
            let digest = cfg.digest();
            let kept = old.iter().find(|s| s.cfg.name == cfg.name && s.digest == digest && s.cfg.scope == cfg.scope).filter(|s| {
                matches!(*lock(&s.state), State::Ready(_) | State::Connecting | State::Disabled)
            });
            if let Some(k) = kept {
                fresh.push(k.clone());
                continue;
            }
            let slot = Arc::new(Slot {
                cfg,
                digest,
                state: Mutex::new(State::Connecting),
                withheld: Mutex::new(Vec::new()),
                last_attempt: Mutex::new(None),
            });
            fresh.push(slot.clone());
            let hub = self.clone();
            let name = format!("mcp-connect-{}", slot.cfg.name);
            if std::thread::Builder::new().name(name).spawn(move || hub.connect(&slot)).is_err() {
                // No thread: the slot stays `connecting`; the next reload retries.
            }
        }
        *slots = fresh;
        // Servers dropped from the list close with their last Arc.
        drop(old);
    }

    /// Configuration problems found by the last load (a bad entry, a broken file).
    pub fn problems(&self) -> Vec<String> {
        lock(&self.problems).clone()
    }

    fn connect(&self, slot: &Arc<Slot>) {
        *lock(&slot.last_attempt) = Some(Instant::now());
        let set = |s: State| *lock(&slot.state) = s;
        let cfg = &slot.cfg;
        if cfg.disabled {
            return set(State::Disabled);
        }
        let workspace = self.opts.paths.workspace.as_deref();
        if !Trust::new(&self.opts.paths).is_trusted(workspace, cfg) {
            return set(State::NeedsTrust);
        }
        let expanded = match cfg.expanded(&*self.opts.env) {
            Ok(c) => c,
            Err(e) => return set(State::Failed(e)),
        };
        let mut copts = self.opts.connect.clone();
        if copts.cwd.is_none() {
            copts.cwd = self.opts.paths.workspace.clone();
        }
        match Client::connect(&expanded, &copts) {
            Ok(client) => {
                let client = Arc::new(client);
                match client.tools() {
                    Ok(tools) => {
                        *lock(&slot.withheld) = self.check_pins(slot, &tools);
                        set(State::Ready(client));
                    }
                    Err(McpError::NeedsAuth { detail, .. }) => set(State::NeedsAuth(detail)),
                    Err(e) => set(State::Failed(e.to_string())),
                }
            }
            Err(McpError::NeedsAuth { detail, .. }) => set(State::NeedsAuth(detail)),
            Err(e) => set(State::Failed(e.to_string())),
        }
    }

    // ------------------------------------------------------------ pinned descriptions

    fn pin_digest(t: &Tool) -> String {
        sha256_hex(format!("{}\n{}", t.description, t.input_schema).as_bytes())
    }

    fn read_pins(&self) -> Value {
        std::fs::read_to_string(self.opts.paths.pins_file()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_else(|| json!({}))
    }

    fn write_pins(&self, pins: &Value) {
        let path = self.opts.paths.pins_file();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&path, serde_json::to_string_pretty(pins).unwrap_or_default());
    }

    /// First sight of a server pins everything; afterwards a changed or new tool is withheld.
    fn check_pins(&self, slot: &Slot, tools: &[Tool]) -> Vec<String> {
        let mut pins = self.read_pins();
        let key = &slot.digest;
        let current: Map<String, Value> = tools.iter().map(|t| (t.name.clone(), Value::String(Self::pin_digest(t)))).collect();
        match pins.get(key).and_then(Value::as_object) {
            None => {
                pins[key] = Value::Object(current);
                self.write_pins(&pins);
                Vec::new()
            }
            Some(pinned) => tools.iter().filter(|t| pinned.get(&t.name) != current.get(&t.name)).map(|t| t.name.clone()).collect(),
        }
    }

    /// Records the server's current tools as approved and makes them available. Returns how many
    /// were approved.
    pub fn approve_tools(&self, server: &str) -> Result<usize, McpError> {
        let slot = self.slot(server).ok_or_else(|| McpError::Config(format!("no MCP server named {server}")))?;
        let client = match &*lock(&slot.state) {
            State::Ready(c) => c.clone(),
            _ => return Err(McpError::Config(format!("{server} is not connected"))),
        };
        let tools = client.tools()?;
        let mut pins = self.read_pins();
        pins[&slot.digest] = Value::Object(tools.iter().map(|t| (t.name.clone(), Value::String(Self::pin_digest(t)))).collect());
        self.write_pins(&pins);
        lock(&slot.withheld).clear();
        Ok(tools.len())
    }

    // ------------------------------------------------------------ status and tools

    fn slot(&self, name: &str) -> Option<Arc<Slot>> {
        lock(&self.slots).iter().find(|s| s.cfg.name == name).cloned()
    }

    fn all(&self) -> Vec<Arc<Slot>> {
        lock(&self.slots).clone()
    }

    /// Waits until no server is still connecting, or `grace` passes. True if all settled.
    pub fn wait_settled(&self, grace: Duration) -> bool {
        let end = Instant::now() + grace;
        loop {
            let busy = self.all().iter().any(|s| matches!(*lock(&s.state), State::Connecting));
            if !busy {
                return true;
            }
            if Instant::now() >= end {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    pub fn is_empty(&self) -> bool {
        lock(&self.slots).is_empty()
    }

    pub fn statuses(&self) -> Vec<ServerStatus> {
        self.all()
            .iter()
            .map(|s| {
                let (status, tools) = match &*lock(&s.state) {
                    State::Connecting => (SlotStatus::Connecting, 0),
                    State::Ready(c) => {
                        let n = c.tools().map_or(0, |t| t.len() as u32);
                        let held = lock(&s.withheld).clone();
                        if held.is_empty() {
                            (SlotStatus::Connected, n)
                        } else {
                            let visible = n.saturating_sub(held.len() as u32);
                            (SlotStatus::NeedsReview(held), visible)
                        }
                    }
                    State::Failed(e) => (SlotStatus::Failed(e.clone()), 0),
                    State::NeedsAuth(e) => (SlotStatus::NeedsAuth(e.clone()), 0),
                    State::NeedsTrust => (SlotStatus::NeedsTrust, 0),
                    State::Disabled => (SlotStatus::Disabled, 0),
                };
                ServerStatus { name: s.cfg.name.clone(), scope: s.cfg.scope, transport: s.cfg.transport.name(), tools, status, host: s.cfg.host() }
            })
            .collect()
    }

    /// Every available tool, stable-ordered by exposed name.
    pub fn tools(&self) -> Vec<ExposedTool> {
        let mut out: BTreeMap<String, ExposedTool> = BTreeMap::new();
        for slot in self.all() {
            let Some(client) = self.ready(&slot) else { continue };
            let Ok(tools) = client.tools() else {
                self.note_dead(&slot, &client);
                continue;
            };
            let held = lock(&slot.withheld).clone();
            for t in tools.into_iter().filter(|t| !held.contains(&t.name)) {
                let mut exposed = names::exposed(&slot.cfg.name, &t.name);
                let mut n = 2;
                while out.contains_key(&exposed) {
                    exposed = format!("{}_{n}", &names::exposed(&slot.cfg.name, &t.name)[..names::exposed(&slot.cfg.name, &t.name).len().min(names::MAX_NAME - 3)]);
                    n += 1;
                }
                let consequence = slot.cfg.tool_consequences.get(&t.name).copied().unwrap_or(slot.cfg.consequence);
                out.insert(exposed.clone(), ExposedTool { exposed, server: slot.cfg.name.clone(), tool: t, consequence });
            }
        }
        out.into_values().collect()
    }

    fn ready(&self, slot: &Arc<Slot>) -> Option<Arc<Client>> {
        match &*lock(&slot.state) {
            State::Ready(c) => Some(c.clone()),
            _ => None,
        }
    }

    /// A server that died is marked failed with its exit note.
    fn note_dead(&self, slot: &Arc<Slot>, client: &Arc<Client>) {
        if !client.is_alive() {
            let why = client.exit_note().unwrap_or_else(|| "the connection closed".into());
            *lock(&slot.state) = State::Failed(why);
        }
    }

    pub fn find(&self, exposed: &str) -> Option<ExposedTool> {
        self.tools().into_iter().find(|t| t.exposed == exposed)
    }

    /// Ranked search over the available tools, or `select:` by name.
    pub fn search(&self, query: &str, limit: usize) -> Vec<ExposedTool> {
        let tools = self.tools();
        let entries: Vec<Entry> = tools.iter().map(ExposedTool::entry).collect();
        search::search(&entries, query, limit).into_iter().map(|i| tools[i].clone()).collect()
    }

    // ------------------------------------------------------------ calls

    /// Calls the tool named `exposed`. A server that died is restarted once, at most every
    /// few seconds.
    pub fn call(&self, exposed: &str, args: Value, cancelled: &dyn Fn() -> bool) -> Result<ToolOutput, McpError> {
        let tool = self.find(exposed).ok_or_else(|| McpError::Config(format!("{exposed} is not an available MCP tool (a server may have disconnected or changed its tools)")))?;
        let slot = self.slot(&tool.server).ok_or_else(|| McpError::Config(format!("no MCP server named {}", tool.server)))?;
        let client = self.live_client(&slot)?;
        let result = client.call_tool(&tool.tool.name, args, cancelled)?;
        let capped = output::cap(&result.text(), self.opts.max_output_tokens, exposed, self.opts.spill_dir.as_deref());
        Ok(ToolOutput { text: capped.text.clone(), is_error: result.is_error, capped })
    }

    fn live_client(&self, slot: &Arc<Slot>) -> Result<Arc<Client>, McpError> {
        if let Some(c) = self.ready(slot).filter(|c| c.is_alive()) {
            return Ok(c);
        }
        let recent = lock(&slot.last_attempt).is_some_and(|t| t.elapsed() < RECONNECT_GAP);
        if !recent {
            // Restart on the caller's thread: the call is waiting for it anyway.
            *lock(&slot.state) = State::Connecting;
            self.connect(slot);
            if let Some(c) = self.ready(slot) {
                return Ok(c);
            }
        }
        let why = match &*lock(&slot.state) {
            State::Failed(e) | State::NeedsAuth(e) => e.clone(),
            State::NeedsTrust => "needs trust".into(),
            _ => "not connected".into(),
        };
        Err(McpError::Closed { server: slot.cfg.name.clone(), detail: why })
    }

    // ------------------------------------------------------------ resources and prompts

    fn clients(&self) -> Vec<(String, Arc<Client>)> {
        self.all().iter().filter_map(|s| self.ready(s).map(|c| (s.cfg.name.clone(), c))).collect()
    }

    pub fn resources(&self) -> Vec<(String, Resource)> {
        self.clients().into_iter().flat_map(|(n, c)| c.resources().unwrap_or_default().into_iter().map(move |r| (n.clone(), r))).collect()
    }

    pub fn resource_templates(&self) -> Vec<(String, ResourceTemplate)> {
        self.clients().into_iter().flat_map(|(n, c)| c.resource_templates().unwrap_or_default().into_iter().map(move |r| (n.clone(), r))).collect()
    }

    pub fn has_resources(&self) -> bool {
        self.clients().iter().any(|(_, c)| c.capabilities().resources)
    }

    pub fn has_prompts(&self) -> bool {
        self.clients().iter().any(|(_, c)| c.capabilities().prompts)
    }

    pub fn read_resource(&self, server: &str, uri: &str, cancelled: &dyn Fn() -> bool) -> Result<ToolOutput, McpError> {
        let slot = self.slot(server).ok_or_else(|| McpError::Config(format!("no MCP server named {server}")))?;
        let client = self.live_client(&slot)?;
        let parts: Vec<ResourceContent> = client.read_resource(uri, cancelled)?;
        let text = parts
            .iter()
            .map(|p| match &p.text {
                Some(t) => t.clone(),
                None => format!("[{}: {} bytes of binary]", p.mime.as_deref().unwrap_or("binary"), p.blob_bytes.unwrap_or(0)),
            })
            .collect::<Vec<_>>()
            .join("\n");
        let capped = output::cap(&text, self.opts.max_output_tokens, &format!("{server}-resource"), self.opts.spill_dir.as_deref());
        Ok(ToolOutput { text: capped.text.clone(), is_error: false, capped })
    }

    pub fn prompts(&self) -> Vec<(String, Prompt)> {
        self.clients().into_iter().flat_map(|(n, c)| c.prompts().unwrap_or_default().into_iter().map(move |p| (n.clone(), p))).collect()
    }

    pub fn get_prompt(&self, server: &str, name: &str, args: &Map<String, Value>, cancelled: &dyn Fn() -> bool) -> Result<Vec<PromptMessage>, McpError> {
        let slot = self.slot(server).ok_or_else(|| McpError::Config(format!("no MCP server named {server}")))?;
        self.live_client(&slot)?.get_prompt(name, args, cancelled)
    }

    /// The sum of the available tools' schema text, in estimated tokens (name, description and
    /// input schema as a provider would send them).
    pub fn schema_tokens(&self) -> usize {
        self.tools().iter().map(|t| output::estimate_tokens(&format!("{}{}{}", t.exposed, t.tool.description, t.tool.input_schema))).sum()
    }

    /// Counts per server of tools the model can see.
    pub fn counts(&self) -> HashMap<String, usize> {
        let mut m = HashMap::new();
        for t in self.tools() {
            *m.entry(t.server).or_insert(0) += 1;
        }
        m
    }
}

/// An `OAuth` handle for an HTTP server entry (`marlowe mcp auth`).
pub fn oauth_for(cfg: &ServerConfig, connect: &ConnectOpts, env: &dyn Fn(&str) -> Option<String>) -> Result<OAuth, String> {
    let cfg = cfg.expanded(env)?;
    match &cfg.transport {
        Transport::Http { url, oauth, .. } => {
            let u = Url::parse(url)?;
            Ok(OAuth::new(&cfg.name, &u, oauth.clone().unwrap_or_default(), connect.http.clone(), connect.tokens.clone()))
        }
        _ => Err(format!("{} is not an http server; only http servers use OAuth", cfg.name)),
    }
}
