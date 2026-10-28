//! The machine's load while a project runs: what the project's meters show.
//!
//! The engine asks a [`MachineProbe`] every [`EngineConfig::probe_every`](super::EngineConfig) and
//! writes the answer into `metrics` through the state actor, so a client sees `cpu_pct`,
//! `gpu_pct` and `vram_mb` move as coalesced patches. Nothing is written when the rounded numbers
//! did not change, and the meters go back to zero when the engine stops (parked, finished,
//! cancelled): a card that waits for the user is not using the machine.

use crate::mutation::Mutation;

/// One reading of the machine.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Machine {
    /// CPU in use, percent of the whole machine.
    pub cpu_pct: f64,
    /// GPU in use, percent.
    pub gpu_pct: f64,
    /// GPU memory in use, MB.
    pub vram_mb: u64,
}

/// Where the readings come from. The daemon's reads the Sentinel's GPU state and this process's
/// processor time; tests pass a script.
pub trait MachineProbe: Send + Sync {
    /// A reading now. Must not block for long.
    fn sample(&self) -> Machine;
}

/// A reading as it is stored: whole percents and megabytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Rounded {
    cpu: u8,
    gpu: u8,
    vram_mb: u64,
}

impl Rounded {
    pub(super) fn of(m: Machine) -> Rounded {
        let pct = |v: f64| if v.is_finite() { v.round().clamp(0.0, 100.0) as u8 } else { 0 };
        Rounded { cpu: pct(m.cpu_pct), gpu: pct(m.gpu_pct), vram_mb: m.vram_mb }
    }

    pub(super) fn mutation(self) -> Mutation {
        Mutation::SetMachine { cpu_pct: f64::from(self.cpu), gpu_pct: f64::from(self.gpu), vram_mb: self.vram_mb }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readings_are_stored_as_whole_numbers_in_range() {
        let r = Rounded::of(Machine { cpu_pct: 12.5, gpu_pct: 143.0, vram_mb: 9_210 });
        assert_eq!(r, Rounded { cpu: 13, gpu: 100, vram_mb: 9_210 });
        assert_eq!(Rounded::of(Machine { cpu_pct: f64::NAN, gpu_pct: -4.0, vram_mb: 0 }), Rounded::default());
        assert_eq!(Rounded::of(Machine { cpu_pct: 12.4, ..Machine::default() }), Rounded::of(Machine { cpu_pct: 11.6, ..Machine::default() }), "jitter inside a percent writes nothing");
    }
}
