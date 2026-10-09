//! The retrieval stage profile — one NDJSON row per §4.2 call, under an explicit flag.
//!
//! Same discipline as [`crate::dump`] and for the same reason: a diagnostic side channel, never a
//! fourth channel between harness and implementation. The §4.2 response is byte-identical with or
//! without it, and `cost.latency_ms` is computed by the same `Stopwatch` whether the flag is set
//! or not.
//!
//! ## The residual is a field, not a rounding note
//!
//! Every row carries `span_us` — the measured span the stages were taken inside — and the nine
//! stage totals. `residual_us = span_us - sum(stages)` is what the reader must look at first,
//! because a breakdown that does not add up can hide exactly the cost it was built to find. A
//! profile whose stages sum to 60% of the span would still show a plausible ranking of stages,
//! and the missing 40% would sit in whichever stage a reader assumed it belonged to.
//!
//! ## What is inside the span, and what is deliberately outside it
//!
//! `span_us` covers `select_for_injection` only. The query's own forward pass is timed separately
//! as `embed_us`, because it is the stage a **warm embedding cache removes from the run
//! altogether** — STATE.md records that trap by name, and a profile that folded the two together
//! would report a cold system and a warm system as the same shape. `total_us` is the whole §4.2
//! handler, so `total_us - embed_us - span_us` is the serialization and dump cost that the wire's
//! `latency_ms` includes and the stages do not.
//!
//! Pool sizes ride along on every row. A stage cost is meaningless without the size of the set it
//! ran over: `considered` is the whole-store scan ADR-003's hot index would remove, `scoped` is
//! what the cues rank, and `reranked` is how many pairs the cross-encoder saw.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::Path;

use marlowe_memory::probe::Stage;
use marlowe_memory::retrieve::Selection;

use crate::adapter::RerankSettings;

use crate::elapsed::{ElapsedUs, StageTimer};

pub struct RetrievalProfile {
    out: BufWriter<File>,
}

impl RetrievalProfile {
    pub fn create(path: &Path) -> std::io::Result<Self> {
        // Truncating, not appending — see `FeatureDump::create`. An append would merge two runs'
        // latency distributions into one file, and a P95 read from that is a P95 of neither.
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        Ok(Self {
            out: BufWriter::new(file),
        })
    }

    /// `span_us` is captured by the caller **the instant the pipeline returns**, never read from
    /// the timer here.
    ///
    /// The first draft did read it here, and the difference was not cosmetic: this method runs
    /// after the gate-feature dump has written ~500 JSON rows, so `span` absorbed ~1.5 ms of
    /// diagnostic I/O and reported it as pipeline residual. A reader would have seen a
    /// millisecond and a half of unexplained cost *inside retrieval* that does not exist in
    /// production, and the `unattributed` column that was supposed to catch exactly that read
    /// zero. The instrument was misattributing its own overhead to the thing it measures.
    pub fn write(
        &mut self,
        query_id: &str,
        embed_us: ElapsedUs,
        embed_was_cached: bool,
        timer: &StageTimer,
        span_us: ElapsedUs,
        total_us: ElapsedUs,
        rerank: RerankSettings,
        selection: &Selection<'_>,
    ) -> std::io::Result<()> {
        let totals = timer.totals();
        let span = span_us.as_profile_us();
        let staged: u64 = totals.iter().map(|t| t.as_profile_us()).sum();

        let mut row = serde_json::Map::new();
        row.insert("query_id".into(), query_id.into());
        row.insert("total_us".into(), serde_json::json!(total_us.as_profile_us()));
        row.insert("embed_us".into(), serde_json::json!(embed_us.as_profile_us()));
        // Whether the query's forward pass actually ran. Without this a cold run and a warm run
        // produce the same field name holding two different quantities, which is the trap
        // STATE.md records: a warm cache removes the forward pass from the timed span.
        row.insert("embed_was_cached".into(), serde_json::json!(embed_was_cached));
        row.insert("span_us".into(), serde_json::json!(span));
        for stage in Stage::ALL {
            row.insert(
                format!("{}_us", stage.name()),
                serde_json::json!(totals[stage.index()].as_profile_us()),
            );
        }
        // Saturating, and it can only be non-negative in practice: the stages are taken strictly
        // inside the span. Written rather than left to the reader so a negative — which would
        // mean double-counting — reads as zero here and as a broken sum in the report, instead of
        // wrapping into an enormous plausible-looking number.
        row.insert("residual_us".into(), serde_json::json!(span.saturating_sub(staged)));
        // **The configuration travels with the measurement.** A sweep cell that forgot a flag
        // would otherwise be indistinguishable from one that used it, and the label in a filename
        // is not evidence about what ran.
        row.insert("rerank_batched".into(), serde_json::json!(rerank.batched));
        row.insert("rerank_threads".into(), serde_json::json!(rerank.threads));
        row.insert("rerank_provider".into(), serde_json::json!(rerank.provider.name()));
        // **Which SHAPE ran, on every row.** The two shapes have different published numbers
        // (`RerankPlan::label`); a profile whose rows cannot say which one produced them is a
        // latency table about an unnamed system.
        row.insert("rerank_plan".into(), serde_json::json!(rerank.plan.label()));
        row.insert("considered".into(), serde_json::json!(selection.considered));
        row.insert("scoped".into(), serde_json::json!(selection.scoped));
        row.insert("survived_pruning".into(), serde_json::json!(selection.survived_pruning));
        row.insert("reranked".into(), serde_json::json!(selection.reranked));
        row.insert("injected".into(), serde_json::json!(selection.injected.len()));
        writeln!(self.out, "{}", serde_json::Value::Object(row))?;
        // Flushed per row rather than per run. A profile is read after a run that may be aborted
        // by the §4.0.7 deadline, and a buffered tail would silently shorten the population a P95
        // is computed over.
        self.out.flush()
    }
}
