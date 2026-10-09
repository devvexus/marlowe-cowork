//! Stage boundaries for the retrieval profile — **announced here, timed elsewhere.**
//!
//! ## Why this is a callback and not a stopwatch
//!
//! CONTRACTS.md §4.5 forbids reading a system clock on any path reachable from §4.1/4.6/4.7, and
//! `marlowe/tests/determinism_guard.rs` enforces that by name: `Instant::now` may appear in
//! `elapsed.rs` and `frame_clock.rs` and nowhere else. A per-stage profiler written the obvious
//! way — a `Stopwatch` per stage inside [`crate::retrieve::select_for_injection`] — would put a
//! real-clock read on the memory path and force a third entry into that allowlist. The guard
//! would still pass, and §4.5's structural guarantee would be gone.
//!
//! So the memory crate **announces boundaries and measures nothing**. A [`StageProbe`]
//! implementation lives behind the existing fence and is the only thing that reads a clock. The
//! shape is ADR-027's `WalkObserver`: a deterministic observation point, `()` in production.
//!
//! ## What a stage boundary means
//!
//! [`StageProbe::enter`] closes whatever stage was open and opens the named one;
//! [`StageProbe::finish`] closes the last. Every instant between the first `enter` and `finish`
//! therefore belongs to exactly one stage, which is what makes the totals reconcilable against
//! the span the wire reports. A breakdown whose parts do not sum to the whole is a breakdown
//! that can hide the cost it exists to find, so the residual is reported rather than absorbed —
//! see `crates/marlowe/src/profile.rs`.

/// One stage of §4.2's retrieval breakdown.
///
/// The names describe **what is inside the span**, not what a reader might assume from the
/// pipeline diagram. [`Stage::Gate`] covers scoring *and* the `ScoredCandidate` construction that
/// happens in the same pass, because the two are one `map` and splitting a single measured span
/// into two invented halves is the failure `adapter.rs` already refuses for `cues`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// §4.3's three exclusions, over the **whole store**. ADR-003's live-only hot index is what
    /// removes this scan; the candidate filter has stood in for the partition since Session A.
    Candidates,
    /// Session scoping — filtering the candidate set to the requesting session.
    Scope,
    /// BM25 over the scoped set, including the per-query re-tokenization of every candidate.
    Lexical,
    /// Cosine of the query vector against every scoped candidate's stored vector.
    Dense,
    /// `features::extract_all` — the set-level ranks, margins and z-scores.
    Features,
    /// The frozen gate's verdict per candidate, plus building the `ScoredCandidate` vector.
    Gate,
    /// Session derivation and top-N pruning.
    Prune,
    /// The cross-encoder, `RERANK_BUDGET` pairs.
    Rerank,
    /// Ordering, the token budget, the injected set, and the dump-order sort.
    Assemble,
}

impl Stage {
    /// Every stage, in pipeline order. The array is the authority for both the index mapping and
    /// the report's column order, so a new stage cannot be added to one and missed by the other.
    pub const ALL: [Stage; 9] = [
        Stage::Candidates,
        Stage::Scope,
        Stage::Lexical,
        Stage::Dense,
        Stage::Features,
        Stage::Gate,
        Stage::Prune,
        Stage::Rerank,
        Stage::Assemble,
    ];

    pub const COUNT: usize = Self::ALL.len();

    /// Position in [`Stage::ALL`]. Derived by search rather than by a hand-written `match`, so
    /// the two can never disagree.
    pub fn index(self) -> usize {
        Self::ALL
            .iter()
            .position(|s| *s == self)
            .expect("Stage::ALL lists every variant")
    }

    /// The name used in the profile's NDJSON rows and in the report's columns.
    pub fn name(self) -> &'static str {
        match self {
            Stage::Candidates => "candidates",
            Stage::Scope => "scope",
            Stage::Lexical => "lexical",
            Stage::Dense => "dense",
            Stage::Features => "features",
            Stage::Gate => "gate",
            Stage::Prune => "prune",
            Stage::Rerank => "rerank",
            Stage::Assemble => "assemble",
        }
    }
}

/// Receives stage boundaries. Implementations may time them; this crate never does.
pub trait StageProbe {
    /// Close the open stage, if any, and open `stage`.
    fn enter(&mut self, stage: Stage);
    /// Close the open stage. Called once, on the way out.
    fn finish(&mut self);
}

/// The production probe: nothing.
///
/// Monomorphized away, so the shipped path pays no branch and no call. This is what
/// [`crate::retrieve::select_for_injection`] passes.
impl StageProbe for () {
    #[inline]
    fn enter(&mut self, _: Stage) {}
    #[inline]
    fn finish(&mut self) {}
}

/// A probe that may or may not be installed, so **one call site serves both**.
///
/// This exists to avoid writing the `select_for_injection_probed` call twice — once profiled and
/// once not. Two copies of a nine-argument call is precisely the shape that drifts: a later change
/// to the profiled arm would leave the shipped arm behind, and every test would still pass because
/// both still compile and both still rank. One call site cannot drift from itself.
///
/// The cost when absent is a null check per stage boundary, nine per query, against a span
/// measured in hundreds of milliseconds.
impl<P: StageProbe> StageProbe for Option<&mut P> {
    #[inline]
    fn enter(&mut self, stage: Stage) {
        if let Some(probe) = self {
            probe.enter(stage);
        }
    }
    #[inline]
    fn finish(&mut self) {
        if let Some(probe) = self {
            probe.finish();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_stage_has_a_distinct_index_and_name() {
        // The index mapping is what a fixed-size accumulator array is addressed by, and the name
        // is what the report joins on. A duplicate in either would make two stages share a
        // column and read as one — a breakdown that silently loses a stage while still summing
        // to the total, which is the shape of failure this whole module is built to avoid.
        let mut indices: Vec<usize> = Stage::ALL.iter().map(|s| s.index()).collect();
        indices.sort_unstable();
        indices.dedup();
        assert_eq!(indices.len(), Stage::COUNT, "indices are not distinct");
        assert_eq!(indices, (0..Stage::COUNT).collect::<Vec<_>>(), "indices are not dense");

        let mut names: Vec<&str> = Stage::ALL.iter().map(|s| s.name()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), Stage::COUNT, "names are not distinct");
    }
}
