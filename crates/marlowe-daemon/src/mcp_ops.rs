//! MCP servers (track K): McpList, McpAdd, McpRemove.
//!
//! Seam stub: `client.rs` already routes these requests here; until track K lands every one is
//! answered with an `Error` naming the track. `.mcp.json` and settings layers; the servers themselves run in `marlowe-mcp`. Replace the body of
//! [`dispatch`]; keep its signature.

use std::sync::Arc;

use marlowe_agent::{Conversation, ToolHost};
use marlowe_proto::Request;

use crate::daemon::Core;

pub(crate) fn dispatch(core: &Arc<Core>, client: u64, req: Request) {
    crate::client::not_yet(core, client, &req, "K");
}

/// Composes the MCP host beside a session's built-in tools (`stack::tools`). Returns `host`
/// unchanged until K lands.
pub(crate) fn compose(_core: &Arc<Core>, _convo: &Conversation, host: Arc<dyn ToolHost>) -> Arc<dyn ToolHost> {
    host
}

/// Connects configured MCP servers at daemon start (`boot.rs`), on its own thread.
pub(crate) fn on_boot(_core: &Arc<Core>) {}
