//! The client <-> daemon wire.
//!
//! JSON lines over `127.0.0.1` TCP. A client's first line is [`Request::Hello`] with the token
//! from `daemon.json`; the daemon answers with [`Event::Welcome`] (or [`Event::Error`] and a
//! close). Every event is wrapped in an [`Envelope`] with a daemon-wide sequence number, and
//! every client sees the same stream; `Hello::since_seq` replays from the ring buffer.

#![forbid(unsafe_code)]

pub mod auth;
mod event;
mod frame;
mod ids;
pub mod paths;
mod request;

pub use event::{
    Envelope, Event, LibraryModel, ProviderStatus, SessionSummary, SlotAssignment, StatusReport,
    StoredTurn, TurnStop,
};
pub use frame::{write_line, FrameError, LineReader, MAX_LINE_BYTES};
pub use ids::{ApprovalId, SessionId};
pub use paths::DaemonInfo;
pub use request::{ApprovalDecision, Request, Secret, Slot, SlotParseError};

/// Wire protocol version. Bumped on any incompatible change.
pub const PROTO_VERSION: u32 = 1;

#[cfg(test)]
mod tests;
