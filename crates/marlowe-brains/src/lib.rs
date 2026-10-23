//! The model library and provider registry.
//!
//! The library lives in `cowork.db` (`models`, `slots`). The first model added is the Conductor's
//! until one is assigned; every other unset slot runs the Conductor's model. Nothing loads unless
//! [`Brains::admit`] says it fits beside what is resident ([`load`]). Providers are constructed
//! by a [`ProviderFactory`] whose constructors the binary registers, so this crate never depends
//! on a provider crate. API keys live in the Windows Credential Manager and are scrubbed from
//! child-process environments.

#![forbid(unsafe_code)]

mod brains;
pub mod context;
pub mod engine;
pub mod credentials;
mod detect;
mod error;
mod factory;
mod library;
pub mod load;

pub use brains::Brains;
pub use credentials::{
    api_key, env_var, scrubbed_env, CredentialStore, KeyringStore, MemoryStore, SCRUBBED_ENV,
};
pub use detect::{detect_ollama, detect_providers, OLLAMA_ADDR};
pub use error::BrainsError;
pub use context::{ContextGate, SlotLease, DEFAULT_WINDOW};
pub use factory::{Hardware, ProviderConfig, ProviderCtor, ProviderFactory};
pub use library::{Library, SCHEMA};
pub use load::{Admission, LoadCost, Priority};
