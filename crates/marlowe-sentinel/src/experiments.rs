//! The experiments bridge: metrics, plateau notices, deadlines (design 3.9), raised as
//! `NoticeSource::Experiment`. Track J2; idle until it lands. Replace [`probe`]'s body; the engine
//! already ticks it (`crate::probe::PROBES`). The daemon side (the experiments table, deadline
//! re-arm on boot) is `marlowe-daemon/src/exp_ops.rs`.

use crate::probe::{Idle, Probe, ProbeContext};

/// Starts the experiments probe.
pub fn probe(_cx: &ProbeContext<'_>) -> Box<dyn Probe> {
    Box::new(Idle("experiments"))
}
