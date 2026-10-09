//! Bounded retry. **Bounded is the whole design; retrying is the easy half.**
//!
//! A benchmark is hundreds to thousands of calls and a hosted endpoint will rate-limit some of
//! them. Not retrying makes the instrument fragile; retrying without a bound is audit finding E8
//! in a new place — this project has already logged one unbounded retry loop, and the shape is
//! that it looks like resilience until the day it looks like a hang.
//!
//! So: a named constant for the attempt ceiling, a named constant for the backoff cap, and an
//! error that **states how many attempts were made** rather than reporting the last failure as
//! though it were the only one.

use std::time::Duration;

/// How many times one model call may be attempted, in total.
///
/// Four, not "until it works". Three retries covers a rate-limit window and a restarting upstream;
/// beyond that the endpoint is not having a bad second, it is having a bad day, and the run should
/// say so rather than absorb it.
pub const MAX_ATTEMPTS: u32 = 4;

/// The first backoff. Doubles per attempt.
pub const BASE_BACKOFF: Duration = Duration::from_millis(500);

/// The ceiling on any single wait, including one a `Retry-After` header asked for.
///
/// **A server-supplied wait is clamped, not obeyed.** `Retry-After: 3600` is a well-formed header
/// and honouring it turns one rate-limited call into an hour-long stall inside a turn that has a
/// wall-clock budget. Clamping keeps the header useful — it still orders the waits correctly —
/// without letting the far end set our deadline.
pub const MAX_BACKOFF: Duration = Duration::from_secs(20);

/// What to do about a status code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// 2xx.
    Proceed,
    /// Worth another attempt: 429 and the transient 5xx family.
    Retry,
    /// Will fail identically next time: 4xx that is not 429, and anything unrecognised.
    Fail,
}

/// **Keyed on the status, in one function, called by the driver.**
///
/// A second copy of this list inside the driver is how "we retry 429" becomes true of the comment
/// and false of the code.
pub fn disposition(status: u16) -> Disposition {
    match status {
        200..=299 => Disposition::Proceed,
        // Rate limited, and the one everyone means by "retry".
        429 => Disposition::Retry,
        // Transient upstream faults. 500 is included deliberately: OpenRouter returns it when a
        // chosen upstream drops a request mid-flight, which the next attempt routes around.
        500 | 502 | 503 | 504 => Disposition::Retry,
        // 400 malformed, 401 bad key, 402 out of credit, 403 moderation, 404 no such model, 413
        // too large. Every one of them is a property of the request, and the next attempt sends
        // the same request.
        _ => Disposition::Fail,
    }
}

/// How long to wait before attempt `attempt` (1-based: the wait *after* attempt 1 is `backoff(1)`).
///
/// `retry_after` is the server's own header, honoured when it parses as seconds and clamped by
/// [`MAX_BACKOFF`] either way.
pub fn backoff(attempt: u32, retry_after: Option<&str>) -> Duration {
    if let Some(secs) = retry_after.and_then(|v| v.trim().parse::<u64>().ok()) {
        return Duration::from_secs(secs).min(MAX_BACKOFF);
    }
    let exp = BASE_BACKOFF.saturating_mul(1u32 << (attempt.saturating_sub(1)).min(6));
    exp.min(MAX_BACKOFF)
}

/// **The ceiling on how long a turn can be stalled by retrying, in total.**
///
/// # Why this exists, and it is a mutation finding rather than a design flourish
///
/// The first version of `retrying_is_bounded_and_the_bound_is_the_named_constant` asserted
/// `transport_calls == MAX_ATTEMPTS`. Raising `MAX_ATTEMPTS` from 4 to **40** left it green: the
/// test read the constant, so the expectation moved with the thing it was supposed to be checking.
///
/// That is this project's most-logged shape — *assert the property you care about, not a proxy
/// that moves with it* — committed inside a test written to prevent audit finding E8. The proxy
/// was "the code obeys the constant". **The property is that a turn cannot be stalled
/// indefinitely**, and that is a statement about wall time, not about a count.
///
/// So the bound is asserted against a **literal** ceiling here. `MAX_ATTEMPTS = 40` produces
/// roughly thirteen minutes of sleeping and fails. So does raising [`MAX_BACKOFF`] to an hour.
/// A future session that genuinely wants more attempts has to raise this number too, which is one
/// line of diff and one line of review — exactly the deliberate act a bound should require.
pub const MAX_TOTAL_RETRY_WAIT: Duration = Duration::from_secs(60);

