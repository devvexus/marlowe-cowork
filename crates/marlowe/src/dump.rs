//! The gate-feature dump — the side channel `tools/fit_gate.py` reads.
//!
//! **A side file, never the wire.** The obvious alternative was to widen the §4.2 response
//! with the scores of everything considered; that would have been a fourth channel between
//! harness and implementation, which is precisely what the M0a/M0b split exists to prevent.
//! The dump is written under an explicit flag, read by one script, and the retrieval response
//! is byte-identical with or without it.
//!
//! One line per **scored candidate**, not per injected memory. The fit needs the negatives:
//! a calibration fit only on what was injected would be fit on its own output.
//!
//! Nothing derived from the answer key is written here. The join to gold happens in the
//! fitter, from §4.6's `written[].turn_id` mapping — the same join the harness itself uses.
//!
//! **In gated mode the dump also carries this build's own `score`, `margin`,
//! `calibrated_precision`, `winning_cue` and `passes`.** That is not a convenience. When the
//! frozen gate abstains on every query — the outcome since Session B — the §4.2 response carries
//! no `injected` entries at all, so a precision/coverage curve read from the wire would be a curve
//! over an empty set. The alternative was to re-apply the artifact's curves in Python, which
//! would put a second implementation of the gate's arithmetic beside the real one with nothing
//! comparing them. Reading the implementation's own numbers has neither problem.
//!
//! **`score` and `margin` are the v4 ranking key, in order**, so the driver can reproduce the
//! gate's own top-1 — the number the floor condition is judged on — rather than guessing an
//! ordering. Session D carried `min_calibrated_precision` for the same reason; it is gone because
//! the v4 key does not contain it, and leaving a dead ranking field beside a live one is the
//! two-implementations-one-checked pattern this project keeps paying for.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::Path;

use marlowe_memory::consolidate::ConsolidationReport;
use marlowe_memory::retrieve::ScoredCandidate;

pub struct FeatureDump {
    out: BufWriter<File>,
}

impl FeatureDump {
    pub fn create(path: &Path) -> std::io::Result<Self> {
        // Truncating, not appending. An append would silently merge two runs into one fit set,
        // and the resulting artifact would be reproducible from neither.
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        Ok(Self {
            out: BufWriter::new(file),
        })
    }

    /// `gated` distinguishes the two modes. Under `--fit-mode` no gate is loaded, so the
    /// verdict fields are **omitted rather than written as zeros** — the same rule §4.2 applies
    /// to absent latency stages, and for the same reason: a zero reads as a measurement.
    pub fn write(
        &mut self,
        query_id: &str,
        candidates: &[ScoredCandidate<'_>],
        gated: bool,
    ) -> std::io::Result<()> {
        for candidate in candidates {
            let mut row = serde_json::Map::new();
            row.insert("query_id".into(), query_id.into());
            row.insert("memory_id".into(), candidate.entry.id.clone().into());
            // By name, not by position. A reorder of `FEATURE_NAMES` must not silently
            // transpose the fit -- the fitter reads these keys and asserts the set.
            for (name, value) in candidate.features.named() {
                row.insert(name.into(), serde_json::json!(value));
            }
            if gated {
                // The ranking key, in order. Without both levels the driver cannot reproduce the
                // gate's own ordering, and reproducing it in Python would put a second
                // implementation of the ranking beside the real one with nothing comparing them.
                row.insert("score".into(), serde_json::json!(candidate.score));
                row.insert("margin".into(), serde_json::json!(candidate.margin));
                row.insert(
                    "calibrated_precision".into(),
                    serde_json::json!(candidate.calibrated_precision),
                );
                row.insert("winning_cue".into(), candidate.winning_cue.into());
                row.insert("passes".into(), serde_json::json!(candidate.passes));

                // Session H's two new ranking levels, written for the same reason `score` and
                // `margin` are: without them the driver cannot reproduce the shipped ordering,
                // and reproducing it in Python would put a second implementation of the ranking
                // beside the real one with nothing comparing the two.
                row.insert("session_key".into(), serde_json::json!(candidate.session_key));
                row.insert(
                    "survived_pruning".into(),
                    serde_json::json!(candidate.survived_pruning),
                );
                // **`null` when the candidate was not reranked, never 0.0.** A cross-encoder logit
                // is signed and near-zero is a real, middling score, so a zero sentinel would be
                // indistinguishable from a genuine reading — and `analyze_cue_overlap.py` would
                // rank an unreranked candidate above every genuinely negative one.
                row.insert(
                    "rerank_score".into(),
                    match candidate.rerank_score {
                        Some(v) => serde_json::json!(v),
                        None => serde_json::Value::Null,
                    },
                );
                // **The cascade's fusion rank, `null` when absent** — same named-absence rule as
                // `rerank_score` directly above. Without this column a driver cannot reproduce
                // the cascade's final ordering from the dump (the fusion is defined over RANKS),
                // and reconstructing it in Python would put a second implementation of the
                // ranking beside the real one with nothing comparing them.
                row.insert(
                    "fusion_rank".into(),
                    match candidate.fusion_rank {
                        Some(v) => serde_json::json!(v),
                        None => serde_json::Value::Null,
                    },
                );
            }
            writeln!(self.out, "{}", serde_json::Value::Object(row))?;
        }
        Ok(())
    }

    pub fn flush(&mut self) -> std::io::Result<()> {
        self.out.flush()
    }
}

/// The consolidation dump — one NDJSON row per **ingested session**.
///
/// Same discipline as [`FeatureDump`] and for the same reason: a diagnostic side channel under an
/// explicit flag, never a fourth channel between harness and implementation. The §4.6 response is
/// byte-identical with or without it.
///
/// What it carries depends on the policy, and the two must not be confusable:
///
/// * a **dry run** writes the full similarity histogram plus the session clustered at every
///   threshold in `SWEEP_THRESHOLDS`. This is the evidence the frozen threshold is chosen from,
///   and it is produced by a pass that applied nothing.
/// * an **applied** run writes the one clustering it actually performed, with the threshold it
///   read from the frozen artifact.
///
/// `policy` is written on every row, so a sweep row can never be mistaken for a row describing
/// what a run really merged.
pub struct ConsolidationDump {
    out: BufWriter<File>,
}

impl ConsolidationDump {
    pub fn create(path: &Path) -> std::io::Result<Self> {
        // Truncating, not appending — see `FeatureDump::create`. An append would merge two runs'
        // sweeps into one file and the threshold chosen from it would be reproducible from
        // neither.
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        Ok(Self {
            out: BufWriter::new(file),
        })
    }

    /// `elapsed_ms` is consolidation's own share of the §4.6 call.
    ///
    /// It is reported **here rather than in the §4.6 cost block**, which carries a single `total`.
    /// Splitting that total into invented halves would be worse than reporting the measured span
    /// on the side channel, and §4.0.7's 30-second ingest deadline is the number this exists to
    /// let a reader attribute.
    pub fn write(&mut self, report: &ConsolidationReport, elapsed_ms: i64) -> std::io::Result<()> {
        let mut row = serde_json::to_value(report).map_err(std::io::Error::other)?;
        if let Some(object) = row.as_object_mut() {
            object.insert("consolidation_ms".into(), serde_json::json!(elapsed_ms));
        }
        writeln!(self.out, "{row}")?;
        self.out.flush()
    }
}
