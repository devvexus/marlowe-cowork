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
//! Off Windows the types exist so dependents compile; jobs are unsupported there.

pub mod console;
mod job;

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
}

#[cfg(test)]
mod tests;
