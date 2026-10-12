//! The Anthropic Messages API provider (design 3.10, section 4). Seam skeleton, owned by track G.
//!
//! What goes here: a `marlowe_model::Provider` for `ProviderKind::Anthropic` over blocking
//! rustls HTTP (no tokio): image blocks from `Part::Image` / `ToolResult::parts`, thinking
//! signatures round-tripped across tool steps and steers, cache breakpoints (tools, end of system,
//! last stable message, latest), 529 backoff, `count_tokens` for the meter, the 1M-context option.
//! The key comes from Credential Manager with an `ANTHROPIC_API_KEY` fallback read once; the
//! binary registers the provider in `ProviderFactory` (`crates/marlowe/src/wire.rs`).
//!
//! | Module | What goes here |
//! |---|---|
//! | [`request`] | `ChatRequest` -> Messages API body |
//! | [`stream`] | SSE -> `StreamEvent` |
//! | [`transport`] | HTTP, retry, 529 backoff |

#![forbid(unsafe_code)]

pub mod request;
pub mod stream;
pub mod transport;

/// The provider this crate implements.
pub const KIND: marlowe_model::ProviderKind = marlowe_model::ProviderKind::Anthropic;
