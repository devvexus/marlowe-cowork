//! The production harness clock. **A declared fence, not an exemption.**
//!
//! CONTRACTS §4.5 forbids reading a system clock on any path reachable from the §4 interfaces —
//! and states, in the same paragraph, what the legitimate case is:
//!
//! > **In production the harness supplies the real clock**; under eval M0a supplies a synthetic
//! > one. Same code path, so the eval exercises the shipping behaviour rather than a test double.
//!
//! The daemon **is** that harness. Somewhere, exactly once, real time has to enter the system.
//! This file is that place, and it is a whole file containing one struct so the fence in
//! `marlowe/tests/determinism_guard.rs` is narrow: exempting `daemon.rs` would have blinded the
//! guard to every future clock read in a 400-line file, which is the failure its own header warns
//! about for directories.
//!
//! **Nothing here is reachable from §§4.1, 4.6 or 4.7.** The eval adapter is a separate mode that
//! never constructs a `Daemon`, and it supplies M0a's synthetic clock as it always did.

use marlowe_loop::ClockSource;

/// Real time, read once per loop iteration, injected as a port.
///
/// The loop never calls this directly — it holds a `&mut dyn ClockSource`, which is what let the
/// guard catch an executor reading its own clock in M2 C2a.
pub struct SystemClock;

impl ClockSource for SystemClock {
    fn now_ms(&mut self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }
}
