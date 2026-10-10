//! The LSP client, name-based code intelligence and post-edit diagnostics (design 3.3 L2, 2.1
//! T24-T26).
//!
//! | Module | What |
//! |---|---|
//! | [`transport`] | starting a server: [`transport::StdioLauncher`] (a child through `marlowe_os::child`, reader/writer/stderr threads) or the in-memory [`fake`] |
//! | [`client`] | one server's JSON-RPC session: requests with timeouts and cancel, document sync, pushed and pulled diagnostics with a settle window, readiness |
//! | [`manager`] | lazy start per (server, root), at most 2 live, idle stop after 10 min |
//! | [`servers`] | rust-analyzer, pyright, typescript-language-server and where their programs are |
//! | [`intel`] | [`intel::CodeIntel`]: symbols, definition, references, hover, diagnostics and rename by *name*, over the tags index and the servers |
//! | [`uri`] | `file:` URIs and UTF-16/UTF-8 positions |
//!
//! Std threads and crossbeam only; no async runtime. Servers run only on the local workspace
//! (a container workspace gets the L0/L1 fallbacks).

pub mod client;
pub mod fake;
pub mod intel;
pub mod manager;
pub mod servers;
pub mod transport;
pub mod uri;

#[cfg(test)]
mod a8;
#[cfg(test)]
mod tests;

pub use client::{Diag, LspClient};
pub use intel::CodeIntel;
pub use manager::LspManager;
pub use servers::ServerSpec;

/// Why a language-server operation failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LspError {
    #[error("no language server is configured for {0}")]
    NoServer(String),
    #[error("{0} is not installed")]
    NotInstalled(String),
    #[error("could not start the language server: {0}")]
    Start(String),
    #[error("the language server {0} stopped")]
    Gone(String),
    #[error("timed out: {0}")]
    Timeout(String),
    #[error("interrupted")]
    Cancelled,
    #[error("{method} failed ({code}): {message}")]
    Server { method: String, code: i32, message: String },
}
