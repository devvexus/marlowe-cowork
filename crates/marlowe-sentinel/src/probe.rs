//! Probes: watchers that are not file events, ticked on every poll by the engine thread, so a
//! track adds one without editing the engine or `lib.rs`.
//!
//! | Probe | Owner | What it raises |
//! |---|---|---|
//! | [`crate::ci::probe`] | J | `NoticeSource::Ci`: CI results for pushed branches (`gh run list`) |
//! | [`crate::experiments::probe`] | J2 | `NoticeSource::Experiment`: metrics, plateaus, deadlines |
//!
//! A probe's constructor lives in its owner's file and is listed in [`PROBES`]; both start as
//! [`Idle`]. Probes stay read-only like every other watcher.

use std::path::{Path, PathBuf};

use crate::notice::NoticeDraft;

/// What a probe is started with.
#[derive(Debug, Clone, Copy)]
pub struct ProbeContext<'a> {
    /// The profile home (`cowork.db` lives here).
    pub home: &'a Path,
}

/// One probe. Lives on the engine thread.
pub trait Probe: Send {
    fn name(&self) -> &str;

    /// One poll tick, with the current watch roots. Must not block: slow work (a `gh` child, a
    /// metrics read) runs on the probe's own thread, and `tick` returns what has finished since.
    /// Each draft is recorded and deduplicated like any other notice.
    fn tick(&mut self, now_ms: i64, roots: &[PathBuf]) -> Vec<NoticeDraft>;
}

/// Starts a probe.
pub type ProbeCtor = fn(&ProbeContext<'_>) -> Box<dyn Probe>;

/// Every registered probe, in tick order.
pub const PROBES: &[ProbeCtor] = &[crate::ci::probe, crate::experiments::probe];

/// A probe that raises nothing (a track's stand-in until it lands).
#[derive(Debug, Clone, Copy)]
pub struct Idle(pub &'static str);

impl Probe for Idle {
    fn name(&self) -> &str {
        self.0
    }

    fn tick(&mut self, _now_ms: i64, _roots: &[PathBuf]) -> Vec<NoticeDraft> {
        Vec::new()
    }
}

pub(crate) fn start_all(cx: &ProbeContext<'_>) -> Vec<Box<dyn Probe>> {
    PROBES.iter().map(|start| start(cx)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_registered_probe_starts_idle() {
        let mut probes = start_all(&ProbeContext { home: Path::new("C:/home") });
        let names: Vec<String> = probes.iter().map(|p| p.name().to_string()).collect();
        assert_eq!(names, ["ci", "experiments"]);
        for p in &mut probes {
            assert!(p.tick(0, &[PathBuf::from("C:/runs")]).is_empty());
        }
    }
}
