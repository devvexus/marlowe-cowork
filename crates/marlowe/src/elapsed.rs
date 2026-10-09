//! **The only place in the system that reads a real clock.**
//!
//! CONTRACTS.md section 4.5 is binding: *"on any path reachable from sections 4.1, 4.6, or
//! 4.7, the implementation MUST NOT read a system clock. Every timestamp is derived from the
//! `clock` supplied by the caller."* Staleness half-life is unmeasurable and every
//! decay-dependent result irreproducible if that is violated.
//!
//! And yet section 4.2 requires `cost.latency_ms`, self-reported by the implementation. So
//! exactly one real-clock read is legitimate, and the two rules coexist only if that read
//! cannot influence anything else.
//!
//! The reconciliation, stated precisely: **a monotonic elapsed duration is not a timestamp.**
//! It cannot be converted into one, it names no point in time, and nothing in memory can key
//! off it. Decay, activation, `silent_until` maturation and supersession recency all read the
//! supplied clock and are structurally unable to reach this module, because the only thing it
//! hands out is an `ElapsedMs` that goes straight into a cost block.
//!
//! Two mechanisms hold that:
//!
//! 1. [`ElapsedMs`] has no arithmetic with timestamps, no `From<ElapsedMs> for i64` that
//!    reads as a time, and one accessor whose name says where it belongs.
//! 2. A workspace test (`determinism_guard.rs`) bans `Instant`, `SystemTime` and `UNIX_EPOCH`
//!    everywhere except this file, by name. A second real-clock read cannot appear without
//!    appearing in that allowlist.
//!
//! The harness agrees this is the boundary: `canonical.py` excludes `latency_ms` from the
//! reproduction hash by a one-key allowlist, precisely because it is self-reported and varies
//! by nature. Everything else must match byte for byte.

use std::time::Instant;

use marlowe_memory::probe::{Stage, StageProbe};

/// A duration, in milliseconds, that may only be reported as cost.
///
/// Deliberately not `i64` and deliberately not convertible to one except through
/// [`ElapsedMs::as_cost_ms`], whose name is the documentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElapsedMs(u128);

impl ElapsedMs {
    /// The only way out. Named for its single legitimate destination.
    pub fn as_cost_ms(self) -> i64 {
        // Saturating rather than wrapping: a nonsense latency should read as implausibly
        // large, never as a small plausible one.
        i64::try_from(self.0).unwrap_or(i64::MAX)
    }
}

/// Times an operation, and hands back only a duration.
///
/// ```ignore
/// let stopwatch = Stopwatch::start();
/// let result = do_the_work(clock);          // `clock` is the caller's, not ours
/// let cost = stopwatch.stop().as_cost_ms(); // the one real-clock read
/// ```
pub struct Stopwatch(Instant);

impl Stopwatch {
    pub fn start() -> Self {
        Self(Instant::now())
    }

    pub fn stop(self) -> ElapsedMs {
        ElapsedMs(self.0.elapsed().as_millis())
    }

    /// The same span at microsecond resolution, without consuming the stopwatch.
    ///
    /// For the retrieval profile only. `stop` stays the single source of `cost.latency_ms` — the
    /// wire's number must not become a rounded copy of this one, or a profiling change could move
    /// a published latency without touching the retrieval path at all.
    pub fn elapsed_us(&self) -> ElapsedUs {
        ElapsedUs(self.0.elapsed().as_micros())
    }
}

/// A duration in **microseconds**, for the retrieval stage profile.
///
/// Separate from [`ElapsedMs`] and deliberately not convertible to it. Millisecond resolution is
/// the contract's unit for `cost.latency_ms` and it is far too coarse for a breakdown whose
/// smaller stages are tens of microseconds: rounded to milliseconds, six of the nine stages read
/// zero and the profile would report the rerank as *the whole cost* whatever the truth was. That
/// is a measurement that agrees with the expected answer by construction.
///
/// It carries the same anti-timestamp property as `ElapsedMs`, for the same reason: one accessor,
/// named for its only destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ElapsedUs(u128);

impl ElapsedUs {
    pub fn as_profile_us(self) -> u64 {
        u64::try_from(self.0).unwrap_or(u64::MAX)
    }
}

/// Times [`Stage`] boundaries announced by `marlowe-memory`.
///
/// **The whole point of this type living here** is that `marlowe-memory` must not read a clock:
/// §4.5 forbids it on every path reachable from §4.1/4.6/4.7 and `determinism_guard.rs` enforces
/// it by file name. The memory crate announces boundaries; this — inside the existing fence — is
/// what turns them into durations. See `marlowe_memory::probe`.
///
/// `enter` closes the open stage and opens the named one, so every instant between the first
/// `enter` and `finish` belongs to exactly one stage. That is the property the profile's residual
/// check reads: stages that do not sum to the measured span mean a boundary is missing, and a
/// breakdown with a hole in it can hide the cost it was built to find.
pub struct StageTimer {
    span: Instant,
    open: Option<(Stage, Instant)>,
    totals: [u128; Stage::COUNT],
}

