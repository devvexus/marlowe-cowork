//! MCP servers (track K): `McpList`, `McpAdd`, `McpRemove`, and the MCP host composed beside each
//! session's built-in tools.
//!
//! One [`marlowe_mcp::hub::Hub`] serves each workspace, shared by that workspace's sessions and
//! gone (its servers killed) with the last of them. A session's runner composes it in
//! [`compose`]: it waits a few seconds for servers to connect, and a slower server joins the tool
//! list when it is ready. `McpAdd` and `McpRemove` edit the scope's file (`<home>\mcp.json`,
//! `<workspace>\.mcp.json`, `<workspace>\.marlowe\mcp.local.json`) and reload a live hub, so an
//! `McpAdd` of an existing name is also how a server is reconnected after `marlowe mcp auth`.
//! Adding to the project or local scope is the user's act, so it also trusts that exact entry;
//! entries that arrive in a repository's files wait for `marlowe mcp trust`.
//!
//! A `Session`-scope server (`marlowe -p --mcp-config`) lasts as long as the connection that added
//! it: it is dropped by `McpRemove`, and by a reaper when that client goes away without one. The
//! hub is still the workspace's, so until `McpAdd` names a session (a proto field the integrator
//! adds) the server is reachable by that workspace's other sessions while it lives; other clients'
//! `McpList` does not show it.

mod host;
#[cfg(test)]
mod host_tests;
mod ide_context;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use marlowe_agent::{Conversation, ToolHost};
use marlowe_mcp::client::ConnectOpts;
use marlowe_mcp::config::{self, Paths, Scope, ServerConfig, Trust};
use marlowe_mcp::hub::{Hub, HubOpts};
use marlowe_mcp::oauth::{KeyringTokens, TokenStore};
use marlowe_proto::{Event, McpServerInfo, Request, SettingsScope, Slot};

use host::McpTools;

use crate::daemon::Core;

/// How long a session's first turn waits for its servers to connect.
const CONNECT_GRACE: Duration = Duration::from_secs(3);
/// How often a reaper looks for its client.
const REAP_EVERY: Duration = Duration::from_millis(250);
/// How long `McpAdd` and `McpList` wait for a reloaded hub before answering.
const REPLY_GRACE: Duration = Duration::from_secs(3);
/// Saved over-long outputs older than this are removed at start-up.
const SPILL_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 3600);

/// A server added for one run, and the client connection that owns it.
#[derive(Clone)]
struct Owned {
    client: u64,
    cfg: ServerConfig,
}

struct Registry {
    hubs: Mutex<HashMap<String, Weak<Hub>>>,
    /// Servers added for a run (`--mcp-config`), by workspace key.
    session: Mutex<HashMap<String, Vec<Owned>>>,
    /// `(client, workspace key)` pairs a reaper is already watching.
    reapers: Mutex<HashSet<(u64, String)>>,
    tokens: Arc<dyn TokenStore>,
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            hubs: Mutex::new(HashMap::new()),
            session: Mutex::new(HashMap::new()),
            reapers: Mutex::new(HashSet::new()),
            tokens: Arc::new(KeyringTokens::new()),
        }
    }
}

impl Registry {
    fn session_servers(&self, key: &str) -> Vec<ServerConfig> {
        lock(&self.session).get(key).map(|l| l.iter().map(|o| o.cfg.clone()).collect()).unwrap_or_default()
    }
}

