//! The scripted **producer**. No model, no memory, no network.
//!
//! **M2 C2d changed what this crate is.** In M1 it owned the view models and stood in for a daemon
//! that did not exist. The view models now live in `marlowe-view` and there is a real daemon, so
//! this is one producer of a [`marlowe_view::SessionView`] and the other is
//! `marlowe-daemon::project`. What it still is: deterministic, offline, and the thing M1's
//! interaction suite runs against.
//!
//! # It is a dev-dependency of `marlowe-surface`, not a dependency
//!
//! That is the point of the split. `marlowe-surface/src/` cannot name this crate, so a surface
//! cannot reach a producer even by accident — ARCHITECTURE §2.14 enforced by the compiler. The
//! stub is still linked into the surface's *tests*, because a scripted producer is exactly what a
//! headless interaction suite needs.
//!
//! # Determinism
//!
//! Every beat is scheduled at an offset from the moment it was queued, so a session driven by a
//! virtual clock replays identically. That is what lets the §B13 suite render frame N and frame
//! N+1 and diff them.

#![forbid(unsafe_code)]

pub mod amplitude;
pub mod frame_clock;
pub mod script;

pub use frame_clock::Clock;
pub use script::Session;
