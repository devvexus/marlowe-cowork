//! CONTRACTS.md section 4 as Rust types, plus the one predicate every layer above needs.
//!
//! This crate is the M0a↔M0b boundary. It depends on no other Marlowe crate, and nothing
//! here knows what a retriever is. **The JSON is normative; these types are its binding**
//! (section 4.2b), because the scorer is Python and the implementation is Rust.
//!
//! # The header used to end *"— and nothing else"*, and [`text`] is why it no longer does
//!
//! Amended 2026-08-17 rather than quietly falsified. [`text`] holds the character predicate that
//! decides what may appear in a contract value **and** what may reach a terminal. It is not a
//! section 4 type and it does not pretend to be.
//!
//! It is here because this is the only crate both sides can reach. The predicate is needed by
//! `marlowe-loop` (the value check), by `marlowe` and by `marlowe-surface` (three render sites) —
//! and those last two sit above the loop, so a home anywhere else means either an inverted
//! dependency or a second copy. A second copy is the defect: a checker that refuses `U+202E`
//! beside a renderer that prints it is a defence that reports success. See [`text`]'s own header.
//!
//! **This is a deliberate widening of one crate's scope to keep one definition, and it is the
//! whole of it.** If a third thing arrives wanting to live here on the same argument, that is the
//! signal to cut a leaf crate rather than to widen this sentence again.
//!
//! Three interfaces, and there is no fourth. Section 4: *"M0b may expose no other surface
//! to M0a. If the harness needs a fourth, the split is leaking and the fix belongs in
//! CONTRACTS.md, not in the harness."*
//!
//! # Determinism
//!
//! Section 4.0.8 and the `repro` acceptance criterion together mean a run at a fixed seed
//! and clock must reproduce **bit-identically**. Two hazards live in this crate's
//! neighbourhood and are called out here because they pass locally and fail in `repro`:
//!
//! 1. **`HashMap` iteration order is randomised per process** in Rust. Any map whose
//!    iteration order can reach the wire must be a `BTreeMap`, or be collected and sorted
//!    explicitly. A workspace test greps for the type to keep this honest.
//! 2. **No identifier may derive from anything time-adjacent.** The clock probe shifts every
//!    supplied timestamp by ten years and compares injected `memory_id`s for equality, so an
//!    id built from a timestamp fails translation invariance — and a run that reads the
//!    system clock for an id fails `repro` on the second run.

#![forbid(unsafe_code)]

pub mod common;
pub mod frame;
pub mod text;
pub mod wire;

pub use common::{
    AbstentionReason, Channel, Clock, Fidelity, PayloadKind, Speaker, TrustClass,
};
pub use text::{is_renderable, sanitize, sanitize_line, sanitize_prose, Shape};
pub use frame::{ErrorKind, FrameError, Op, RequestFrame, ResponseFrame};
pub use wire::{
    AnswerCost, AnswerLatency, AnswerRequest, AnswerResponse, ContractVersion,
    ExclusivityError, GateStamp, IngestCost, IngestLatency, IngestRequest, IngestResponse,
    InjectedMemory, Origin, RejectedWrite, RetrievalBudget, RetrievalCost, RetrievalLatency,
    RetrievalRequest, RetrievalResponse, Turn, Written, CONTRACT_VERSION,
};