/// Drops the servers `client` added for `key`, once its connection is gone. One reaper thread per
/// client and workspace; it ends with the connection or the daemon.
fn reap_when_gone(core: &Arc<Core>, client: u64, ws: Option<PathBuf>) {
    let key = ws_key(ws.as_deref());
    if !lock(&registry(core).reapers).insert((client, key.clone())) {
        return;
    }
    let core = core.clone();
    let spawned = std::thread::Builder::new().name("marlowe-mcp-reap".into()).spawn(move || {
        while core.bus.is_attached(client) && !core.is_shutting_down() {
            std::thread::sleep(REAP_EVERY);
        }
        let reg = registry(&core);
        {
            let mut all = lock(&reg.session);
            if let Some(list) = all.get_mut(&key) {
                list.retain(|o| o.client != client);
            }
        }
        lock(&reg.reapers).remove(&(client, key.clone()));
        if let Some(h) = live_hub(&core, ws.as_deref()) {
            h.set_session(reg.session_servers(&key));
            h.reload();
        }
    });
    if spawned.is_err() {
        eprintln!("marlowe: could not start a thread to clean up an MCP client's servers");
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn registry(core: &Arc<Core>) -> Arc<Registry> {
    core.ext.get_or_init(Registry::default)
}

fn ws_key(ws: Option<&Path>) -> String {
    ws.map_or_else(|| "-".to_string(), |w| w.display().to_string().to_ascii_lowercase())
}

fn spill_dir(core: &Core) -> PathBuf {
    core.home.join("cache").join("mcp-output")
}

/// The workspace's live hub, or a new one.
fn hub_for(core: &Arc<Core>, ws: Option<&Path>) -> Arc<Hub> {
    let reg = registry(core);
    let key = ws_key(ws);
    let mut hubs = lock(&reg.hubs);
    if let Some(h) = hubs.get(&key).and_then(Weak::upgrade) {
        return h;
    }
    let mut connect = ConnectOpts::new(reg.tokens.clone());
    connect.roots = ws.map(|w| vec![w.to_path_buf()]).unwrap_or_default();
    let mut opts = HubOpts::new(Paths::new(&core.home, ws), connect);
    opts.session = reg.session_servers(&key);
    opts.strict = std::env::var(marlowe_mcp::STRICT_ENV).is_ok_and(|v| v.trim() == "1");
    opts.spill_dir = Some(spill_dir(core));
    if let Some(n) = std::env::var("MAX_MCP_OUTPUT_TOKENS").ok().and_then(|v| v.parse().ok()).filter(|n| *n > 0) {
        opts.max_output_tokens = n;
    }
    let hub = Hub::start(opts);
    hubs.retain(|_, w| w.strong_count() > 0);
    hubs.insert(key, Arc::downgrade(&hub));
    hub
}

fn live_hub(core: &Arc<Core>, ws: Option<&Path>) -> Option<Arc<Hub>> {
    lock(&registry(core).hubs).get(&ws_key(ws)).and_then(Weak::upgrade)
}

fn conductor_window(core: &Core) -> Option<u32> {
    let (model, _) = core.brains.resolve(&Slot::Conductor).ok()?;
    core.brains.model_info(&model).ok()?.context_window
}

/// Composes the MCP host beside a session's built-in tools (`stack::tools`).
pub(crate) fn compose(core: &Arc<Core>, convo: &Conversation, host: Arc<dyn ToolHost>) -> Arc<dyn ToolHost> {
    let hub = hub_for(core, convo.workspace.as_deref());
    hub.wait_settled(CONNECT_GRACE);
    let blobs = Arc::new(marlowe_model::image::FsBlobStore::new(core.home.join("blobs")));
    Arc::new(McpTools::new(host, hub, conductor_window(core)).with_blobs(blobs))
}

// ---------------------------------------------------------------- requests

fn to_scope(s: SettingsScope) -> Scope {
    match s {
        SettingsScope::User => Scope::User,
        SettingsScope::Project => Scope::Project,
        SettingsScope::Local => Scope::Local,
        SettingsScope::Session => Scope::Session,
    }
}

fn from_scope(s: Scope) -> SettingsScope {
    match s {
        Scope::User => SettingsScope::User,
        Scope::Project => SettingsScope::Project,
        Scope::Local => SettingsScope::Local,
        Scope::Session => SettingsScope::Session,
    }
}

/// The servers the workspace's configuration names, with a status taken from a live hub when
/// there is one and otherwise from what is on disk (nothing is started to answer a list).
///
/// `viewer` is the client asking: a run's own servers (`Session` scope) belong to the connection
/// that added them and are not listed to another one while it is connected.
fn server_list(core: &Arc<Core>, ws: Option<&Path>, viewer: u64) -> Vec<McpServerInfo> {
    let reg = registry(core);
    let hidden: HashSet<String> = lock(&reg.session)
        .get(&ws_key(ws))
        .map(|l| l.iter().filter(|o| o.client != viewer && core.bus.is_attached(o.client)).map(|o| o.cfg.name.clone()).collect())
        .unwrap_or_default();
    let is_hidden = |name: &str, scope: Scope| scope == Scope::Session && hidden.contains(name);
    if let Some(hub) = live_hub(core, ws) {
        return hub
            .statuses()
            .into_iter()
            .filter(|s| !is_hidden(&s.name, s.scope))
            .map(|s| {
                let detail = ide_detail(core, ws, &s.name, s.scope);
                McpServerInfo { name: s.name, scope: from_scope(s.scope), transport: s.transport.to_string(), tools: s.tools, status: s.status.label(), detail }
            })
            .collect();
    }
    let session = reg.session_servers(&ws_key(ws));
    let paths = Paths::new(&core.home, ws);
    let trust = Trust::new(&paths);
    // `--strict-mcp-config` means the run's own servers and nothing from any configuration file.
    let strict = std::env::var(marlowe_mcp::STRICT_ENV).is_ok_and(|v| v.trim() == "1");
    let servers = if strict { session } else { config::load_all(&paths, &session).servers };
    servers
        .into_iter()
        .filter(|s| !is_hidden(&s.name, s.scope))
        .map(|s| {
            let status = if s.disabled {
                "disabled"
            } else if !trust.is_trusted(ws, &s) {
                "needs trust"
            } else {
                "idle (connects with a session)"
            };
            let detail = ide_detail(core, ws, &s.name, s.scope);
            McpServerInfo { name: s.name.clone(), scope: from_scope(s.scope), transport: s.transport.name().to_string(), tools: 0, status: status.to_string(), detail }
        })
        .collect()
}

/// What `/ide` shows for the discovered editor server (session scope, named `ide`): the window
/// and the folders it has open. Other servers have nothing to add.
fn ide_detail(core: &Arc<Core>, ws: Option<&Path>, name: &str, scope: Scope) -> String {
    if name == marlowe_mcp::ide::SERVER_NAME && scope == Scope::Session {
        marlowe_mcp::ide::status(&core.home, ws)
    } else {
        String::new()
    }
}

fn workspace_path(ws: &Option<String>) -> Option<PathBuf> {
    ws.as_deref().filter(|w| !w.trim().is_empty()).map(PathBuf::from)
}

pub(crate) fn dispatch(core: &Arc<Core>, client: u64, req: Request) {
    let core = core.clone();
    let spawned = std::thread::Builder::new().name("marlowe-mcp-request".into()).spawn(move || {
        let reply = |e: Event| core.bus.reply(client, e);
        let fail = |m: String| core.bus.reply(client, Event::Error { session: None, message: m });
        match req {
            Request::McpList { workspace } => {
                let ws = workspace_path(&workspace);
                if let Some(h) = live_hub(&core, ws.as_deref()) {
                    h.wait_settled(REPLY_GRACE);
                }
                reply(Event::McpServers { list: server_list(&core, ws.as_deref(), client) });
            }
            Request::McpAdd { name, scope, config: entry, workspace } => {
                let ws = workspace_path(&workspace);
                let paths = Paths::new(&core.home, ws.as_deref());
                let scope = to_scope(scope);
                let result = if scope == Scope::Session {
                    config::parse_entry(&name, scope, &entry).map(|cfg| {
                        let reg = registry(&core);
                        let mut all = lock(&reg.session);
                        let list = all.entry(ws_key(ws.as_deref())).or_default();
                        list.retain(|o| o.cfg.name != name);
                        list.push(Owned { client, cfg });
                        if let Some(h) = live_hub(&core, ws.as_deref()) {
                            h.set_session(list.iter().map(|o| o.cfg.clone()).collect());
                        }
                        drop(all);
                        reap_when_gone(&core, client, ws.clone());
                    })
                } else {
                    // Adding it is the user's act, so that exact entry is trusted from now on.
                    config::add(&paths, scope, &name, &entry).and_then(|cfg| Trust::new(&paths).approve(ws.as_deref(), &cfg))
                };
                match result {
                    Ok(()) => {
                        if let Some(h) = live_hub(&core, ws.as_deref()) {
                            h.reload();
                            h.wait_settled(REPLY_GRACE);
                        }
                        reply(Event::McpServers { list: server_list(&core, ws.as_deref(), client) });
                    }
                    Err(e) => fail(format!("mcp_add: {e}")),
                }
            }
            Request::McpRemove { name, scope, workspace } => {
                let ws = workspace_path(&workspace);
                let paths = Paths::new(&core.home, ws.as_deref());
                let scope = to_scope(scope);
                let removed = if scope == Scope::Session {
                    let reg = registry(&core);
                    let mut all = lock(&reg.session);
                    let list = all.entry(ws_key(ws.as_deref())).or_default();
                    let before = list.len();
                    list.retain(|o| o.cfg.name != name);
                    if let Some(h) = live_hub(&core, ws.as_deref()) {
                        h.set_session(list.iter().map(|o| o.cfg.clone()).collect());
                    }
                    Ok(list.len() != before)
                } else {
                    config::remove(&paths, scope, &name)
                };
                match removed {
                    Ok(true) => {
                        if let Some(h) = live_hub(&core, ws.as_deref()) {
                            h.reload();
                        }
                        reply(Event::McpServers { list: server_list(&core, ws.as_deref(), client) });
                    }
                    Ok(false) => fail(format!("mcp_remove: no server named {name} in the {} scope", scope.as_str())),
                    Err(e) => fail(format!("mcp_remove: {e}")),
                }
            }
            _ => {}
        }
    });
    if spawned.is_err() {
        // No thread to run it on: say so rather than leave the client waiting.
        eprintln!("marlowe: could not start a thread for an MCP request");
    }
}

/// Removes saved MCP output older than a week, on its own thread.
pub(crate) fn on_boot(core: &Arc<Core>) {
    let dir = spill_dir(core);
    let _ = std::thread::Builder::new().name("marlowe-mcp-clean".into()).spawn(move || {
        let Ok(rd) = std::fs::read_dir(&dir) else { return };
        for e in rd.flatten() {
            let old = e.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|age| age > SPILL_MAX_AGE);
            if old {
                let _ = std::fs::remove_file(e.path());
            }
        }
    });
}
