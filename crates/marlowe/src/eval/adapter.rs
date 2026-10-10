//! The §4.0 adapter: request frames on stdin, response frames on stdout, one JSON value per
//! line. The harness owns the process lifetime and ends the run by closing stdin.
//!
//! Ported from the old harness's adapter onto the current `marlowe-memory` API. The pipeline is
//! the shipped one: lexical + dense cues, frozen gate, session pruning, CrossEncoder rerank at
//! `RERANK_BUDGET`, the declared operating point, `ADMIT_TOP_K`, and frozen consolidation at
//! the end of every ingest. `RerankSettings::cascade` swaps in the GPU cascade for measurement.

use std::io::{BufRead, Write};
use std::path::Path;

use marlowe_contract::{
    AbstentionReason, AnswerCost, AnswerLatency, AnswerRequest, AnswerResponse, ContractVersion,
    ErrorKind, GateStamp, IngestCost, IngestLatency, IngestRequest, IngestResponse, Op,
    RequestFrame, ResponseFrame, RetrievalBudget, RetrievalCost, RetrievalLatency,
    RetrievalRequest, RetrievalResponse, CONTRACT_VERSION,
};
use marlowe_journal::{Journal, Profile};
use marlowe_memory::consolidate::{self, Policy};
use marlowe_memory::cue::dense::embedder::Embedder;
use marlowe_memory::cue::dense::vectors::VectorStore;
use marlowe_memory::cascade::Cascade;
use marlowe_memory::rerank::CrossEncoder;
use marlowe_memory::retrieve::{
    debug_assert_injection_valid, select_for_injection, Rerank, RetrievalScope, Scoring,
    RERANK_BUDGET,
};
use marlowe_memory::{ingest, Abstention, BeliefStore, Coverage, FrozenGate, OperatingPoint, GATE_VERSION};

use super::dump::{FeatureDump, Stopwatch};

/// §4.7 retrieval budget for an answer call; the same 7,000 tokens / 300 ms the scoring
/// driver's budget condition reads.
const ANSWER_BUDGET: RetrievalBudget = RetrievalBudget { max_tokens: 7000, max_latency_ms: 300 };

/// How the rerank stage runs. `encoder: None` is an explicit `--reranking off`.
pub struct RerankSettings {
    pub encoder: Option<CrossEncoder>,
    /// The fusion members; `Some` runs `Rerank::Cascade` with `encoder` as the narrower.
    pub cascade: Option<Cascade>,
    /// Shipped value `false` on CPU (`RerankProvider::default_batching`).
    pub batched: bool,
}

pub struct Adapter {
    journal: Journal,
    beliefs: BeliefStore,
    embedder: Embedder,
    vectors: VectorStore,
    gate: FrozenGate,
    operating_point: OperatingPoint,
    coverage: Coverage,
    policy: Policy,
    rerank: RerankSettings,
    dump: Option<FeatureDump>,
}

impl Adapter {
    /// Start against a fresh, empty profile root (`Profile::init` refuses a non-empty one, which
    /// keeps the harness's spawns from sharing state). The gate, operating point and
    /// consolidation artifact load here, so a bad artifact stops the process before any query.
    pub fn start(
        profile_root: &Path,
        embedder: Embedder,
        rerank: RerankSettings,
        coverage: Coverage,
        dump_path: Option<&Path>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let gate = FrozenGate::load()?;
        let operating_point = OperatingPoint::load()?;
        let policy = Policy::load()?;
        let dump = dump_path.map(FeatureDump::create).transpose()?;
        let profile = Profile::init(profile_root)?;
        let journal = Journal::open(&profile)?;
        let beliefs = BeliefStore::derive(&journal, profile.manifest().derivation_version)?;
        Ok(Self {
            journal,
            beliefs,
            embedder,
            vectors: VectorStore::default(),
            gate,
            operating_point,
            coverage,
            policy,
            rerank,
            dump,
        })
    }

