//! The Ollama provider adapter. **ADR-028.**
//!
//! Marlowe's default model is local: `127.0.0.1:11434`, no account, no key, no network. That is
//! what makes K6's *"install → first useful output, zero config"* reachable — every way of
//! sourcing a hosted key puts something in front of the first run, and K6 measures whether that
//! something is there, not whether it is small.
//!
//! # This is an adapter, not a broker
//!
//! It normalizes what a model speaks (§12). It does **not** store, refresh or transport a
//! credential — Ollama needs none, and building a credential broker with no consumer is how a
//! credential path gets written once and reviewed never. That is M5's work (§2.13, ADR-005).
//!
//! # Three things ADR-028 requires, and where each one lives
//!
//! | Requirement | Here |
//! |---|---|
//! | Degrade honestly when the endpoint is absent | [`Availability`], surfaced as [`marlowe_loop::DegradedPath::ModelUnavailable`] — a declared value on the run, never a crash, and the message names the remedy |
//! | Record the capability difference | [`ModelCapability`], including a **measured** tool-call success rate rather than a reputation |
//! | ADR-008's tiered routing survives | [`Routing`] maps a **task role** to a model. `CapabilityProfile::model_route` already names a role and never a model, so hosted models slot in without reshaping it |
//!
//! # Cloud tags are refused
//!
//! Ollama serves some models under `*-cloud` / `:cloud` tags that proxy to Ollama's hosted
//! service. Those reach the network, need an account, and would silently undo the one property
//! this whole decision exists to establish. [`Routing`] refuses them **by name** rather than
//! letting one be selected by a config line nobody re-reads.

#![forbid(unsafe_code)]

pub mod capability;
pub mod deadline;
pub mod http;
pub mod hybrid;
pub mod ollama;
pub mod llamacpp;
pub mod ollama_store;
pub mod routing;
pub mod sse;
pub mod think;
pub mod wire;

pub use capability::{ModelCapability, ToolCallReport};
pub use marlowe_permission::ArgValue;
pub use http::{HttpError, LocalEndpoint};
pub use ollama::{DEFAULT_CONTEXT_TOKENS, MODEL_CONTEXT_CEILING, capability_for, default_capability, Availability, OllamaDriver, DEFAULT_MODEL};
pub use llamacpp::{LlamaCppDriver, Offload, OffloadPolicy, LLAMACPP_DEFAULT_PORT};
pub use hybrid::{EngineFailure, SupervisedServer, FELL_BACK_MARKER, HYBRID_PROVIDER_NAME};
pub use ollama_store::{LaunchPlan, ResolveError, ResolvedModel, Sampling};
pub use routing::{is_cloud_tag, Routing, RoutingError};
pub use sse::{Frame, SseStream};
pub use think::{Segment, Split, ThinkSplitter};
