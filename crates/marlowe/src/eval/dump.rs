//! Side channels for `--eval-adapter`: the gate-feature dump and the one real-clock read.
//!
//! The dump is never the wire: the §4.2 response is byte-identical with or without it. One row
//! per scored candidate (negatives included), carrying this build's own ranking key so
//! `tools/score_longmemeval.py` and `tools/recall_at_k.py` never re-implement the gate.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::Path;
use std::time::Instant;

use marlowe_memory::retrieve::ScoredCandidate;

pub struct FeatureDump {
    out: BufWriter<File>,
}

impl FeatureDump {
    /// Truncates: an append would merge two runs into one file.
    pub fn create(path: &Path) -> std::io::Result<Self> {
        let file = OpenOptions::new().write(true).create(true).truncate(true).open(path)?;
        Ok(Self { out: BufWriter::new(file) })
    }

    pub fn write(&mut self, query_id: &str, candidates: &[ScoredCandidate<'_>]) -> std::io::Result<()> {
        for c in candidates {
            let mut row = serde_json::Map::new();
            row.insert("query_id".into(), query_id.into());
            row.insert("memory_id".into(), c.entry.id.clone().into());
            for (name, value) in c.features.named() {
                row.insert(name.into(), serde_json::json!(value));
            }
            row.insert("score".into(), serde_json::json!(c.score));
            row.insert("margin".into(), serde_json::json!(c.margin));
            row.insert("calibrated_precision".into(), serde_json::json!(c.calibrated_precision));
            row.insert("winning_cue".into(), c.winning_cue.into());
            row.insert("passes".into(), serde_json::json!(c.passes));
            row.insert("session_key".into(), serde_json::json!(c.session_key));
            row.insert("survived_pruning".into(), serde_json::json!(c.survived_pruning));
            // null when not reranked, never 0.0: a logit near zero is a real score.
            row.insert(
                "rerank_score".into(),
                c.rerank_score.map_or(serde_json::Value::Null, |v| serde_json::json!(v)),
            );
            writeln!(self.out, "{}", serde_json::Value::Object(row))?;
        }
        self.out.flush()
    }
}

/// A monotonic duration that may only be reported as cost (§4.2 `latency_ms`). It is not a
/// timestamp and nothing in memory can key off it; every time in memory comes from the
/// request's `clock` (§4.5).
pub struct Stopwatch(Instant);

impl Stopwatch {
    pub fn start() -> Self {
        Self(Instant::now())
    }

    /// Whole milliseconds, saturating.
    pub fn cost_ms(&self) -> i64 {
        i64::try_from(self.0.elapsed().as_millis()).unwrap_or(i64::MAX)
    }
}
