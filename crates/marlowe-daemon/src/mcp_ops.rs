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

mod host;
#[cfg(test)]
mod host_tests;

use std::collections::HashMap;
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
/// How long `McpAdd` and `McpList` wait for a reloaded hub before answering.
const REPLY_GRACE: Duration = Duration::from_secs(3);
/// Saved over-long outputs older than this are removed at start-up.
const SPILL_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 3600);

struct Registry {
    hubs: Mutex<HashMap<String, Weak<Hub>>>,
    /// Servers added for the run (`--mcp-config`), by workspace key.
    session: Mutex<HashMap<String, Vec<ServerConfig>>>,
    tokens: Arc<dyn TokenStore>,
}

impl Default for Registry {
    fn default() -> Self {
        Self { hubs: Mutex::new(HashMap::new()), session: Mutex::new(HashMap::new()), tokens: Arc::new(KeyringTokens::new()) }
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
    opts.session = lock(&reg.session).get(&key).cloned().unwrap_or_default();
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
    Arc::new(McpTools::new(host, hub, conductor_window(core)))
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
fn server_list(core: &Arc<Core>, ws: Option<&Path>) -> Vec<McpServerInfo> {
    if let Some(hub) = live_hub(core, ws) {
        return hub
            .statuses()
            .into_iter()
            .map(|s| McpServerInfo { name: s.name, scope: from_scope(s.scope), transport: s.transport.to_string(), tools: s.tools, status: s.status.label() })
            .collect();
    }
    let reg = registry(core);
    let session = lock(&reg.session).get(&ws_key(ws)).cloned().unwrap_or_default();
    let paths = Paths::new(&core.home, ws);
    let trust = Trust::new(&paths);
    config::load_all(&paths, &session)
        .servers
        .into_iter()
        .map(|s| {
            let status = if s.disabled {
                "disabled"
            } else if !trust.is_trusted(ws, &s) {
                "needs trust"
            } else {
                "idle (connects with a session)"
            };
            McpServerInfo { name: s.name.clone(), scope: from_scope(s.scope), transport: s.transport.name().to_string(), tools: 0, status: status.to_string() }
        })
        .collect()
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
                reply(Event::McpServers { list: server_list(&core, ws.as_deref()) });
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
                        list.retain(|s| s.name != name);
                        list.push(cfg);
                        if let Some(h) = live_hub(&core, ws.as_deref()) {
                            h.set_session(list.clone());
                        }
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
                        reply(Event::McpServers { list: server_list(&core, ws.as_deref()) });
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
                    list.retain(|s| s.name != name);
                    if let Some(h) = live_hub(&core, ws.as_deref()) {
                        h.set_session(list.clone());
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
                        reply(Event::McpServers { list: server_list(&core, ws.as_deref()) });
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
