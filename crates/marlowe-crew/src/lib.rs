//! The Crew. An agent is a role card + a model/effort slot + a tool allowlist + a budget + a
//! sandbox; nothing more.
//!
//! The [`Supervisor`] owns every run: a queue, per-provider concurrency limits, one thread per
//! active run (behind `catch_unwind`), cancel/pause/resume/steer, and budget enforcement. Runs are
//! durable rows in `cowork.db` (`crew_runs` + `crew_run_steps`) and resume from their last
//! completed step after a daemon restart. Runs are depth 1: crew tool hosts never expose
//! `dispatch`. Results are prose with provenance ([`Dispatch`]); safety comes from the
//! [`Sandbox`] and a handful of approval points, not from typed contracts between steps.

#![forbid(unsafe_code)]

pub mod dispatch;
pub mod playbook;
mod role;
mod run;
mod runner;
pub mod sandbox;
pub mod shell_policy;
mod store;
mod supervisor;
mod tools;

pub use dispatch::{DispatchArgs, DispatchReceipt, DispatchTools, Estimate, DISPATCH_TOOL};
pub use marlowe_proto::{
    Citation, Confidence, CrewBudget, CrewRunInfo, CrewRunStatus, CrewSpent, CrewStep, DiffSummary, Dispatch, Playbook,
    RoleInfo, RunId, SandboxInfo, SandboxKind,
};
pub use role::{parse_role, RoleCard, RoleError, RoleSource, Roles, BUILTIN_ROLES};
pub use run::{title_from_goal, Run, StepRecord};
pub use sandbox::{repo_digest, MergeOutcome, Sandbox, ShellVerdict};
pub use store::RunStore;
pub use supervisor::{
    result_memory, CrewConfig, CrewDeps, CrewEvent, CrewSetup, GpuBusy, RunApprovals, RunNotice, Supervisor, INTERRUPTED,
};
pub use tools::{CrewToolFactory, CrewTools};

/// A crew failure.
#[derive(Debug, thiserror::Error)]
pub enum CrewError {
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("database: {0}")]
    Db(#[from] rusqlite::Error),
    #[error(transparent)]
    Brains(#[from] marlowe_brains::BrainsError),
    #[error(transparent)]
    Role(#[from] RoleError),
    #[error("unknown role '{0}'")]
    UnknownRole(String),
    #[error("no run {0}")]
    UnknownRun(RunId),
    /// The run is in a state that does not allow the operation (e.g. merging a running run).
    #[error("run {run} is {status}: {why}")]
    BadState { run: RunId, status: &'static str, why: String },
    #[error("git: {0}")]
    Git(String),
    #[error("the run was cancelled")]
    Cancelled,
    /// An agent step ended without a usable result.
    #[error("step {step} failed: {message}")]
    Step { step: String, message: String },
    #[error("the crew supervisor has stopped")]
    Stopped,
    /// The operation is declared but its implementation has not landed (a seam stub).
    #[error("not wired yet: {0}")]
    NotWired(&'static str),
}

/// Unix epoch milliseconds.
pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests;
