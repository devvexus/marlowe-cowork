//! Errors.

use marlowe_model::{ModelRef, ProviderError, ProviderKind};

#[derive(Debug, thiserror::Error)]
pub enum BrainsError {
    #[error("database: {0}")]
    Db(#[from] rusqlite::Error),
    /// Never carries the secret itself.
    #[error("credential store: {0}")]
    Credential(String),
    #[error("you have no models; add one first")]
    NoModels,
    #[error("{0} is not in the library")]
    UnknownModel(ModelRef),
    #[error("{} is coming soon", .0.display_name())]
    Unsupported(ProviderKind),
    #[error("no adapter registered for {0}")]
    NotRegistered(ProviderKind),
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error("{0}")]
    Invalid(String),
}
