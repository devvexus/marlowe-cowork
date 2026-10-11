//! The verification harness (design 3.8): how an agent checks its own work.
//!
//! | Module | What it does |
//! |---|---|
//! | [`profile`] | finds the stacks (cargo, uv/poetry/pip, npm/pnpm/yarn/bun, go, cmake, make, compose), their build/test/lint/format/typecheck/install commands, services, migrations, lockfiles; [`overrides`] reads `.marlowe/project.toml`; [`hubs`] lists the files parallel work collides on |
//! | [`run`] | runs those commands and the gate; [`output`] reads failing test ids and compiler findings out of the output and keeps the failures when a log is long |
//! | [`service`] | dev servers in their own Job Object, ready by log, port or URL, killed with the host |
//! | [`http`] | `http_check`: localhost plain HTTP from here, other hosts https through `marlowe-net` |
//! | [`browser`] | a headless-browser request/report and step language; the driver (Edge over DevTools) is `marlowe-exec`'s |
//! | [`migrate`] | migrations against a throwaway sqlite database, schema read back, round trip |
//! | [`tools`] | the four tools as plans and executions over JSON arguments |
//!
//! Every child process starts through `marlowe_os::child` ([`env::Env::request`]): API keys are
//! scrubbed from its environment unless the launch names them in `env_allow`, and it lives in a
//! kill-on-close Job Object. Commands are a program and arguments, never a shell string
//! ([`cmd`]). The tools are declared in `marlowe-exec/src/specs/verify.rs`;
//! `persona/blocks/verify.md` is appended only when that group is live.

#![forbid(unsafe_code)]

pub mod browser;
pub mod cmd;
pub mod env;
pub mod http;
pub mod hubs;
pub mod migrate;
pub mod output;
pub mod overrides;
pub mod profile;
pub mod run;
pub mod service;
pub mod state;
pub mod tools;

mod stacks;

pub use env::Env;
pub use state::VerifyState;
