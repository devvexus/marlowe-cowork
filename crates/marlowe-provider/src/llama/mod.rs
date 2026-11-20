//! Marlowe's local engine: the selected backend's `llama-server`, launched by Marlowe with
//! several slots. Every local model runs here; Ollama is only a source of model files.
//!
//! What a model runs with is read from Ollama's store on disk ([`eligible`]): the GGUF, its draft
//! and projector, the Modelfile's parameters, system prompt and messages, or why llama.cpp can't
//! run it. Marlowe starts the server with N slots ([`launch`], [`server`], [`settings`]), keeps the
//! conductor's conversation on a slot of its own and lets crew agents run at once on the rest
//! ([`engine`]). The request is the one Ollama would have sent ([`chat`]), so tool calling is
//! unchanged. A model that does not fit whole on the GPU runs split, as Ollama runs it: the
//! planner decides before, the engine checks the free memory before it starts a server and the
//! driver's spill after ([`engine::VramProbe`]), because `--fit off` does not fail under the
//! Windows sysmem fallback.

pub mod chat;
pub mod eligible;
pub mod engine;
pub mod hw;
pub mod launch;
pub mod persist;
pub mod render;
pub mod server;
pub mod settings;
pub mod template;
pub mod warm;

#[cfg(test)]
mod provider_tests;
#[cfg(test)]
pub(crate) mod testkit;

pub use eligible::{EngineModel, ModelDefaults};
pub use engine::{conversation_key, same_model, EngineConfig, LlamaEngine, ReadAheads, Status, VramProbe, Want};
pub use hw::{EngineControl, LlamaModels};
pub use server::{Spawner, SystemSpawner};
pub use settings::{keep_alive_from, saved_user_env, GpuFit, ServerSettings};
pub use template::Thinking;
