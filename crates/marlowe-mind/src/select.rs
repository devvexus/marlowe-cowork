//! Injection (the shipped gated pipeline with the dense cue wired) and explicit recall.

use std::time::Instant;

use marlowe_journal::{Actor as JournalActor, AppendRequest, EventKind, TraceId};
use marlowe_memory::cue::{dense, lexical};
use marlowe_memory::probe::StageProbe;
use marlowe_memory::retrieve::{
    debug_assert_injection_valid, estimate_tokens, select_for_injection_filtered, Rerank, RetrievalScope, Scoring,
    Selection, ADMIT_TOP_K, RERANK_BUDGET,
};
use marlowe_memory::{Abstention, Coverage, MemoryEntry, GATE_VERSION};
use marlowe_proto::MemoryHit;
use serde::Serialize;

use crate::actor::{is_live, now_ms, Actor, EmbedRun};
use crate::error::MindError;
use crate::live::{LiveProbe, LiveSink, MindEvent};
use crate::tags::{is_episode_summary, recall_admits, wire_kind, wire_scope};
use crate::types::{wire_trust, InjectQuery, Injection, RecallQuery};
use crate::workspaces::{self, Workspaces};
use crate::MAX_INJECT_TOKENS;

/// Texts embedded per background step; small so commands interleave.
const EMBED_CHUNK: usize = 32;
/// Injection candidates without a vector that an injection embeds on the spot. With more missing
/// (boot without a vector file, an import in flight) the injection abstains until the background
/// queue catches up: a missing vector would score 0 on the dense cue, which the shipped path
/// never does.
const INJECT_FLUSH_LIMIT: usize = 64;
/// Same, for explicit recall (latency matters less).
const RECALL_FLUSH_LIMIT: usize = 512;
/// Reciprocal-rank-fusion constant for recall.
const RRF_K: f32 = 60.0;
/// Default recall size when the caller passes 0.
const DEFAULT_RECALL: usize = 10;
/// Cross-encoder logits added to a same-project memory when filling the slots after rank 1.
/// Product path only and unmeasured; the eval adapter never passes a workspace.
pub(crate) const WORKSPACE_BOOST: f32 = 1.0;

/// `MemoryInjected` payload: what was admitted and why, for the audit trail.
#[derive(Serialize)]
struct InjectedPayload<'a> {
    session_id: &'a str,
    memory_ids: Vec<String>,
    rerank_margin: Option<f32>,
    gate_version: &'a str,
    considered: u32,
}

impl Actor {
    pub(crate) fn inject(&mut self, q: InjectQuery, live: Option<&LiveSink>) -> Injection {
        let started = Instant::now();
        let mut probe = LiveProbe::new(live);
        let mut out = match self.try_inject(&q, &mut probe) {
            Ok(i) => i,
            Err(reason) => Injection { abstained: Some(reason), ..Injection::default() },
        };
        // An early abstention leaves its stage open.
        probe.finish();
        out.stages = std::mem::take(&mut probe.stages);
        out.latency_ms = started.elapsed().as_millis() as u64;
        out
    }

