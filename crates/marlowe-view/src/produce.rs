//! The producer contract. **What a surface may ask of whatever is behind it.**
//!
//! `marlowe-surface` contains two drivers — the TUI's key dispatcher and §B11's classic REPL — and
//! both need to hand an [`Intent`] somewhere and get a fresh [`SessionView`] back. Giving them a
//! concrete producer would have put `marlowe-stub` or `marlowe-daemon` back into the surface's
//! dependencies and undone C2d.
//!
//! So they take a `&mut impl Produce`. The surface can **use** a producer and cannot **construct**
//! one: there is no `Produce::new`, and a trait with no constructor cannot be conjured from
//! nothing. That is the same property the M1 header claimed and did not have, expressed in a way
//! the compiler checks.
//!
//! # Every refusal is named
//!
//! [`Produce::apply`] returns `Result<_, IntentError>` rather than swallowing what it cannot do.
//! A producer that silently ignored an intent would make a working surface and a broken one look
//! identical — the failure this project has recorded more times than any other. The scripted stub
//! accepts `ForceState`; a daemon driving a real run refuses it by name.

use crate::view::{Intent, IntentError, SessionView};

/// Anything that can publish a [`SessionView`] and act on an [`Intent`].
pub trait Produce {
    /// The current view. Borrowed, never handed over — a surface takes its own snapshot.
    fn view(&self) -> &SessionView;

    /// Act on a request, or say why not.
    fn apply(&mut self, intent: Intent) -> Result<(), IntentError>;

    /// Advance to `now_ms`. Idempotent for a given time; safe to call every frame.
    fn tick(&mut self, now_ms: u64);

    /// Whether work is outstanding. §B11's REPL drains on this.
    fn is_busy(&self) -> bool {
        false
    }

    /// Milliseconds until something will change without user input, for the poll timeout. `None`
    /// means the loop may block — a terminal doing nothing should cost nothing.
    fn next_beat_in(&self, _now_ms: u64) -> Option<u64> {
        None
    }

    /// Do any deferred connection work. **Called after the first frame is on screen**, never
    /// before: §6 says the header paints before the daemon connection resolves, and a producer
    /// that did its handshake in its constructor would put a network round-trip in front of the
    /// first paint. The scripted producer has nothing to do here.
    fn connect_now(&mut self) {}
}

/// A monotonic millisecond time base, supplied to a surface rather than read by one.
///
/// **The surface has no exemption in `determinism_guard.rs` and must not gain one.** Every render
/// is a pure function of `(state, now_ms)`, which is what makes §B13's flicker rows diffable; a
/// widget calling `Instant::now()` would take that away. So time arrives through this trait, and
/// the only implementations live behind the two fenced files the guard names.
pub trait ClockRead {
    fn now_ms(&self) -> u64;
}