impl StageTimer {
    pub fn start() -> Self {
        Self {
            span: Instant::now(),
            open: None,
            totals: [0; Stage::COUNT],
        }
    }

    /// Per-stage totals, in [`Stage::ALL`] order.
    pub fn totals(&self) -> [ElapsedUs; Stage::COUNT] {
        let mut out = [ElapsedUs(0); Stage::COUNT];
        for (slot, total) in out.iter_mut().zip(self.totals.iter()) {
            *slot = ElapsedUs(*total);
        }
        out
    }

    /// The whole span this timer has been open for — start to now, stages and gaps alike.
    ///
    /// Read *after* `finish`, it is the denominator the stage totals are reconciled against.
    pub fn span(&self) -> ElapsedUs {
        ElapsedUs(self.span.elapsed().as_micros())
    }
}

impl StageProbe for StageTimer {
    fn enter(&mut self, stage: Stage) {
        let now = Instant::now();
        if let Some((previous, started)) = self.open {
            self.totals[previous.index()] += now.duration_since(started).as_micros();
        }
        self.open = Some((stage, now));
    }

    fn finish(&mut self) {
        if let Some((previous, started)) = self.open.take() {
            self.totals[previous.index()] += started.elapsed().as_micros();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elapsed_is_reportable_as_cost() {
        let sw = Stopwatch::start();
        let ms = sw.stop().as_cost_ms();
        assert!(ms >= 0, "a duration is never negative");
    }

    #[test]
    fn elapsed_cannot_masquerade_as_a_timestamp() {
        // Structural, and asserted so it is a stated property rather than an accident: a
        // freshly measured elapsed duration is small, where any real epoch-millis timestamp
        // is enormous. If ElapsedMs ever gained a conversion that produced wall-clock time,
        // this is the assertion that would start failing.
        let ms = Stopwatch::start().stop().as_cost_ms();
        assert!(
            ms < 1_000_000,
            "an elapsed duration must not be a point in time; got {ms}"
        );
    }

    #[test]
    fn stage_totals_never_exceed_the_span_they_were_taken_inside() {
        // The profile's whole claim is that the parts add up to the whole. If they could sum to
        // MORE than the span, a stage would be double-counted and the biggest number in the
        // table would be an artifact of the instrument. Asserted rather than assumed, because
        // the failure looks exactly like a real finding.
        let mut timer = StageTimer::start();
        for stage in Stage::ALL {
            timer.enter(stage);
            std::hint::black_box((0..500).sum::<u64>());
        }
        timer.finish();

        let sum: u64 = timer.totals().iter().map(|t| t.as_profile_us()).sum();
        assert!(
            sum <= timer.span().as_profile_us(),
            "stages sum to {sum} us inside a {} us span",
            timer.span().as_profile_us()
        );
    }

    /// Spin until this timer's clock has demonstrably advanced by at least one microsecond.
    ///
    /// **This replaces `black_box((0..20_000).sum())`, and the replacement is the point.** That
    /// idiom times a *workload*, and in `--release` the optimizer folds the sum to a constant, so
    /// the span rounds to zero microseconds and every assertion of the form "the total is > 0"
    /// fails — deterministically, five runs out of five, on code that is working perfectly. The
    /// test was then a statement about the build profile rather than about `StageTimer`: it read
    /// the same whether or not the property it names held, in one direction in debug and the other
    /// in release.
    ///
    /// Spinning on the clock removes the workload from the assertion entirely. It returns as soon
    /// as `Instant` reports a non-zero microsecond count — on Windows that is one QPC tick, well
    /// under a microsecond of real spinning — and it cannot be optimized away, because the value it
    /// observes comes from outside the program. What remains asserted is what was always meant:
    /// **`finish` banks an elapsed span that was already on the clock.**
    fn spin_until_the_clock_moves() {
        spin_past_micros(0);
    }

    /// Spin until at least `micros` microseconds have elapsed on this timer's clock.
    ///
    /// The parameterised form exists because one test needs a *long* visit and a *short* one, and
    /// the difference between them has to survive the scheduler: any assertion comparing two
    /// measured spans is really comparing "how long the work took plus how long the OS descheduled
    /// us for", and a one-microsecond margin loses that comparison the first time a parallel test
    /// binary gets the core.
    fn spin_past_micros(micros: u128) {
        let start = Instant::now();
        while start.elapsed().as_micros() <= micros {
            std::hint::spin_loop();
        }
    }

    /// A first visit long enough that a *restart* could not be mistaken for an accumulation.
    ///
    /// 500 us, and the number is doing one job: if re-entry reset the total, the second (short)
    /// visit would have to out-measure this to keep the assertion green, which takes a
    /// half-millisecond stall in a spin loop.
    const LONG_VISIT_US: u128 = 500;

    #[test]
    fn a_stage_entered_twice_accumulates_rather_than_restarts() {
        // `Rerank` is entered once today, but a stage that reset on re-entry would report only
        // its last visit — a plausible, smaller number with nothing marking the loss.
        //
        // **The assertion is Rerank against ITSELF, one visit later, and that is a correction.**
        // Comparing Rerank to an empty `Assemble` measured the scheduler: `Assemble` spans two
        // adjacent statements, so a deschedule between them makes it arbitrarily large and the
        // test fails on a busy machine while the code is perfect. It also read `0 >= 0` in
        // release, green and vacuous. Reading the same stage before and after its second visit
        // removes both problems — accumulation must make it strictly larger, and a restart makes
        // it smaller, whatever else the machine is doing.
        // **THREE visits, because the total is banked in TWO places and a test that exercises
        // one says nothing about the other.** A visit ended by `enter` is banked by `enter`; the
        // last visit is banked by `finish`. Mutating `enter`'s `+=` to `=` left an earlier version
        // of this test green, because its second visit happened to be closed by `finish`. Visits 1
        // and 2 close through `enter`, visit 3 closes through `finish`, and the total must rise
        // strictly at every step.
        let mut timer = StageTimer::start();
        let rerank = |t: &StageTimer| t.totals()[Stage::Rerank.index()].as_profile_us();

        timer.enter(Stage::Rerank);
        spin_past_micros(LONG_VISIT_US);
        timer.enter(Stage::Assemble); // banks visit 1, through `enter`
        let after_first = rerank(&timer);
        assert!(
            after_first >= LONG_VISIT_US as u64,
            "the first visit spanned at least {LONG_VISIT_US} us of clock but banked              {after_first} us"
        );

        timer.enter(Stage::Rerank);
        spin_until_the_clock_moves();
        timer.enter(Stage::Assemble); // banks visit 2, through `enter`
        let after_second = rerank(&timer);
        assert!(
            after_second > after_first,
            "a stage entered twice must ACCUMULATE through `enter`: Rerank read {after_first} us              after one visit and {after_second} us after two. Equal means the visit was dropped;              smaller means re-entry restarted the total and the long first visit was lost with              nothing marking it."
        );

        timer.enter(Stage::Rerank);
        spin_until_the_clock_moves();
        timer.finish(); // banks visit 3, through `finish`
        let after_third = rerank(&timer);
        assert!(
            after_third > after_second,
            "and it must accumulate through `finish` too: Rerank read {after_second} us after two              visits and {after_third} us after three"
        );
    }

    #[test]
    fn finish_is_what_closes_the_last_stage() {
        // Without `finish`, the last stage — the rerank, in the shipped order — contributes
        // nothing and the residual absorbs it. The residual is reported, so this would surface;
        // it is asserted here so it surfaces as a named test instead of as a puzzling table.
        //
        // **Two things are asserted, because the name has two halves and only one of them is a
        // duration.** `open` going to `None` is *the stage closed* — structural, resolution-free,
        // and true in every build profile. The banked total is *what closing it was for*; a
        // `finish` that took the stage without accumulating would satisfy the first and is exactly
        // the mutation this file has to catch.
        let mut timer = StageTimer::start();
        timer.enter(Stage::Rerank);
        spin_until_the_clock_moves();
        assert!(timer.open.is_some(), "the stage is open until finish closes it");
        assert_eq!(
            timer.totals()[Stage::Rerank.index()].as_profile_us(),
            0,
            "an open stage has contributed nothing yet"
        );

        timer.finish();

        assert!(timer.open.is_none(), "finish closes the open stage");
        assert!(
            timer.totals()[Stage::Rerank.index()].as_profile_us() > 0,
            "and banks the span it had accumulated -- the clock moved before finish was called,              so a zero total means the stage was dropped rather than closed"
        );
    }

    #[test]
    fn finish_on_a_timer_with_nothing_open_banks_nothing() {
        // The other half of `open.take()`, and the reason the assertion above is not enough on its
        // own: a `finish` that closed the stage correctly and then double-counted it on a second
        // call would still pass every assertion in this file. Idempotence is the property.
        let mut timer = StageTimer::start();
        timer.enter(Stage::Rerank);
        spin_until_the_clock_moves();
        timer.finish();
        let once = timer.totals()[Stage::Rerank.index()].as_profile_us();
        spin_until_the_clock_moves();
        timer.finish();
        let twice = timer.totals()[Stage::Rerank.index()].as_profile_us();
        assert_eq!(once, twice, "a second finish must have nothing left to bank");
    }
}