    fn try_inject(&mut self, q: &InjectQuery, probe: &mut LiveProbe<'_>) -> Result<Injection, String> {
        if q.text.trim().is_empty() {
            return Err("the query is empty".into());
        }
        if let Some(reason) = self.models.degraded() {
            return Err(format!("degraded: {reason}"));
        }
        let now = now_ms();
        let workspace = q.workspace.as_deref().filter(|w| !w.trim().is_empty());
        // User requests, other projects' explicit Project memories and ids already in context
        // (the profile) never compete; assistant prose is excluded by the core.
        let admit = |e: &MemoryEntry| workspaces::admits(e, workspace) && !q.exclude.contains(&e.id);
        let (total, missing) = {
            let pool: Vec<&MemoryEntry> = self.beliefs.injection_candidates(now).into_iter().filter(|e| admit(e)).collect();
            let missing: Vec<(String, String)> = pool
                .iter()
                .filter(|e| self.vectors.get(&e.id).is_none())
                .map(|e| (e.id.clone(), e.text.clone()))
                .collect();
            (pool.len(), missing)
        };
        // The operating point needs a rank 2; with fewer than two candidates it abstains anyway,
        // so say so without loading ~200 MB of models.
        match total {
            0 => return Err(Abstention::NoCandidates.as_str().into()),
            1 => return Err(Abstention::NoRunnerUp.as_str().into()),
            _ => {}
        }
        if missing.len() > INJECT_FLUSH_LIMIT {
            return Err(format!("memory index warming ({} of {total} embedded)", total - missing.len()));
        }
        if !missing.is_empty() {
            probe.begin("index", None);
            for (i, chunk) in missing.chunks(EMBED_CHUNK).enumerate() {
                probe.progress((i * EMBED_CHUNK) as u32, missing.len() as u32);
                self.embed_and_install(chunk.to_vec())?;
            }
        }
        if !self.models.embedder_resident() {
            let size = self.models.embedder_bytes();
            probe.begin("load", Some(format!("embedder · {}", mb(size))));
        }
        let embedder = self.models.embedder()?;
        probe.begin("embed", None);
        let query_vector = embedder.embed(&q.text).map_err(|e| format!("query embedding failed: {e}"))?;
        let cold_reranker = !self.models.reranker_resident();
        if cold_reranker {
            let size = self.models.reranker_bytes();
            probe.begin("load", Some(format!("reranker · {}", mb(size))));
        }
        let reranker = self.models.reranker()?;
        if cold_reranker {
            probe.end(Some(if reranker.choice.provider.is_gpu() { "gpu" } else { "cpu" }.into()));
        }
        probe.label("rerank", if reranker.choice.provider.is_gpu() { "gpu" } else { "cpu" }.into());
        let scoring = Scoring::Gated { gate: &self.gate, operating_point: &self.point, coverage: Coverage::Declared };
        // Batching follows the provider: off on CPU (measured slower at one thread), on for GPU.
        let batched = reranker.choice.provider.default_batching();
        let mut rerank = match reranker.cascade.as_mut() {
            Some(c) => Rerank::Cascade { encoder: &mut reranker.encoder, fusion: c.members_mut(), batched },
            None => Rerank::CrossEncoder { encoder: &mut reranker.encoder, budget: RERANK_BUDGET, batched },
        };
        let budget = q.budget_tokens.min(MAX_INJECT_TOKENS);
        let selection = select_for_injection_filtered(
            &self.beliefs,
            &q.session,
            &q.text,
            now,
            budget,
            &scoring,
            &self.vectors,
            Some(&query_vector),
            &mut rerank,
            RetrievalScope::Profile,
            &admit,
            probe,
        );
        debug_assert_injection_valid(&selection.injected);

        if selection.injected.is_empty() {
            return Err(match selection.abstention {
                Some(a) => a.as_str().to_string(),
                None if selection.budget_exhausted => "the first memory did not fit the token budget".into(),
                None => "nothing was eligible".into(),
            });
        }
        // The workspace boost re-sorts by single-graph logit; under the cascade the fusion's order
        // stands as measured.
        let fused = selection.scored.iter().any(|c| c.fusion_rank.is_some());
        let slate = workspace
            .filter(|_| !fused)
            .and_then(|ws| boosted_slate(&selection, budget, &self.workspaces, ws))
            .unwrap_or_else(|| selection.injected.iter().map(|m| (m.memory_id.clone(), m.score)).collect());
        let mut blocks = Vec::new();
        let mut recalled = Vec::new();
        for (id, score) in &slate {
            let Some(entry) = self.beliefs.get(id) else { continue };
            let h = hit(entry, *score);
            blocks.push(block(&h, is_episode_summary(entry)));
            recalled.push(h);
        }
        let payload = InjectedPayload {
            session_id: &q.session,
            memory_ids: recalled.iter().map(|h| h.id.clone()).collect(),
            rerank_margin: selection.rerank_margin,
            gate_version: GATE_VERSION,
            considered: selection.considered,
        };
        let event = AppendRequest {
            trace_id: TraceId::new_v5(&TraceId::NAMESPACE_OID, q.session.as_bytes()),
            session_id: Some(q.session.clone()),
            run_id: None,
            actor: JournalActor::Harness,
            kind: EventKind::MemoryInjected,
            payload: serde_json::to_value(&payload).map_err(|e| e.to_string())?,
        };
        // The journal is the audit spine: an injection it cannot record does not happen.
        self.journal
            .append(marlowe_contract::Clock::new(now_ms()), event)
            .map_err(|e| format!("could not journal the injection: {e}"))?;
        let floor = recalled.iter().map(|h| h.trust).min();
        Ok(Injection { blocks, recalled, floor, abstained: None, latency_ms: 0, stages: Vec::new() })
    }

