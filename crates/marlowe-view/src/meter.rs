//! The meter's shape, and **whether a source is attached at all**.
//!
//! ADR-021: the widget invents nothing and renders what its source reports. That rule needs the
//! *shape* of a reading to live somewhere both a producer and a surface can see, without either
//! being able to reach the other's half. This module is that shape. The envelope that generates a
//! scripted reading is a **producer** and deliberately does not live here — see
//! `marlowe-stub::amplitude`.
//!
//! # `MeterSource::None` is not a frame of zeroes
//!
//! [`BASELINE`] means *live, and flat* — a reading. [`MeterSource::None`] means *no source is
//! attached*, and the meter holds whatever it last drew (ADR-021's freeze). §B5 means two
//! different things by a still meter and a frozen one, and the user can tell them apart, so the
//! type does too.
//!
//! Collapsing the two would be the cheapest possible way to make a daemon with no telemetry look
//! like a daemon reporting silence, with nothing on screen saying which.

/// Horizontal samples. Six cells wide × 2 braille dot-columns per cell.
pub const SAMPLES: usize = 12;

/// Vertical levels. Two rows × 4 braille dot-rows per row.
pub const LEVELS: u8 = 8;

/// One rendered frame of the meter: a level per column, each `0..=LEVELS`.
pub type Frame = [u8; SAMPLES];

/// A frame of all zeroes — the flat baseline `idle` reports. **A reading, not an absence.**
pub const BASELINE: Frame = [0; SAMPLES];

/// What a producer reports for the meter this frame.
///
/// The distinction is load-bearing and is the reason this is not simply a `Frame`: a producer that
/// has no amplitude telemetry must be able to say so. A daemon that reported [`BASELINE`] when it
/// meant "I am not measuring anything" would render as a live, silent session — which is a claim
/// about the world, made by a component that has no way to know it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeterSource {
    /// A source measured this. The meter draws it.
    Reported(Frame),
    /// No source is attached. The meter **holds its last frame** — ADR-021's freeze, arriving as
    /// an absence of data rather than as a branch inside the widget.
    None,
}

impl MeterSource {
    /// The frame to draw, given what was last drawn. `None` holds.
    ///
    /// Taking the previous frame as an argument is what keeps the freeze in the data rather than
    /// in the widget: there is no "is this the waiting state" test anywhere in the meter.
    pub fn resolve(self, last: Frame) -> Frame {
        match self {
            MeterSource::Reported(f) => f,
            MeterSource::None => last,
        }
    }

    pub fn is_reporting(self) -> bool {
        matches!(self, MeterSource::Reported(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absent_source_holds_the_last_frame_rather_than_flattening_it() {
        // The failure this prevents: a producer with no telemetry rendering as a live silent
        // session. `BASELINE` is a measurement of silence; `None` is the absence of a measurement,
        // and §B5 shows the user different things for the two.
        let busy: Frame = [4; SAMPLES];
        assert_eq!(MeterSource::None.resolve(busy), busy);
        assert_eq!(MeterSource::Reported(BASELINE).resolve(busy), BASELINE);
    }

    #[test]
    fn a_flat_reading_and_no_reading_are_not_the_same_value() {
        assert_ne!(MeterSource::Reported(BASELINE), MeterSource::None);
        assert!(MeterSource::Reported(BASELINE).is_reporting());
        assert!(!MeterSource::None.is_reporting());
    }
}
