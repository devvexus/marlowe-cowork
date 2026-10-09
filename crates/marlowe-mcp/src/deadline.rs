//! **The MCP request deadline. A declared fence, and a whole file holding one type.**
//!
//! # Why a real clock is read here
//!
//! CONTRACTS §4.5 forbids reading a system clock on any path reachable from the §4 interfaces, and
//! `determinism_guard::the_only_real_clock_read_is_the_latency_fence` enforces it across the
//! workspace. It caught this file's contents on the first run after they were written, in
//! `lib.rs`, which is the guard working.
//!
//! What is needed here is genuinely a clock: an MCP server is a child process, and a child that
//! never answers would otherwise block the turn **forever**, with no output and no indication why.
//! That is the failure Addendum B §B5's *motion means Marlowe is working* exists to prevent, and it
//! is not solvable by counting iterations — a blocking `read_line` on a silent pipe does not
//! iterate.
//!
//! # Why it cannot leak, which is the only thing that makes it legitimate
//!
//! `marlowe/src/elapsed.rs`'s reconciliation applies verbatim: **a monotonic elapsed duration is
//! not a timestamp.** [`Deadline`] names no point in time, hands out no number at all, and has
//! exactly one method returning `bool`. There is no accessor, no `Duration`, no conversion, and
//! nothing downstream can key off it. Decay, activation, `silent_until` maturation and supersession
//! recency all read a supplied clock and are structurally unable to reach this module — this crate
//! does not depend on `marlowe-memory` and cannot.
//!
//! # Why it is its OWN FILE
//!
//! `marlowe-daemon/src/clock.rs` states the rule and this follows it: *"a whole file containing one
//! struct so the fence is narrow: exempting a 400-line file would have blinded the guard to every
//! future clock read in it, which is the failure its own header warns about for directories."*
//!
//! `lib.rs` is four hundred lines of protocol handling and is exactly the file that should keep
//! failing the guard if a second clock read appears in it.

use std::time::{Duration, Instant};

/// A bound on how long one request may wait. **Nothing but `bool` comes out.**
pub struct Deadline(Instant);

impl Deadline {
    pub fn after(ms: u64) -> Self {
        Self(Instant::now() + Duration::from_millis(ms))
    }

    /// Whether the bound has been reached.
    ///
    /// The only method, and it returns no quantity — see the header. A `remaining()` returning a
    /// `Duration` would be the first step toward a number something else could read.
    pub fn passed(&self) -> bool {
        Instant::now() > self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_deadline_in_the_past_has_passed_and_one_in_the_future_has_not() {
        assert!(Deadline::after(0).passed());
        assert!(!Deadline::after(60_000).passed());
    }

    /// **The fence's own property, asserted here as well as in the guard.**
    ///
    /// If this type ever grows a way to hand out a quantity, the argument in the header stops
    /// holding — a duration that can be read is a duration something can key off.
    #[test]
    fn nothing_but_a_bool_comes_out_of_this_module() {
        let src = include_str!("deadline.rs");
        let body = src.split("#[cfg(test)]").next().expect("there is a non-test half");
        let returns: Vec<&str> = body
            .lines()
            .filter(|l| l.trim_start().starts_with("pub fn"))
            .collect();
        assert_eq!(returns.len(), 2, "two public functions: `after` and `passed`");
        assert!(
            returns.iter().any(|l| l.contains("-> bool")),
            "`passed` must return a bool"
        );
        assert!(
            !body.contains("-> Duration")
                && !body.contains("-> u64")
                && !body.contains("-> u128")
                && !body.contains("-> Instant"),
            "this module handed out a quantity. §4.5's fence holds only because a monotonic \
             elapsed duration is not a timestamp AND nothing can read one from here"
        );
    }
}