    pub(crate) fn recall(&mut self, q: RecallQuery) -> Result<Vec<MemoryHit>, MindError> {
        let limit = if q.limit == 0 { DEFAULT_RECALL } else { q.limit };
        if q.text.trim().is_empty() {
            return Ok(Vec::new());
        }
        // Dense when available; lexical alone in degraded mode.
        let query_vector = if self.models.degraded().is_none() {
            if self.queue.pending.len() <= RECALL_FLUSH_LIMIT && !self.queue.rebuild {
                let _ = self.flush_embeddings(RECALL_FLUSH_LIMIT);
            }
            self.models.embedder().ok().and_then(|e| e.embed(&q.text).ok())
        } else {
            None
        };
        let candidates: Vec<&MemoryEntry> =
            self.beliefs.entries().filter(|e| is_live(e) && recall_admits(q.scope.as_ref(), e)).collect();
        if candidates.is_empty() {
            return Ok(Vec::new());
        }
        let lexical = lexical::score_indexed(self.beliefs.lexicon(), &candidates, &q.text);
        let dense: Vec<Option<f32>> = candidates
            .iter()
            .map(|e| query_vector.as_ref().and_then(|qv| self.vectors.get(&e.id).map(|v| dense::cosine(qv, v))))
            .collect();

        // Reciprocal rank fusion of the two cues; ties break by id.
        let n = candidates.len();
        let by_id = |a: &usize, b: &usize| candidates[*a].id.cmp(&candidates[*b].id);
        let mut fused = vec![0.0f32; n];
        let mut lex_rank: Vec<usize> = (0..n).filter(|i| lexical[*i] > 0.0).collect();
        lex_rank.sort_by(|a, b| lexical[*b].total_cmp(&lexical[*a]).then_with(|| by_id(a, b)));
        let mut dense_rank: Vec<usize> = (0..n).filter(|i| dense[*i].is_some()).collect();
        dense_rank.sort_by(|a, b| dense[*b].unwrap_or(0.0).total_cmp(&dense[*a].unwrap_or(0.0)).then_with(|| by_id(a, b)));
        for ranking in [&lex_rank, &dense_rank] {
            for (rank, i) in ranking.iter().enumerate() {
                fused[*i] += 1.0 / (RRF_K + rank as f32 + 1.0);
            }
        }
        let mut order: Vec<usize> = (0..n).filter(|i| fused[*i] > 0.0).collect();
        order.sort_by(|a, b| fused[*b].total_cmp(&fused[*a]).then_with(|| by_id(a, b)));
        order.truncate(limit);
        Ok(order.into_iter().map(|i| hit(candidates[i], fused[i])).collect())
    }

    /// Embeds up to `limit` queued beliefs now.
    pub(crate) fn flush_embeddings(&mut self, limit: usize) -> Result<(), String> {
        let mut done = 0;
        while !self.queue.is_empty() && done < limit {
            let n = self.embed_step()?;
            if n == 0 && self.queue.is_empty() {
                break;
            }
            done += n.max(1);
        }
        Ok(())
    }

    /// One embedding chunk. Returns how many vectors it installed.
    pub(crate) fn embed_step(&mut self) -> Result<usize, String> {
        if self.models.degraded().is_some() {
            self.queue.clear();
            return Ok(0);
        }
        if self.embed_run.is_none() {
            let total = if self.queue.rebuild { self.vectors.pending(&self.beliefs).len() } else { self.queue.pending.len() } as u32;
            self.embed_run = Some(EmbedRun { started: Instant::now(), total, done: 0, installed: 0 });
            self.tell(MindEvent::EmbedStart { queued: total });
        }
        let batch: Vec<(String, String)> = if self.queue.rebuild {
            let missing = self.vectors.pending(&self.beliefs);
            if missing.len() <= EMBED_CHUNK {
                self.queue.rebuild = false;
            }
            missing.into_iter().take(EMBED_CHUNK).collect()
        } else {
            let mut out = Vec::new();
            while out.len() < EMBED_CHUNK {
                let Some(id) = self.queue.pending.pop_first() else { break };
                if let Some(e) = self.beliefs.get(&id) {
                    if e.fidelity > marlowe_contract::Fidelity::Tombstone && self.vectors.get(&id).is_none() {
                        out.push((id, e.text.clone()));
                    }
                }
            }
            out
        };
        let taken = batch.len() as u32;
        match self.embed_and_install(batch) {
            Ok(n) => {
                self.note_embedded(taken, n as u32);
                Ok(n)
            }
            Err(e) => {
                // The popped ids are recovered by a pass over everything still missing.
                self.queue.pending.clear();
                self.queue.rebuild = true;
                self.embed_run = None;
                Err(e)
            }
        }
    }

    /// Reports a finished chunk to the write-tail listener, and the run's end when the queue is empty.
    fn note_embedded(&mut self, taken: u32, installed: u32) {
        let Some(run) = &mut self.embed_run else { return };
        run.done += taken;
        run.installed += installed;
        run.total = run.total.max(run.done);
        let (done, total) = (run.done, run.total);
        self.tell(MindEvent::EmbedProgress { done, total });
        if self.queue.is_empty() {
            if let Some(run) = self.embed_run.take() {
                self.tell(MindEvent::EmbedDone { installed: run.installed, ms: run.started.elapsed().as_millis() as u64 });
                self.tell(MindEvent::Indexed);
            }
        }
    }

