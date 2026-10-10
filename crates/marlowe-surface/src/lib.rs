//! The TUI and the classic CLI. **Rendering and input only.**
//!
//! ARCHITECTURE.md §2.14: *surfaces own rendering and input; they never hold policy, and never
//! hold state the daemon does not have. Surfaces are projections; closing one does not affect a
//! run.*
//!
//! That is enforced by the dependency graph rather than by discipline: this crate depends on
//! `marlowe-view` (shapes only) and no producer, so it has no way to produce a session value or
//! a status state. It renders what it is handed.
//!
//! # What is here
//!
//! The reusable rendering foundation: the palette ([`theme`]), the region contract ([`region`]),
//! markdown and maths ([`markdown`], [`math`]), the meter ([`meter`], [`amplitude`],
//! [`frame_clock`]), chrome glyphs ([`chrome`]), keys, clipboard, overlays and `doctor`. The
//! application that arranges them is not part of this crate.
//!
//! # What §B13 asks for, and where each answer lives
//!
//! | acceptance row | where |
//! |---|---|
//! | every bordered region has a label and a hotkey | [`region::Region`] — unconstructable without both |
//! | zero background fills | [`theme`] — no API returns a `bg` |
//! | ≤ 1 accent + 3 state + 3 weights | [`theme::Theme`] — seven colours and no eighth |
//! | rendered markdown cannot forge chrome | [`chrome`] — one definition of what chrome looks like |
//!
//! The tests are in `tests/`; this table is the map, not the enforcement.

#![forbid(unsafe_code)]

pub mod amplitude;
/// The glyphs harness chrome draws, and the reservation that keeps them out of model prose.
pub mod chrome;
pub mod clipboard;
pub mod doctor;
pub mod frame_clock;
pub mod keys;
/// Markdown for the conversation pane, inside §B2's colour budget. ADR-047.
pub mod markdown;
/// LaTeX maths typeset on the character grid: 2D display, one-line inline, never the source.
pub mod math;
pub mod meter;
pub mod overlay;
pub mod region;
pub mod theme;

pub use doctor::{MIN_COLS, MIN_ROWS};
pub use region::{FocusLevel, Region, RegionId, RegionTree, TabId};
pub use theme::{ColorDepth, Theme, ThemeError};
