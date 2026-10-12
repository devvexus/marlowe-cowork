//! Settings (design 3.5, 2.3 M9): layered TOML files plus a read-only import of Claude Code's
//! `.claude/settings.json`.
//!
//! | Layer (low to high) | File | Trusted |
//! |---|---|---|
//! | user | `<home>\settings.toml` | always |
//! | claude project | `<ws>\.claude\settings.json` (import, read-only) | by digest |
//! | project | `<ws>\.marlowe\settings.toml` (checked in) | by digest |
//! | claude local | `<ws>\.claude\settings.local.json` (import, read-only) | by digest |
//! | local | `<ws>\.marlowe\settings.local.toml` ("always for this project") | by digest |
//! | cli | `--settings`, headless flags | always |
//!
//! Rules from every layer are pooled (deny > ask > allow, decided in `marlowe-permission`).
//! Scalars take the highest layer that sets them. A project's hooks, allow rules, extra
//! directories and project commands apply only once the user trusts the project's digest
//! ([`trust`]); its deny and ask rules always apply, and no project file can select `bypass` or
//! `dont_ask`.
//!
//! | Module | What |
//! |---|---|
//! | [`file`] | the TOML schema ([`SettingsFile`]) |
//! | [`layers`] | loading, merging, the trust digest, the wire view |
//! | [`rules`] | configured rule rows and validation |
//! | [`hooks`] | hook rows, Claude Code event names, matchers, project trust |
//! | [`import`] | `.claude/settings.json` -> [`SettingsFile`] |
//! | [`edit`] | writes (rules, dotted keys) that keep the file's comments |
//! | [`protocol`] | hook input JSON per event, output fields and exit codes (Codex's schemas are the oracle) |
//!
//! The daemon side is `marlowe-daemon/src/mode_ops.rs`; the loop side is `marlowe-agent`'s
//! `approval` (modes, rules) and `hooks` (hook runner) modules.

#![forbid(unsafe_code)]

pub mod edit;
pub mod file;
pub mod hooks;
pub mod import;
pub mod layers;
pub mod protocol;
pub mod rules;

#[cfg(test)]
mod tests;

pub use file::{HookCommand, MatcherGroup, SettingsFile};
pub use hooks::{HookEntry, HookKind, HookEvent};
pub use layers::{Layer, Settings, Source};
pub use rules::RuleEntry;

/// A settings error for the client.
#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("{0}")]
    Invalid(String),
    #[error("{path}: {message}")]
    File { path: String, message: String },
    #[error("{0}")]
    Io(#[from] std::io::Error),
}