    /// Embeds `batch` and installs the vectors whose belief text is unchanged.
    fn embed_and_install(&mut self, batch: Vec<(String, String)>) -> Result<usize, String> {
        if batch.is_empty() {
            return Ok(0);
        }
        let texts: Vec<String> = batch.iter().map(|(_, t)| t.clone()).collect();
        let vectors = self.models.embedder()?.embed_batch(&texts).map_err(|e| e.to_string())?;
        let mut installed = 0;
        for ((id, text), v) in batch.iter().zip(vectors) {
            installed += usize::from(self.vectors.install(&self.beliefs, id, text, v));
        }
        self.vectors_dirty |= installed > 0;
        Ok(installed)
    }
}

/// `127 MB` for a byte count.
fn mb(bytes: u64) -> String {
    format!("{} MB", (bytes + 500_000) / 1_000_000)
}

/// A reranked candidate competing for a slot after rank 1.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Slot<'a> {
    pub id: &'a str,
    pub text: &'a str,
    pub rerank: f32,
    pub score: f32,
    pub margin: f32,
    pub same_project: bool,
}

/// The shipped slate with the workspace boost applied, or `None` (keep the shipped slate) when
/// nothing was injected or no reranked candidate is from this workspace.
fn boosted_slate(sel: &Selection<'_>, max_tokens: u32, map: &Workspaces, workspace: &str) -> Option<Vec<(String, f32)>> {
    let first = sel.injected.first()?;
    let slots: Vec<Slot<'_>> = sel
        .scored
        .iter()
        .filter(|c| c.survived_pruning && c.entry.id != first.memory_id)
        .filter_map(|c| {
            Some(Slot {
                id: &c.entry.id,
                text: &c.entry.text,
                rerank: c.rerank_score?,
                score: c.score,
                margin: c.margin,
                same_project: map.same_project(c.entry, workspace),
            })
        })
        .collect();
    if !slots.iter().any(|s| s.same_project) {
        return None;
    }
    Some(fill_slots((&first.memory_id, &first.content, first.score), slots, max_tokens))
}

/// Rank 1 (the memory the operating point judged) stays first. Slots 2..=ADMIT_TOP_K go to the
/// reranked candidates by rerank logit plus [`WORKSPACE_BOOST`] when same-project, then the
/// core's own tie-breaks (score, margin, id). Stops at the first that does not fit the budget,
/// as the core does.
pub(crate) fn fill_slots(first: (&str, &str, f32), mut slots: Vec<Slot<'_>>, max_tokens: u32) -> Vec<(String, f32)> {
    let key = |s: &Slot<'_>| s.rerank + if s.same_project { WORKSPACE_BOOST } else { 0.0 };
    slots.sort_by(|a, b| {
        key(b)
            .total_cmp(&key(a))
            .then_with(|| b.score.total_cmp(&a.score))
            .then_with(|| b.margin.total_cmp(&a.margin))
            .then_with(|| a.id.cmp(b.id))
    });
    let mut out = vec![(first.0.to_string(), first.2)];
    let mut tokens = estimate_tokens(first.1);
    for s in slots.into_iter().take(ADMIT_TOP_K - 1) {
        let cost = estimate_tokens(s.text);
        if tokens + cost > max_tokens {
            break;
        }
        tokens += cost;
        out.push((s.id.to_string(), s.score));
    }
    out
}

pub(crate) fn hit(e: &MemoryEntry, score: f32) -> MemoryHit {
    MemoryHit {
        id: e.id.clone(),
        text: e.text.clone(),
        kind: wire_kind(e.kind()),
        trust: wire_trust(e.effective_trust),
        scope: wire_scope(e.scope()),
        occurred_at_ms: e.occurred_at_ms,
        provenance: e.provenance().describe(),
        score,
    }
}

/// One context block per injected memory, stating what it is and where it came from.
fn block(h: &MemoryHit, episode: bool) -> String {
    let what = if episode { "Earlier session" } else { "Memory" };
    format!(
        "[{what} {} · {} · {} · {} · {}] {}",
        h.id,
        h.kind.label(),
        h.trust.label(),
        civil_date(h.occurred_at_ms),
        h.provenance,
        h.text
    )
}

/// `YYYY-MM-DD` (UTC) for epoch milliseconds.
pub(crate) fn civil_date(ms: i64) -> String {
    let (y, m, d) = civil(ms);
    format!("{y:04}-{m:02}-{d:02}")
}

/// (year, month, day) in UTC for epoch milliseconds.
pub(crate) fn civil(ms: i64) -> (i64, i64, i64) {
    let days = ms.div_euclid(86_400_000);
    // Hinnant's civil-from-days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}
