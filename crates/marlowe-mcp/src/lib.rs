//! MCP client (ADR-052, amended by M4.6): stdio and streamable HTTP with OAuth, tools, resources,
//! prompts, `list_changed`, output caps and tool search.
//!
//! | Module | What |
//! |---|---|
//! | [`config`] | `.mcp.json` in four scopes, variable expansion, workspace trust by digest |
//! | [`hub`] | one workspace's servers: connect threads, status, tool list, calls, pinned descriptions |
//! | [`client`] | one connected server: handshake, cached lists, `list_changed` |
//! | [`ide`] | the editor's loopback server found through its lock file (`mcp__ide__*`, track H2) |
//! | [`rpc`] | JSON-RPC over a [`rpc::Wire`]; [`stdio`] and [`http_wire`] are the wires |
//! | [`http`], [`oauth`] | HTTP (loopback plain, remote through `marlowe-net`) and OAuth 2.1 |
//! | [`search`], [`output`], [`names`] | `tool_search` ranking, the 25k-token cap, `mcp__srv__tool` names |
//!
//! # Trust (ADR-052)
//!
//! The server is trusted, its output is not. Installing a server is the user's authorization, so
//! a tool description reaches the model as a description; what a tool returns is untrusted
//! content (the daemon's tool host marks every MCP call as tainting). Each tool loads with the
//! consequence the user declared for its server, `irreversible` by default, so every call asks.
//!
//! # Network
//!
//! This crate reaches a remote host only through `marlowe-net` (the whole TLS surface, ADR-031).
//! It never depends on a TLS or HTTP-client crate itself; the manifest test below checks that.

#![forbid(unsafe_code)]

pub mod client;
pub mod config;
pub mod http;
pub mod http_wire;
pub mod hub;
pub mod ide;
pub mod names;
pub mod oauth;
pub mod output;
pub mod rpc;
pub mod search;
pub mod stdio;
pub mod util;

#[cfg(feature = "fake-server")]
pub mod fake;

/// Set to `1` before a daemon starts to make its MCP hubs use only the run's own servers
/// (`marlowe -p --strict-mcp-config`).
pub const STRICT_ENV: &str = "MARLOWE_MCP_STRICT";

/// The newest protocol revision this client speaks (it also accepts 2025-03-26 and 2024-11-05).
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// Why an MCP operation failed. Every variant names the server.
#[derive(Debug, Clone, thiserror::Error)]
pub enum McpError {
    #[error("mcp server `{server}`: the command `{command}` could not be started: {detail}")]
    Spawn { server: String, command: String, detail: String },
    #[error("mcp server `{server}`: {detail}")]
    Protocol { server: String, detail: String },
    #[error("mcp server `{server}` did not answer `{method}` within {secs} s")]
    Timeout { server: String, method: String, secs: u64 },
    #[error("mcp server `{server}` returned error {code}: {message}")]
    Remote { server: String, code: i64, message: String },
    #[error("mcp server `{server}`: {detail}")]
    Closed { server: String, detail: String },
    #[error("mcp server `{server}` {detail}")]
    NeedsAuth { server: String, detail: String },
    #[error("mcp server `{server}`: http {status}: {detail}")]
    Http { server: String, status: u16, detail: String },
    #[error("{0}")]
    Config(String),
    #[error("cancelled")]
    Cancelled,
}

#[cfg(test)]
mod tests {
    /// The network discipline, checked from the manifest: `marlowe-net` is the road to a remote
    /// host, and no TLS or HTTP-client crate is a direct dependency.
    #[test]
    fn the_only_network_crate_is_marlowe_net() {
        let manifest = include_str!("../Cargo.toml");
        let deps = manifest.split("[dependencies]").nth(1).and_then(|d| d.split("[features]").next()).expect("a dependencies section");
        let declared = |name: &str| deps.lines().filter(|l| !l.trim_start().starts_with('#')).any(|l| l.trim_start().starts_with(name));
        for forbidden in ["rustls", "webpki", "reqwest", "hyper", "tokio", "ureq", "native-tls", "openssl"] {
            assert!(!declared(forbidden), "`{forbidden}` is a dependency of marlowe-mcp; remote traffic goes through marlowe-net");
        }
        assert!(declared("marlowe-net"), "the manifest scan found the sanctioned road");
    }
}
