//! The verification harness (design 3.8). Seam skeleton, owned by track D.
//!
//! | Module | What goes here |
//! |---|---|
//! | [`profile`] | detect cargo, uv/pip/poetry + pytest/ruff/mypy, npm/pnpm/yarn/bun, go, cmake/ctest, make, compose, migrations; the hub file list; `.marlowe\project.toml` override |
//! | [`service`] | dev servers in their own Job Object (`marlowe_os::child`, per-launch `env_allow`), readiness by URL/port/log regex, killed at run/session end |
//! | [`http`] | `http_check` on `marlowe-net`, localhost exempt from egress approval |
//! | [`browser`] | snapshot/screenshot/console through `marlowe_exec::render::EdgeRenderer`; last 3 images kept |
//!
//! The tools are declared in `marlowe-exec/src/specs/verify.rs`; `persona/blocks/verify.md` is
//! appended only when this group is live.

#![forbid(unsafe_code)]

pub mod browser;
pub mod http;
pub mod profile;
pub mod service;
