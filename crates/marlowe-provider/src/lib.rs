//! Local model providers on the `marlowe_model::Provider` seam.
//!
//! [`OllamaProvider`] speaks Ollama's `/api/chat`. Everything here is loopback HTTP with no TLS;
//! hosted providers live in their own crates. Providers translate wire formats only and never
//! interpret tool names.

#![forbid(unsafe_code)]

pub mod http;
pub mod ollama;
pub mod ollama_runtime;
pub mod ollama_store;
pub mod sse;
pub mod think;
pub mod wire;

pub use http::{HttpError, LocalEndpoint};
pub use ollama::{OllamaProvider, DEFAULT_CONTEXT_TOKENS};
pub use ollama_runtime::{OllamaHw, OllamaServerControl};
pub use ollama_store::{LaunchPlan, ResolveError, ResolvedModel, Sampling};
pub use sse::{Frame, SseStream};
pub use think::{Segment, Split, ThinkSplitter};
