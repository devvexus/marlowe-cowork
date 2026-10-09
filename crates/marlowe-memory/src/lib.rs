//! CONTRACTS.md sections 2–3 — the belief store and its provenance machinery.
//!
//! Memory is the spine, not a subsystem. Concretely, that means: **one envelope for every
//! belief** ([`entry::MemoryEntry`]), **trust derived at write time and never declared**
//! ([`trust`]), and a store that is **rebuildable from the log** ([`store::BeliefStore`])
//! rather than an authority of its own.
//!
//! The three things a reader should check first, because each is a place where a plausible
//! default would delete a requirement while every test stayed green:
//!
//! | Where | The default that would hide a failure |
//! |---|---|
//! | [`trust::trust_for_channel`] | a `_ =>` arm — the laundering suite would pass while measuring nothing |
//! | [`entry::MemoryEntry::is_injection_candidate`] | dropping an exclusion — maturation is the one that can go while every latency and precision test still passes |
//! | [`store::BeliefStore::derive`] | assuming `Fidelity::Record` on replay — the rebuild would resurrect forgotten memories (invariant 9) |
//! | [`gate::FrozenGate::load`] | a fallback weight vector — a run would stamp `frozen-v1` while scoring with weights nobody fit |

#![forbid(unsafe_code)]

pub mod claim;
pub mod consolidate;
pub mod cue;
pub mod entry;
pub mod error;
pub mod gate;
pub mod ingest;
pub mod operating_point;
pub mod probe;
pub mod rerank;
pub mod retrieve;
pub mod store;
pub mod tags;
pub mod trust;
pub mod write;

pub use claim::{
    correct_claim, forget_claim, remember_claim, ClaimRejected, ClaimWrite, WriteReceipt,
};
pub use entry::{memory_id, MaturationPolicy, MemoryEntry, MemoryId, MATURATION_WINDOW_MS};
pub use error::MemoryError;
pub use gate::{FrozenGate, GateError, GATE_VERSION, THRESHOLD};
pub use ingest::{ingest, IngestOutcome};
pub use operating_point::{Abstention, Coverage, OperatingPoint, OperatingPointError};
pub use store::{BeliefSnapshot, BeliefStore, DERIVATION_VERSION, SNAPSHOT_FORMAT};
pub use tags::{BeliefKind, BeliefScope, Provenance, Tags, Utterance};
pub use trust::{check_actor, effective_trust, trust_for_channel, RejectionReason};
pub use write::{correct_belief, write_belief, Author, BeliefReceipt, BeliefWrite};
