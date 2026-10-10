//! Settings (design 3.5, 2.3 M9). Seam skeleton, owned by track E.
//!
//! | Module | What goes here |
//! |---|---|
//! | [`layers`] | `<home>\settings.toml`, `.marlowe\settings.toml`, `.marlowe\settings.local.toml`, CLI flags; merge order and which layer a change writes (`marlowe_proto::SettingsScope`) |
//! | [`rules`] | allow/ask/deny rule patterns (`bash(git push*)`, `edit(src/**)`, `web_fetch(domain:x)`, `mcp__srv__tool`) as configured; adjudication stays in `marlowe-permission` |
//! | [`hooks`] | hook configuration (all Claude Code events), project trust by digest |
//! | [`import`] | read-only import of `.claude/settings.json` hooks and permissions |
//!
//! The daemon side is `marlowe-daemon/src/mode_ops.rs`; the loop side is a `ToolMiddleware`.

#![forbid(unsafe_code)]

pub mod hooks;
pub mod import;
pub mod layers;
pub mod rules;
