//! **The second and last place in the system that reads a real clock.**
//!
//! The first is `crates/marlowe/src/elapsed.rs`, which fences the one contract-legitimate read
//! (`cost.latency_ms`, section 4.2). This module fences the other one: a terminal that animates
//! needs a monotonic time base, and `marlowe --tui --timing-probe` has to measure milliseconds to
//! first frame because K4 is stated in them.
//!
//! `determinism_guard.rs` names both files. A third read cannot appear without appearing there.
//!
//! # Why the clock lives in the stub and not in the surface
//!
//! ARCHITECTURE.md §2.14: *surfaces hold no policy and no state the daemon does not have.* Time is
//! state. The stub stands in for the daemon, so the stub owns the clock and the surface is handed
//! a `now_ms` wherever it needs one.
//!
//! That is not bookkeeping — it is what makes §B13's flicker rows measurable. Every render in
//! `marlowe-surface` is a pure function of `(state, now_ms)`, so a test can render frame N and
//! frame N+1 at chosen times and diff the two buffers cell by cell. A surface that read
//! `Instant::now()` inside a widget could not be diffed, and "zero flicker" would go back to being
//! a matter of opinion.

use std::time::Instant;

/// A monotonic millisecond time base.
///
/// Two implementations and no third: [`Clock::real`] drives the running binary, [`Clock::virtual_`]
/// drives every test. The virtual one is not a mock of the real one — it is the same type with a
/// different source, so a test exercises the production path.
#[derive(Debug)]
pub struct Clock {
    source: Source,
}

#[derive(Debug)]
enum Source {
    Real { origin: Instant },
    Virtual { now_ms: u64 },
}

impl Clock {
    /// The running binary's clock. Zero is the moment this is constructed, which is deliberately
    /// *before* the first frame: `--timing-probe` reports first-frame latency as a reading of this
    /// clock, so the origin has to sit at process start rather than at first render.
    pub fn real() -> Self {
        Self {
            source: Source::Real {
                origin: Instant::now(),
            },
        }
    }

    /// A clock that only moves when a test moves it.
    pub fn virtual_(now_ms: u64) -> Self {
        Self {
            source: Source::Virtual { now_ms },
        }
    }

    /// Milliseconds since this clock's origin.
    pub fn now_ms(&self) -> u64 {
        match &self.source {
            Source::Real { origin } => origin.elapsed().as_millis() as u64,
            Source::Virtual { now_ms } => *now_ms,
        }
    }

    /// Move a virtual clock forward. Panics on a real clock rather than silently doing nothing —
    /// a test that thinks it controls time and does not is a test that proves nothing.
    pub fn advance(&mut self, ms: u64) {
        match &mut self.source {
            Source::Virtual { now_ms } => *now_ms += ms,
            Source::Real { .. } => {
                panic!("advance() called on a real clock; construct Clock::virtual_ for tests")
            }
        }
    }

    /// True if this clock can be advanced. The surface never asks — it exists so the binary can
    /// refuse to enter the deterministic replay path with a real clock.
    pub fn is_virtual(&self) -> bool {
        matches!(self.source, Source::Virtual { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virtual_clock_moves_only_when_advanced() {
        let mut c = Clock::virtual_(0);
        assert_eq!(c.now_ms(), 0);
        assert_eq!(c.now_ms(), 0);
        c.advance(250);
        assert_eq!(c.now_ms(), 250);
    }

    #[test]
    #[should_panic(expected = "advance() called on a real clock")]
    fn advancing_a_real_clock_panics() {
        Clock::real().advance(1);
    }
}
