//! The model seam: provider-neutral request, response and streaming types, and [`Provider`].
//!
//! Providers translate wire formats only. They never interpret tool names; the loop does.

#![forbid(unsafe_code)]

mod alloc;
mod cancel;
mod error;
mod fold;
mod info;
mod message;
mod provider;
mod request;
mod stream;

pub use alloc::ContextAllocations;
pub use cancel::{CancelHook, CancelToken};
pub use error::{ProviderError, ProviderErrorKind};
pub use fold::{fold_events, CompletionBuilder};
pub use info::{ModelInfo, ModelRef, ModelRefError, Pricing, ProviderKind};
pub use message::{Message, Part, Role, Thinking, ToolCall, ToolResult};
pub use provider::Provider;
pub use request::{ChatRequest, Effort, SystemBlock, ToolSpec};
pub use stream::{Completion, StopReason, StreamEvent, Usage};

#[cfg(test)]
mod serde_tests;
