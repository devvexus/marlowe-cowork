//! CI polling (`gh run list` for pushed branches) and `NoticeSource::Ci` (design 3.9). Track J;
//! idle until it lands. Replace [`probe`]'s body; the engine already ticks it
//! (`crate::probe::PROBES`).

use crate::probe::{Idle, Probe, ProbeContext};

/// Starts the CI probe.
pub fn probe(_cx: &ProbeContext<'_>) -> Box<dyn Probe> {
    Box::new(Idle("ci"))
}
