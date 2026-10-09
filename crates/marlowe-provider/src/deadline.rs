//! **The one place `marlowe-provider` reads a monotonic clock, and it exists to keep that fence
//! narrow.**
//!
//! One thing in this crate needs to know how long something has taken: `hybrid::start` waits for a
//! `llama-server` it spawned to answer `/health`, and a child that never answers would hang the
//! daemon forever with no output and no indication why. That is the failure Addendum B §B5 exists
//! to prevent, and counting iterations cannot solve it — the poll loop's own HTTP probes carry
//! multi-second timeouts, so `iterations × POLL_INTERVAL` would understate the elapsed time by
//! whatever the probes cost, and report the understatement as a measurement.
//!
//! # Why a whole module for twenty lines, and why not `marlowe_net::age::Mark`
//!
//! `Mark` is the same type and it is **already fenced and already argued**, so reusing it was the
//! first attempt: zero new fences, zero new exemptions. It compiled, the determinism guard passed,
//! and **it broke a different documented boundary** —
//! `marlowe-provider/tests/no_tls_in_the_default_path.rs` failed with
//! *"`marlowe-provider` now reaches `["rustls", "webpki-roots"]`"*.
//!
//! ADR-031 §2.3 makes `marlowe-net` the whole of the TLS supply-chain surface **precisely so that
//! this crate's dependency tree stays evidence for ADR-028**: local-first, loopback only, no
//! account, no key, no TLS. A dependency added for a twenty-line timer would have dragged `rustls`
//! and a vendored root store into the crate whose empty TLS surface is the argument. The saving of
//! one fence entry is not worth the loss of that evidence.
//!
//! So this is the fourth instance of a pattern this project already has three of —
//! `marlowe-daemon/src/clock.rs`, `marlowe-net/src/age.rs`, `marlowe-mcp/src/deadline.rs` — each a
//! whole file holding one type, each fenced by path, each for the same stated reason:
//!
//! > It is a whole file holding one struct so this fence stays narrow: exempting `daemon.rs` would
//! > blind the guard to every future clock read in it.
//!
//! `llamacpp.rs` is eleven hundred lines and `hybrid.rs` spawns processes. Fencing either would
//! exempt every future clock read in them, silently, because the fence would already be green.
//!
//! **This is not a claim that reading time is free.** It is a claim that this one read cannot reach
//! output, made narrow enough that the next one has to be argued for separately.

use std::time::{Duration, Instant};

/// When something started, for the sole purpose of asking how long ago.
///
/// Deliberately **not** convertible to a timestamp, a `u64`, or anything serialisable. The type
/// offers exactly one question — [`Self::elapsed`] — so a value stamped here cannot end up in a
/// payload, a journal entry, a memory id or a `repro` hash even by accident. A `pub` field holding
/// an `Instant` would have made that possible; this does not.
#[derive(Debug, Clone, Copy)]
pub struct Started(Instant);

impl Started {
    /// Stamp now. **The only monotonic clock read in this crate.**
    pub fn now() -> Self {
        Self(Instant::now())
    }

    /// How long since. Monotonic, so it cannot go backwards across a clock adjustment — which
    /// matters for a health wait that would otherwise be extendable by a machine's NTP sync.
    pub fn elapsed(&self) -> Duration {
        self.0.elapsed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_start_measures_forward_and_offers_nothing_else() {
        // The property worth pinning is the *absence*: `Started` exposes no way to obtain a time
        // value, only an interval. If someone later adds one, this comment is where to argue it,
        // and the guard's fence on this file is what makes that a deliberate act rather than a
        // convenience.
        let s = Started::now();
        assert!(s.elapsed() < Duration::from_secs(60));
    }
}
