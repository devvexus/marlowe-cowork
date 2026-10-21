//! Provider errors.

use serde::{Deserialize, Serialize};

/// The class of a provider failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderErrorKind {
    Auth,
    RateLimit,
    Network,
    BadRequest,
    Server,
    Cancelled,
    Unsupported,
}

impl ProviderErrorKind {
    /// Whether a retry can reasonably succeed by default.
    pub fn default_retriable(self) -> bool {
        matches!(self, Self::RateLimit | Self::Network | Self::Server)
    }
}

/// The start of a [`ProviderError::malformed_call`] detail.
pub const MALFORMED_CALL: &str = "malformed tool call";

/// A provider failure. `detail` must never contain a secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{kind:?}: {detail}")]
pub struct ProviderError {
    pub kind: ProviderErrorKind,
    pub detail: String,
    pub retriable: bool,
}

impl ProviderError {
    /// An error with the kind's default retriability.
    pub fn new(kind: ProviderErrorKind, detail: impl Into<String>) -> Self {
        Self { kind, detail: detail.into(), retriable: kind.default_retriable() }
    }

    pub fn auth(detail: impl Into<String>) -> Self {
        Self::new(ProviderErrorKind::Auth, detail)
    }

    pub fn network(detail: impl Into<String>) -> Self {
        Self::new(ProviderErrorKind::Network, detail)
    }

    pub fn bad_request(detail: impl Into<String>) -> Self {
        Self::new(ProviderErrorKind::BadRequest, detail)
    }

    pub fn server(detail: impl Into<String>) -> Self {
        Self::new(ProviderErrorKind::Server, detail)
    }

    /// The engine could not parse the model's tool call (broken markup or JSON). Not retried on
    /// the wire: the same request would give the same result; the loop tells the model once.
    pub fn malformed_call(detail: impl std::fmt::Display) -> Self {
        Self { kind: ProviderErrorKind::Server, detail: format!("{MALFORMED_CALL}: {detail}"), retriable: false }
    }

    pub fn is_malformed_call(&self) -> bool {
        self.detail.starts_with(MALFORMED_CALL)
    }

    pub fn cancelled() -> Self {
        Self::new(ProviderErrorKind::Cancelled, "cancelled")
    }

    pub fn unsupported(detail: impl Into<String>) -> Self {
        Self::new(ProviderErrorKind::Unsupported, detail)
    }

    /// Classifies an HTTP status: 401/403 auth, 429 rate limit, 4xx bad request, 5xx server.
    pub fn from_status(status: u16, detail: impl Into<String>) -> Self {
        let kind = match status {
            401 | 403 => ProviderErrorKind::Auth,
            408 => ProviderErrorKind::Network,
            429 => ProviderErrorKind::RateLimit,
            400..=499 => ProviderErrorKind::BadRequest,
            _ => ProviderErrorKind::Server,
        };
        Self::new(kind, detail)
    }

    pub fn is_cancelled(&self) -> bool {
        self.kind == ProviderErrorKind::Cancelled
    }
}
