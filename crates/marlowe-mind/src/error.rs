//! Memory service errors.

use std::path::PathBuf;

/// A memory service failure.
#[derive(Debug, thiserror::Error)]
pub enum MindError {
    #[error("journal: {0}")]
    Journal(#[from] marlowe_journal::JournalError),
    #[error("memory: {0}")]
    Memory(#[from] marlowe_memory::MemoryError),
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization: {0}")]
    Json(#[from] serde_json::Error),
    /// No memory has this id.
    #[error("no memory with id {0}")]
    NotFound(String),
    /// The write was refused (empty text, would lower authority, unknown parent).
    #[error("refused: {0}")]
    Rejected(String),
    /// No old profile at that path.
    #[error("no old harness profile at {0}")]
    NoLegacyProfile(PathBuf),
    /// The old journal failed chain verification under its own key.
    #[error("the old journal failed verification: {0}")]
    LegacyChain(String),
    /// A frozen artifact (gate, operating point, consolidation policy) failed to load.
    #[error("artifact: {0}")]
    Artifact(String),
    /// The operation needs a model and the service is degraded.
    #[error("degraded: {0}")]
    Degraded(String),
    /// The actor thread has exited.
    #[error("the memory service has stopped")]
    Closed,
}
