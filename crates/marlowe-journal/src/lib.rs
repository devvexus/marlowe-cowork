//! CONTRACTS.md section 1 — the append-only, provenance-signed event log.
//!
//! **The only source of truth.** Memory, runs, sessions, lineage and audit are materialized
//! views over this (ARCHITECTURE section 1), which is what makes "memory is the spine, not a
//! subsystem" structural rather than rhetorical.
//!
//! The single most important property, from ARCHITECTURE section 2.1:
//!
//! > **The model never appends.** The model *requests*; the harness validates, stamps
//! > origin / trust class / derivation / signature, and appends. There is no unsigned write
//! > path, which is how the 0% ASR target on unsigned memory writes (K3) is met structurally
//! > rather than by filtering.
//!
//! Three things enforce that here rather than describing it:
//!
//! 1. [`event::Actor`] has no `Model` variant, so a model-authored event is unrepresentable.
//! 2. [`event::AppendRequest`] carries no `seq`, `ts` or `signature`; the journal stamps all
//!    three, and [`event::JournalEvent`]'s signature field has no public constructor.
//! 3. [`journal::Journal::replay`] requires an [`journal::OperatorCapability`], because a
//!    model that can replay the log makes forgetting cosmetic (invariant 8).
//!
//! The journal **does not interpret payloads**. It signs the exact serialized bytes and
//! stores them. What a memory *means* is the belief store's business.

#![forbid(unsafe_code)]

pub mod error;
pub mod event;
pub mod journal;
pub mod profile;
pub mod signature;
mod store;

pub use error::JournalError;
pub use event::{Actor, AppendRequest, EventKind, JournalEvent, Seq, TraceId};
pub use journal::{Journal, OperatorCapability};
pub use profile::{Manifest, Profile, DERIVATION_VERSION};
pub use signature::SigningKey;
