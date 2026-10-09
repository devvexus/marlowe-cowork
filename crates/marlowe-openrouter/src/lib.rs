//! The OpenRouter provider on the `marlowe_model::Provider` seam.
//!
//! HTTPS goes through `marlowe-net` (the workspace's only TLS surface) to the pinned host
//! [`OPENROUTER_HOST`]; there is no base-URL setting. The API key lives in [`ApiKey`], which cannot
//! be printed, and every error string is redacted against it before it leaves this crate.
//! Cost and token counts come from OpenRouter's own `usage` fields.

#![forbid(unsafe_code)]

pub mod catalog;
pub mod provider;
pub mod request;
pub mod retry;
pub mod secret;
pub mod stream;
pub mod transport;

/// The SSE decoder lives in `marlowe-provider` so a local SSE server can share it.
pub use marlowe_provider::sse;

pub use catalog::{parse_catalog, reasoning_field, CatalogEntry, ReasoningMeta};
pub use provider::OpenRouterProvider;
pub use secret::{ApiKey, KEY_VAR, REDACTED};
pub use transport::{
    Reply, Response, ScriptedTransport, Seen, TlsTransport, Transport, TransportError, API_ROOT,
    OPENROUTER_HOST,
};
