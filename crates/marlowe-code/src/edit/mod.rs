//! Edits: `edit`, `multi_edit`, `write` (design 2.1 T5, T6, T8). Track A.
//!
//! | Module | What |
//! |---|---|
//! | [`doc`] | a text file as edits see it: encoding (UTF-8, BOM, UTF-16), dominant line ending |
//! | [`engine`] | one replacement: exact -> EOL-normalised -> whitespace tiers, unique only; did-you-mean |
//! | [`multi`] | plan every edit in memory (cross-file, in order), then commit; all or nothing |
//! | [`numbering`] | `read`'s `cat -n` prefix and its inverse |
//! | [`bench`] | the A5 edit-reliability harness (first-attempt rate, success within 2 retries) |
//!
//! Fuzzy tiers never choose among several candidates, and every result names its tier.

pub mod bench;
pub mod doc;
pub mod engine;
pub mod multi;
pub mod numbering;

pub use doc::{Encoding, Eol, TextDoc};
pub use engine::{apply, Applied, EditError, Tier};
pub use multi::{commit, plan, plan_write, CommitError, FileEdit, MultiError, Planned};

#[cfg(test)]
mod donor_tests;
#[cfg(test)]
mod tests;
