//! The permission and approval layer. ARCHITECTURE §2.9, CONTRACTS §9, brief §8.2.
//!
//! > **Never consults the model.** Enforced in code that runs whether or not the model agrees
//! > (invariant 3).
//!
//! ADR-002 (revised) removed the kernel backstop from the ordinary path. **This crate is the
//! wall.** Three consequences bind, and they are stated here rather than only in the ADR
//! because this is the file somebody edits:
//!
//! 1. It is the highest-value target in the system for review and red-teaming. §8.3's ASR
//!    numbers stop measuring defence-in-depth and start measuring whether containment works.
//! 2. `Reversible` tools are checked, not only `Consequential` ones — a workspace write is a
//!    durable channel into a later run's context.
//! 3. `Inert` reads stay unchecked on targets **only because** three non-kernel mechanisms
//!    cover them: untrusted-content classing, return-by-reference, and egress allowlisting.
//!    If any one weakens, that exemption is revisited rather than inherited.
//!
//! # What is not here, stated so it is not assumed
//!
//! **Path scoping is not implemented.** [`scope`] contains a trait and one implementation that
//! refuses every path, and the reason is in that module's header: the traversal suite and the
//! handle discipline are one requirement and ship together (brief §8.3). Until they do, a
//! `Path` argument is blocked. Nothing in this crate should be read as scoping a path.
//!
//! **The trust ledger is not here.** It is M6. [`decision::Tier`] exists because §9's signature
//! takes one; at M2 it comes from the run's autonomy control and nothing promotes itself.

#![forbid(unsafe_code)]

pub mod adjudicate;
pub mod decision;
pub mod egress;
pub mod scope;
pub mod taint;

pub use adjudicate::{
    Adjudication, Adjudicator, ArgValue, Args, Request, blocks_composed_targets,
};
pub use decision::{
    ActionClass, BlastRadius, BlockReason, DecisionId, NoveltyReason, Outcome, PermissionDecision,
    Reason, RiskTier, Tier,
};
pub use egress::{EgressPolicy, Host, HostError};
pub use scope::{PathScope, ScopeError, ScopedPath, Unavailable};
pub use taint::TaintSet;
