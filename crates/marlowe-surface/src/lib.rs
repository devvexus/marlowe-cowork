//! The TUI and the classic CLI. **Rendering and input only.**
//!
//! ARCHITECTURE.md §2.14: *surfaces own rendering and input; they never hold policy, and never
//! hold state the daemon does not have. Surfaces are projections; closing one does not affect a
//! run.*
//!
//! That is enforced by the dependency graph rather than by discipline: this crate depends on
//! `marlowe-stub` (which stands in for the daemon) and not the reverse, so it has no way to
//! produce a session value, a status state, or an inspector row. It renders what it is handed.
//!
//! # What §B13 asks for, and where each answer lives
//!
//! | acceptance row | where |
//! |---|---|
//! | every bordered region has a label and a hotkey | [`region::Region`] — unconstructable without both |
//! | regions reachable by keyboard alone | [`app`] — no drawing, so every path is testable headless |
//! | zero background fills | [`theme`] — no API returns a `bg` |
//! | zero chrome inside a scroll area | [`render::Chrome`] — scroll rects are explicit |
//! | ≤ 1 accent + 3 state + 3 weights | [`theme::Theme`] — seven colours and no eighth |
//! | zero memory regions | `tests/no_memory_surface.rs` |
//! | honest refusal below 120×30 | [`render::draw`] |
//! | classic CLI parity | [`commands`] — one registry, one dispatcher, both surfaces |
//! | rendered markdown cannot forge chrome | [`chrome`] — one definition of what chrome looks like |
//!
//! The tests are in `tests/`; this table is the map, not the enforcement.

#![forbid(unsafe_code)]

pub mod app;
/// The glyphs harness chrome draws, and the reservation that keeps them out of model prose.
pub mod chrome;
pub mod cli;
pub mod clipboard;
pub mod commands;
pub mod doctor;
pub mod escalation;
pub mod inspector;
pub mod keys;
/// Inline maths — legible where it can be, visibly source where it cannot. ADR-047.
pub mod latex;
/// Markdown for the conversation pane, inside §B2's colour budget. ADR-047.
pub mod markdown;
pub mod meter;
pub mod overlay;
pub mod region;
pub mod render;
pub mod theme;
/// A real terminal window per run. `M3-DESIGN.md` §6.
pub mod window;

pub use app::{Action, App, Key, MIN_COLS, MIN_ROWS};
pub use region::{FocusLevel, Region, RegionId, RegionTree, TabId};
pub use theme::{ColorDepth, Theme, ThemeError};