/// The longest this policy can sleep across one model call, if every attempt is rate-limited and
/// no `Retry-After` shortens the wait.
///
/// `MAX_ATTEMPTS` attempts means `MAX_ATTEMPTS - 1` waits between them.
pub fn worst_case_total_wait() -> Duration {
    (1..MAX_ATTEMPTS).map(|a| backoff(a, None)).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The assertion the count-based one could not make.**
    ///
    /// A test comparing the attempt count against `MAX_ATTEMPTS` is green for every value of
    /// `MAX_ATTEMPTS`, including forty. This one is not.
    #[test]
    fn retrying_cannot_stall_a_turn_for_longer_than_the_stated_ceiling() {
        let worst = worst_case_total_wait();
        assert!(
            worst <= MAX_TOTAL_RETRY_WAIT,
            "the retry policy can sleep for {worst:?}, over the {MAX_TOTAL_RETRY_WAIT:?} ceiling.              A benchmark is thousands of calls and this is per call; an unbounded-in-practice              retry loop is audit finding E8, and it looks like resilience until the day it looks              like a hang. Raise MAX_TOTAL_RETRY_WAIT deliberately, or lower MAX_ATTEMPTS /              MAX_BACKOFF."
        );

        // ...and the control: the ceiling must not be so generous that it would accept anything.
        // At the shipped values the worst case is a few seconds, two orders of magnitude inside
        // the ceiling — a ceiling that only just held would be a number chosen to fit.
        assert!(
            worst < MAX_TOTAL_RETRY_WAIT / 4,
            "the shipped policy is uncomfortably close to its own ceiling: {worst:?}"
        );
        assert!(
            !worst.is_zero(),
            "a policy that never waits is not a backoff, and this test would pass trivially"
        );
    }

    #[test]
    fn only_the_transient_statuses_are_retried() {
        assert_eq!(disposition(200), Disposition::Proceed);
        for s in [429, 500, 502, 503, 504] {
            assert_eq!(disposition(s), Disposition::Retry, "{s}");
        }
        // The ones where retrying spends money and time to receive the same answer.
        for s in [400, 401, 402, 403, 404, 413, 422] {
            assert_eq!(disposition(s), Disposition::Fail, "{s}");
        }
    }

    #[test]
    fn a_server_supplied_wait_is_clamped_rather_than_obeyed() {
        // `Retry-After: 3600` is well-formed. Obeying it stalls a turn for an hour inside a
        // wall-clock budget, so the far end does not get to set our deadline.
        // **Against a LITERAL, not against `MAX_BACKOFF`.** Comparing to the constant is green for
        // every value of it, including an hour — the same proxy that let `MAX_ATTEMPTS = 40`
        // through a test named for the bound. The property is that a far end cannot set our
        // deadline, and that is a statement about seconds.
        assert!(
            backoff(1, Some("3600")) <= Duration::from_secs(30),
            "a server-supplied Retry-After was honoured past any sane bound: {:?}",
            backoff(1, Some("3600"))
        );
        assert_eq!(backoff(1, Some("3600")), MAX_BACKOFF);
        assert_eq!(backoff(1, Some("2")), Duration::from_secs(2));
        // A date-formatted `Retry-After` is legal HTTP and is not parsed here; it falls through
        // to the exponential schedule rather than being read as zero.
        assert_eq!(backoff(1, Some("Wed, 21 Oct 2026 07:28:00 GMT")), BASE_BACKOFF);
    }

    #[test]
    fn the_backoff_is_exponential_and_capped() {
        assert_eq!(backoff(1, None), BASE_BACKOFF);
        assert_eq!(backoff(2, None), BASE_BACKOFF * 2);
        assert_eq!(backoff(3, None), BASE_BACKOFF * 4);
        assert!(backoff(30, None) <= MAX_BACKOFF, "the schedule must not run away");
    }
}
