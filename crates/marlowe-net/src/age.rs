//! **The one place `marlowe-net` reads a monotonic clock, and it exists to keep that fence narrow.**
//!
//! Two things in this crate need to know how old something is: a DNS cache entry, and a pooled
//! connection sitting idle. Both are connection hygiene — neither reads a wall clock, produces a
//! timestamp, or puts a value anywhere a journal, a memory id or a `repro` hash can see it.
//!
//! # Why a whole module for twenty lines
//!
//! `determinism_guard.rs` fences clock reads by path, and the reason it names one is the reason
//! this file exists. Its own comment on `marlowe-daemon/src/clock.rs`:
//!
//! > It is a whole file holding one struct so this fence stays narrow: exempting `daemon.rs` would
//! > blind the guard to every future clock read in it, which is the failure this test's own header
//! > warns about for directories.
//!
//! `marlowe-net/src/lib.rs` is four hundred lines of fetch path. Fencing it would exempt every
//! future clock read in the file that actually issues requests — including one that *did* reach a
//! result — and it would do so silently, because the fence would already be there and green. So
//! the two reads move here and the fence names this file alone.
//!
//! **This is not a claim that reading time is free.** It is a claim that these two reads cannot
//! reach output, made narrow enough that the next one has to be argued for separately.

use std::time::{Duration, Instant};

/// When something happened, for the sole purpose of asking how long ago.
///
/// Deliberately **not** convertible to a timestamp, a `u64`, or anything serialisable. The type
/// offers exactly one question — `elapsed()` — so a value stamped here cannot end up in a payload
/// even by accident. A `pub` field holding an `Instant` would have made that possible; this does
/// not.
#[derive(Debug, Clone, Copy)]
pub struct Mark(Instant);

impl Mark {
    /// Stamp now. **The only monotonic clock read in this crate.**
    pub fn now() -> Self {
        Self(Instant::now())
    }

    /// How long since the mark. Monotonic, so it cannot go backwards across a clock adjustment.
    pub fn elapsed(&self) -> Duration {
        self.0.elapsed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mark_measures_forward_and_offers_nothing_else() {
        // The property worth pinning is the *absence*: `Mark` exposes no way to obtain a time
        // value, only an interval. If someone later adds one, this comment is where to argue it,
        // and the guard's fence on this file is what makes that a deliberate act.
        let m = Mark::now();
        assert!(m.elapsed() < Duration::from_secs(60));
    }
}
