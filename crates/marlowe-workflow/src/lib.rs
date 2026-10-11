//! Workflow scripts (design 2.11, 3.11): the engine behind the `workflow` tool.
//!
//! A workflow is a hermetic Starlark script (Meta's pure-Rust `starlark`; Python-like, no Python
//! runtime, no `while`, no recursion, so it always ends; deterministic) that orchestrates agents
//! through a small host API: `agent`, `parallel`, `pipeline`, `phase`, `log`, `workflow`, `args`,
//! `budget`. The script has no I/O of its own; every effect is an agent's tool call, through
//! permissions, taint and the journal. Scripts are validated before they run ([`check`]) and the
//! errors go back to the model.
//!
//! ```python
//! META = {"name": "triage", "description": "Read each file and rank the risky ones", "phases": ["read", "rank"]}
//!
//! def main(args):
//!     phase("read")
//!     notes = pipeline(args["files"], lambda prev, path, i: agent(f"Summarise the risks in {path}", label=path))
//!     phase("rank")
//!     return agent("Rank these by risk:\n" + "\n".join([n for n in notes if n]),
//!                  schema={"type": "object", "properties": {"top": {"type": "array", "items": {"type": "string"}}}, "required": ["top"]})
//! ```
//!
//! Concurrency without coroutines: see [`engine`] (deterministic replay). The engine knows
//! nothing about the crew: it asks an [`AgentHost`] to run each agent, so `marlowe-crew`
//! (`playbook/script/`) depends on this crate and not the other way round.

#![deny(unsafe_code)]

pub mod convert;
pub mod engine;
pub mod findings;
pub mod host;
pub mod journal;
pub mod pools;
pub mod progress;
pub mod saved;
pub mod schema;
pub mod scripted;
pub mod service;
pub mod validate;

pub use engine::{
    call_key, check, default_concurrency, run, Budget, RunConfig, RunOutcome, ScriptResolver, WorkflowError, MAX_AGENTS,
    MAX_CONCURRENCY, MAX_ITEMS,
};
pub use host::{AgentCall, AgentHost, AgentOpts, AgentResult, AgentStatus, Completion, Control, NoObserver, Observer, WfEvent};
pub use journal::{AgentRecord, Line, Resume};
pub use progress::Progress;
pub use scripted::{Scripted, ScriptedHost};
pub use service::{LaunchRequest, Launched, Prepared, Waited, WorkflowLauncher, Workflows};
pub use validate::{Diagnostic, Meta, Validated};

/// blake3 of a script's text: the trust key (first run of a digest asks).
pub fn digest(source: &str) -> String {
    blake3::hash(source.as_bytes()).to_hex().to_string()
}
