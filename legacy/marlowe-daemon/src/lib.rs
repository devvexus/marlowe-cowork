//! ARCHITECTURE §6 — one binary, two roles.
//!
//! > **`marlowe`** — thin client. Holds no run state. Renders. Auto-spawns the daemon if absent.
//! > **`marlowe --serve`** — the daemon. Owns the journal, belief store, indexes, runs.
//!
//! # Why the split exists, and what it does and does not buy at M2
//!
//! §6 is explicit: *"The client/daemon split is forced by invariant 6: if the client owned the
//! run, closing the terminal would kill it."* So the daemon owns the `Engine`, the `Journal` and
//! the run table, and [`protocol`] has no frame that hands a client a `Run`, a `Checkpoint` or a
//! transcript — asserted in that module rather than left to discipline.
//!
//! **What this buys at M2:** a run outlives the *client*. Close the terminal, reconnect, and the
//! daemon still owns it and still reports it.
//!
//! **AND, AS OF M3 SESSION A, A RUN OUTLIVES THE DAEMON.** Every iteration writes a
//! `Checkpoint` into the journal, and a new daemon on the same profile seeds its run table from
//! those, lists the interrupted ones, and resumes one from its last completed step. The paragraph
//! that used to stand here said the opposite, correctly, for the whole of M2:
//!
//! > *a run does not yet outlive the daemon. There is no WAL and no checkpoint resume, so a daemon
//! > restart loses in-flight runs. That is M3 and K5.*
//!
//! **What is still not durable, stated so this is not read as more than it is:** the *conversation*.
//! `Daemon::sessions` is in memory, so a restart loses the transcript; what resumes is the
//! interrupted **turn**, with its own window, which the checkpoint carries. And nothing resumes on
//! its own — `marlowe --resume <id>` is a person deciding. See ADR-053 §10.
//!
//! The original paragraph continued: calling this "durable runs" would be exactly the adjacent-measurement failure this
//! project has logged fifteen times.
//!
//! # The first frame does not come from here
//!
//! §B13 budgets 150 ms to first frame and §6 names the split as what buys it: *"the client has
//! almost nothing to initialize, and the header paints before the daemon connection resolves."*
//! Nothing in this crate is on the path to the first frame, and `tests/first_frame.rs` asserts
//! that a client can render before a daemon exists at all.

#![forbid(unsafe_code)]

/// The daemon's own announcements, kept as well as printed. §B7's Status tab reads them.
pub mod announce;
pub mod auth;
pub mod client;
pub mod control_plane;
pub mod clock;
pub mod daemon;
pub mod engine;
pub mod escalation;
pub mod live;
pub mod mcp;
pub mod memory;
pub mod onboarding;
pub mod recall;
pub mod roster;
/// The `use` tool: skill discovery and progressive disclosure. ADR-051.
pub mod skills;
pub mod project;
mod staleness;
pub mod protocol;
/// The client half of the control plane, and the projection a run window renders.
pub mod watch_client;

pub use client::{Client, ClientError};
pub use daemon::{Daemon, DaemonConfig, DaemonError, ModelProviderChoice, governance_prompt, workspace_map};
pub use engine::{tier1_runtime_for, HybridEngine};
pub use live::LiveSession;
pub use project::{apply_events, view_from_status, PROVIDERS};
pub use protocol::{Event, Request, RunFrame, StatusReport};
pub use watch_client::RunProjection;

/// The loopback port the daemon listens on.
///
/// Loopback only, and that is not a placeholder for "configurable later": a daemon that owned a
/// user's filesystem and answered from a routable address would be a remote code execution
/// service. If a remote client is ever wanted, it is a separate decision with authentication in
/// it, argued in `DECISIONS.md`.
///
/// **Loopback is per-machine, not per-user**, which is why [`auth`] exists: binding here does not
/// keep another logged-in user off the socket.
pub const DEFAULT_DAEMON_PORT: u16 = 11435;

/// Where the profile lives when nobody says otherwise.
///
/// Under the user's data directory rather than the workspace: a journal inside the workspace would
/// be reachable by `read`, and ARCHITECTURE invariant 8 requires the journal to sit **outside the
/// model's filesystem scope** so that forgetting is not cosmetic.
///
/// **This is the single definition, and it moved here so it could be.** It used to live in
/// `marlowe::agent`, which the client cannot see — so a client resolving the default profile root
/// for [`auth`] would have needed a second copy of this function, and two copies of a path that
/// must match is the shape that makes a mismatch unobservable. `marlowe::agent::default_profile_root`
/// now delegates.
pub fn default_profile_root() -> std::path::PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .or_else(|| std::env::var_os("XDG_DATA_HOME"))
        .or_else(|| std::env::var_os("HOME"))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("marlowe").join("default-profile")
}
