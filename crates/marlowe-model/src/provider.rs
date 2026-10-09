//! The provider trait.

use crate::cancel::CancelToken;
use crate::error::ProviderError;
use crate::info::{ModelInfo, ProviderKind};
use crate::request::ChatRequest;
use crate::stream::{Completion, StreamEvent};

/// A model backend. Implementations translate wire formats only and never interpret tool names.
pub trait Provider: Send + Sync {
    /// Which provider this is.
    fn kind(&self) -> ProviderKind;

    /// The models this provider can serve right now.
    fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError>;

    /// Runs one model call, calling `on` for every increment, and returns the folded result.
    ///
    /// Must return `ProviderErrorKind::Cancelled` promptly once `cancel` fires.
    fn stream(
        &self,
        req: &ChatRequest,
        cancel: &CancelToken,
        on: &mut dyn FnMut(StreamEvent),
    ) -> Result<Completion, ProviderError>;
}