    /// The serial request/response loop (§4.0.5). One `\n` per frame, flushed every time.
    pub fn run(&mut self, input: impl BufRead, mut output: impl Write) -> std::io::Result<()> {
        for line in input.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let frame = self.handle_line(&line);
            output.write_all(frame.to_line().as_bytes())?;
            output.write_all(b"\n")?;
            output.flush()?;
        }
        Ok(())
    }

    pub fn handle_line(&mut self, line: &str) -> ResponseFrame {
        let frame: RequestFrame = match serde_json::from_str(line) {
            Ok(f) => f,
            // No op to echo; class A aborts the run regardless of the placeholder.
            Err(e) => {
                return ResponseFrame::error(
                    Op::Ingest,
                    ErrorKind::MalformedFrame,
                    format!("could not parse request frame: {e}"),
                )
            }
        };
        match frame.op {
            Op::Ingest => self.handle_ingest(frame.body),
            Op::Retrieve => self.handle_retrieve(frame.body),
            Op::Answer => self.handle_answer(frame.body),
        }
    }

    fn handle_ingest(&mut self, body: serde_json::Value) -> ResponseFrame {
        let request: IngestRequest = match serde_json::from_value(body) {
            Ok(r) => r,
            Err(e) => return malformed_body(Op::Ingest, e),
        };
        if let Some(frame) = check_version(Op::Ingest, &request.contract_version) {
            return frame;
        }
        let stopwatch = Stopwatch::start();
        let outcome = match ingest(&mut self.journal, &mut self.beliefs, &request) {
            Ok(o) => o,
            // Class B: a result the harness scores, not a crash.
            Err(e) => return internal(Op::Ingest, e.to_string()),
        };
        // Batched: one LongMemEval ingest is ~493 turns.
        if let Err(e) = self.vectors.embed_missing(&self.beliefs, &mut self.embedder) {
            return internal(Op::Ingest, format!("embedding the ingested memories failed: {e}"));
        }
        // §5.3 consolidation at session close (= end of this call), after embedding because the
        // merge rule reads the vectors. Reads `request.clock`, never a system clock.
        if let Err(e) = consolidate::consolidate(
            &mut self.journal,
            &mut self.beliefs,
            &request.session_id,
            request.clock,
            &self.vectors,
            self.policy,
        ) {
            return internal(Op::Ingest, format!("consolidating {} failed: {e}", request.session_id));
        }
        let response = IngestResponse {
            contract_version: ContractVersion,
            session_id: request.session_id.clone(),
            written: outcome.written,
            rejected: outcome.rejected,
            cost: IngestCost {
                // No model call happens on ingest; zero is the true count.
                ingest_tokens: 0,
                latency_ms: IngestLatency { total: stopwatch.cost_ms() },
            },
        };
        body_frame(Op::Ingest, &response)
    }

    fn handle_retrieve(&mut self, body: serde_json::Value) -> ResponseFrame {
        let request: RetrievalRequest = match serde_json::from_value(body) {
            Ok(r) => r,
            Err(e) => return malformed_body(Op::Retrieve, e),
        };
        if let Some(frame) = check_version(Op::Retrieve, &request.contract_version) {
            return frame;
        }
        let response = self.retrieve(&request, Stopwatch::start());
        if let Err(e) = response.check_exclusivity() {
            return internal(Op::Retrieve, format!("outbound section 4.2 violation: {e}"));
        }
        body_frame(Op::Retrieve, &response)
    }

    fn retrieve(&mut self, request: &RetrievalRequest, stopwatch: Stopwatch) -> RetrievalResponse {
        // Cue + gate + rerank are one measured span reported under `cues`.
        let cue_watch = Stopwatch::start();
        // A failed query embedding degrades the dense cue to 0.0 for every candidate rather than
        // failing the request.
        let query_vector = match self.embedder.embed(&request.query_text) {
            Ok(v) => Some(v),
            Err(e) => {
                eprintln!("marlowe: query embedding failed, dense cue degraded to zero: {e}");
                None
            }
        };
        let scoring = Scoring::Gated {
            gate: &self.gate,
            operating_point: &self.operating_point,
            coverage: self.coverage,
        };
        let batched = self.rerank.batched;
        let mut rerank = match (self.rerank.encoder.as_mut(), self.rerank.cascade.as_mut()) {
            (None, _) => Rerank::Off,
            (Some(encoder), Some(cascade)) => {
                Rerank::Cascade { encoder, fusion: cascade.members_mut(), batched }
            }
            (Some(encoder), None) => Rerank::CrossEncoder { encoder, budget: RERANK_BUDGET, batched },
        };
        let selection = select_for_injection(
            &self.beliefs,
            &request.session_id,
            &request.query_text,
            request.clock.now_ms,
            request.budget.max_tokens,
            &scoring,
            &self.vectors,
            query_vector.as_deref(),
            &mut rerank,
            // ThisSession, always: LongMemEval flattens each case's haystack into one history
            // whose session_id is the query id. Profile scope would let cases see each other.
            RetrievalScope::ThisSession,
        );
        let cues_ms = cue_watch.cost_ms();
        debug_assert_injection_valid(&selection.injected);

        let abstained = selection.injected.is_empty();
        // The wire enum is pinned at four variants; the finer reasons collapse onto
        // NoCandidateAboveThreshold.
        let abstention_reason = if !abstained {
            None
        } else if let Some(reason) = selection.abstention {
            Some(match reason {
                Abstention::NoCandidates => AbstentionReason::NoCandidates,
                Abstention::NoRunnerUp
                | Abstention::MarginUndefined
                | Abstention::BelowThreshold
                | Abstention::NoReranker => AbstentionReason::NoCandidateAboveThreshold,
            })
        } else if selection.budget_exhausted {
            Some(AbstentionReason::BudgetExhausted)
        } else if selection.scoped > 0 && selection.above_threshold == 0 {
            Some(AbstentionReason::NoCandidateAboveThreshold)
        } else {
            Some(AbstentionReason::NoCandidates)
        };

        // Inside the latency span, as every published latency was measured.
        if let Some(dump) = self.dump.as_mut() {
            if let Err(e) = dump.write(&request.query_id, &selection.scored) {
                // A truncated dump would silently shrink the scored population.
                eprintln!("marlowe: gate-feature dump failed: {e}");
                std::process::exit(1);
            }
        }

        RetrievalResponse {
            contract_version: ContractVersion,
            query_id: request.query_id.clone(),
            abstained,
            abstention_reason,
            injected: selection.injected,
            considered: selection.considered,
            gate: GateStamp {
                // The measurement arm is stamped on the wire (`+coverage-full`).
                version: format!("{GATE_VERSION}{}", self.coverage.stamp()),
                threshold: self.gate.threshold(),
                adaptive: false,
            },
            cost: RetrievalCost {
                retrieval_tokens: selection.retrieval_tokens,
                latency_ms: RetrievalLatency {
                    total: stopwatch.cost_ms(),
                    cues: Some(cues_ms),
                    ..Default::default()
                },
            },
        }
    }

    fn handle_answer(&mut self, body: serde_json::Value) -> ResponseFrame {
        let request: AnswerRequest = match serde_json::from_value(body) {
            Ok(r) => r,
            Err(e) => return malformed_body(Op::Answer, e),
        };
        if let Some(frame) = check_version(Op::Answer, &request.contract_version) {
            return frame;
        }
        let stopwatch = Stopwatch::start();
        let retrieval_request = RetrievalRequest {
            contract_version: CONTRACT_VERSION.to_string(),
            clock: request.clock,
            query_id: request.query_id.clone(),
            session_id: request.session_id.clone(),
            turn_index: 0,
            query_text: request.question.clone(),
            budget: ANSWER_BUDGET,
        };
        let retrieval = self.retrieve(&retrieval_request, Stopwatch::start());
        let retrieval_tokens = retrieval.cost.retrieval_tokens;
        let retrieval_latency = retrieval.cost.latency_ms.total;
        // No generator is wired, so every answer is an honest `degraded_path` abstention with
        // empty `grounded_in`.
        let response = AnswerResponse {
            contract_version: ContractVersion,
            query_id: request.query_id.clone(),
            answered: false,
            answer: None,
            abstained: true,
            abstention_reason: Some(AbstentionReason::DegradedPath),
            grounded_in: Vec::new(),
            retrieval,
            cost: AnswerCost {
                prompt_tokens: 0,
                completion_tokens: 0,
                retrieval_tokens,
                latency_ms: AnswerLatency {
                    total: stopwatch.cost_ms(),
                    retrieval: Some(retrieval_latency),
                    generation: None,
                },
            },
        };
        if let Err(e) = response.check_exclusivity() {
            return internal(Op::Answer, format!("outbound section 4.7 violation: {e}"));
        }
        body_frame(Op::Answer, &response)
    }
}

fn body_frame(op: Op, response: &impl serde::Serialize) -> ResponseFrame {
    match serde_json::to_value(response) {
        Ok(value) => ResponseFrame::body(op, value),
        Err(e) => ResponseFrame::error(op, ErrorKind::InternalError, e.to_string()),
    }
}

fn internal(op: Op, detail: String) -> ResponseFrame {
    ResponseFrame::error(op, ErrorKind::InternalError, detail)
}

/// §4.0.4 class A. An unknown `channel` lands here: the closed enum refuses it.
fn malformed_body(op: Op, error: serde_json::Error) -> ResponseFrame {
    ResponseFrame::error(op, ErrorKind::MalformedBody, error.to_string())
}

fn check_version(op: Op, found: &str) -> Option<ResponseFrame> {
    (found != CONTRACT_VERSION).then(|| {
        ResponseFrame::error(
            op,
            ErrorKind::ContractVersionUnsupported,
            format!("this build speaks {CONTRACT_VERSION}, the request declared {found}"),
        )
    })
}
