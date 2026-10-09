//! Memory errors. Each one exists because the alternative was a silent default.

use marlowe_journal::Seq;

#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    #[error(
        "profile was derived by version {found}, this build derives version {expected}. The \
         belief store is a materialized view, so the fix is a rebuild, never a migration \
         (ADR-009)"
    )]
    DerivationVersionMismatch { found: u32, expected: u32 },

    #[error(
        "event at seq {seq} does not decode: {source}. Section 1's retention rule is \
         absolute -- journal payloads must stay decodable forever, because a kind that stops \
         decoding makes every event after it unreplayable"
    )]
    UndecodablePayload { seq: Seq, source: serde_json::Error },

    #[error(
        "event at seq {seq} refers to memory {id}, which no MemoryWritten event created. \
         The fold cannot resolve this entry's state, and assuming a default would resurrect \
         a memory whose real state is unknown (invariant 9)"
    )]
    EventForUnknownMemory { seq: Seq, id: String },

    #[error(transparent)]
    Journal(#[from] marlowe_journal::JournalError),

    #[error(transparent)]
    Json(#[from] serde_json::Error),
}
