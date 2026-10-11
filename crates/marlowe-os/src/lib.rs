//! Thin OS wrappers. The only crate in the workspace allowed `unsafe`; every block is a direct
//! Win32 call with its invariant stated beside it.
//!
//! [`JobObject`] owns a Windows Job Object created with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`:
//! when the last handle closes (drop, or the daemon dying), every process in the job dies,
//! grandchildren included. [`spawn_in_job`] starts a child suspended, assigns it, then resumes
//! it, so nothing the child starts can escape the job. [`JobObject::release`] lets a finished
//! command's background processes run on (the conductor's old behaviour). marlowe-exec uses it for every child
//! process (conductor and crew) in place of `taskkill /T` tree kills.
//!
//! [`console::query`] asks the terminal a question (Device Attributes, background colour) and
//! reads the reply, which on Windows needs virtual-terminal input switched on for the exchange.
//!
//! [`child`] is the one way the harness starts a child process: scrubbed environment (API keys
//! only through a per-launch `env_allow`), Job Object, bounded runs. [`workspace`] is the
//! filesystem-and-exec port the file, search and code tools go through ([`workspace::LocalFs`]
//! here; `DockerExec` from track K), plus the [`workspace::WorktreeProvider`] seam the crew
//! implements.
//!
//! Off Windows the types exist so dependents compile; jobs are unsupported there.

pub mod child;
pub mod console;
/// Job Object internals. Public as a module so B's additions are reachable as
/// `marlowe_os::job::X` without editing this file. [`spawn_in_job`] and
/// [`spawn_in_job_with_flags`] skip the credential scrub: harness code starts children through
/// [`child`] (`tests/spawn_audit.rs` counts every direct call), and seam-crew makes them
/// `pub(crate)` once the crew's last caller has moved.
pub mod job;
pub mod toast;
pub mod workspace;

pub use child::{ChildProc, ChildSpec, Containment, RunLimits, RunOutput, SCRUBBED_ENV};
pub use job::{spawn_in_job, spawn_in_job_with_flags, JobChild, JobObject};

/// An OS wrapper failure.
#[derive(Debug, thiserror::Error)]
pub enum OsError {
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    /// The Win32 call that failed and its `GetLastError` code.
    #[error("{call} failed (os error {code})")]
    Win32 { call: &'static str, code: u32 },
    #[error("{0} is not supported on this platform")]
    Unsupported(&'static str),
    /// A launch tried to set a scrubbed credential variable it did not list in `env_allow`.
    #[error("{0} is a credential variable; a launch passes it only by naming it in env_allow")]
    EnvNotAllowed(String),
}

#[cfg(test)]
mod tests;
