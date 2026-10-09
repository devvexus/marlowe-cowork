//! **Cross-process exclusion for tests that need a scarce machine resource.**
//!
//! # Why this exists
//!
//! `cargo test --workspace` runs test **binaries concurrently** — verified 2026-08-26 by running
//! `-p marlowe-daemon -p marlowe-memory` together and watching `control_plane` fail while every
//! other target was still starting. Within a binary, tests are threads. So a test that needs the
//! GPU, or a specific TCP port, is competing with every other test target on the machine.
//!
//! **Four tests failed that way and all four pass alone**, measured the same day:
//!
//! | test | alone | under `--workspace` |
//! |---|---|---|
//! | `two_daemons_never_collide_and_the_derived_port_would_have` | ok | FAILED |
//! | `the_client_reaches_its_own_daemons_control_plane_when_two_are_adjacent` | ok | FAILED |
//! | `a_silent_peer_does_not_wedge_the_daemon` | ok (7.54 s) | FAILED |
//! | `the_reserve_can_push_the_rerank_off_a_card_that_looks_free` | ok (3.74 s) | FAILED |
//!
//! `rerank_provider` additionally **wedged the whole suite twice**, once for 12 minutes and once
//! for 19, holding a CUDA session while other targets tried to open their own.
//!
//! # The cost of NOT having this, stated plainly
//!
//! Every one of those failures is indistinguishable from a real regression by reading the suite
//! output. Three separate sessions this week diagnosed one of them, and two got it wrong in
//! opposite directions — once calling load-sensitivity a deterministic defect, once the reverse.
//! **A suite that cannot say "this test needs the machine to itself" produces red that nobody can
//! interpret**, and the interpretation cost is paid every time.
//!
//! # A POLL COUNT, NOT A DEADLINE
//!
//! Same reasoning as `cue::dense::vram::bounded_output`: a real deadline needs `Instant::now()`,
//! and reaching for a clock in test support is how a clock read ends up somewhere the determinism
//! guard cares about. A fixed count gives the only property needed — **a ceiling** — and sleep
//! drift makes the bound approximate, which nothing here depends on.
//!
//! # What it does NOT do
//!
//! It does not make a racy test correct. `two_daemons_never_collide` needs a *specific adjacent
//! port*, and no lock can conjure one if the OS has handed it out — that test carries its own
//! retry. This bounds **contention**, not **assumptions**.

#![allow(dead_code)]

use std::fs::OpenOptions;
use std::path::PathBuf;

/// ~120 s at 100 ms per poll. Long enough for the slowest GPU test observed (33 s) several times
/// over; short enough that a leaked lock does not look like a hang.
const POLLS: u32 = 1_200;
const INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// Held for the duration of a test. Releasing is `Drop`, so an early `return` or a panic inside
/// the test still frees it — a lock released only on the happy path is a lock that wedges the
/// suite the first time an assertion fails.
pub struct Exclusive {
    path: PathBuf,
}

impl Drop for Exclusive {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Take exclusive use of a named machine resource until the returned guard drops.
///
/// ```ignore
/// let _gpu = common::exclusive::exclusive("gpu");
/// ```
///
/// **Bind it to a named variable.** `let _ = exclusive("gpu");` drops immediately and locks
/// nothing — the one mistake this API makes easy, so it is named here.
pub fn exclusive(resource: &str) -> Exclusive {
    let path = std::env::temp_dir().join(format!("marlowe-test-lock-{resource}"));

    for _ in 0..POLLS {
        // LOOP-EXEMPT: bounded polling for a file lock, not a driving loop.
        if OpenOptions::new().write(true).create_new(true).open(&path).is_ok() {
            return Exclusive { path };
        }
        std::thread::sleep(INTERVAL);
    }

    // **A leaked lock is the expected failure, not an exotic one.** Test processes are killed in
    // this project routinely — `cargo` being interrupted does not kill the binaries it spawned,
    // and one survived 2h46m this week. A lock whose owner is gone must not block the suite
    // forever, so the ceiling is a takeover rather than a hang.
    let _ = std::fs::remove_file(&path);
    let _ = OpenOptions::new().write(true).create_new(true).open(&path);
    Exclusive { path }
}
