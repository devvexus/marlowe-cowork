//! Marlowe's own parallel local engine: Ollama's bundled `llama-server`, launched by Marlowe with
//! several slots, for the models Ollama itself hands to llama-server whole.
//!
//! Ollama 0.32 serves qwen35-family models one request at a time. The llama-server under it does
//! not have that limit, so for a model with no renderer, parser or template of Ollama's own
//! ([`eligible`]) Marlowe starts the same binary with N slots ([`launch`], [`server`]), keeps the
//! conductor's conversation on a slot of its own and lets crew agents run at once on the rest
//! ([`engine`]). The request is the one Ollama would have sent ([`chat`]), so tool calling is
//! unchanged. Every other model stays on Ollama.

pub mod chat;
pub mod eligible;
pub mod engine;
pub mod hw;
pub mod launch;
pub mod persist;
pub mod render;
pub mod server;

#[cfg(test)]
mod provider_tests;
#[cfg(test)]
pub(crate) mod testkit;

pub use engine::{conversation_key, keep_alive_from, same_model, EngineConfig, LlamaEngine, Status, Want};
pub use hw::LlamaModels;
pub use server::{Spawner, SystemSpawner};
