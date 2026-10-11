//! Workflow scripts (design 2.11, 3.11): the engine behind the `workflow` tool.
//!
//! A workflow is a hermetic Starlark script (Meta's pure-Rust `starlark`; Python-like, no Python
//! runtime, no loops that can fail to end, deterministic) that orchestrates agents through a small
//! host API: `agent`, `parallel`, `pipeline`, `phase`, `log`, `args`, `budget`. The script has no
//! I/O of its own; every effect is an agent's tool call, through permissions, taint and the
//! journal. Scripts are validated before they run and the errors are handed back to the model.
//!
//! The engine knows nothing about the crew: it asks a host (a trait this crate defines) to run an
//! agent, so `marlowe-crew` (the durable run, `playbook/script/`) depends on this crate and not the
//! other way round.
//!
//! Seam stub: empty until track W lands. The crate is a workspace member and a dependency of
//! `marlowe-crew` and `marlowe-daemon` already, so W adds `starlark` to this crate's manifest and
//! edits no shared file. The wire types are in `marlowe_proto::background`.

#![forbid(unsafe_code)]
