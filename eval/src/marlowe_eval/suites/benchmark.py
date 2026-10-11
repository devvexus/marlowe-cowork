"""Drive a corpus through the three interfaces and collect per-query records.

Queries go through section 4.7 `answer`, and injection precision is read off the retrieval
result embedded in the answer. That is deliberate and follows the contract's own reasoning:
section 4.7 embeds rather than references *"or the headline metric cannot be attributed to a
retrieval decision"*. Calling `retrieve` separately would produce a second retrieval that no
answer was built on, and joining the two would be an assumption rather than a measurement.

The `retrieve` interface is exercised directly by the probes and by `conformance`.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any

from ..adapter.base import Client, ImplementationFailure
from ..contract import AnswerRequest, IngestRequest
from ..datasets.model import Corpus
from ..metrics.cost import CostAccumulator, CostSummary
from ..metrics.records import Attributor, InjectedRecord, QueryRecord


@dataclass
class BenchmarkRun:
    corpus: str
    records: list[QueryRecord] = field(default_factory=list)
    attributor: Attributor = field(default_factory=Attributor)
    failures: list[dict[str, str]] = field(default_factory=list)
    ingest_rejections: list[dict[str, str]] = field(default_factory=list)
    cost: CostSummary | None = None

    def as_dict(self) -> dict[str, Any]:
        return {
            "corpus": self.corpus,
            "queries": len(self.records),
            "failed_queries": len(self.failures),
            "ingest_rejections": self.ingest_rejections,
        }


def load_history(client: Client, corpus: Corpus, run: BenchmarkRun) -> None:
    for session in corpus.sessions:
        response = client.ingest(
            IngestRequest(
                clock={"now_ms": session.ingest_at_ms},
                session_id=session.session_id,
                turns=list(session.turns),
            )
        )
        for written in response.written:
            run.attributor.record(written.turn_id, list(written.memory_ids))
        for rejected in response.rejected:
            run.ingest_rejections.append(
                {
                    "session_id": session.session_id,
                    "turn_id": rejected.turn_id,
                    "reason": rejected.reason,
                }
            )


def run_benchmark(
    client: Client,
    corpus: Corpus,
    *,
    token_budget: int = 7000,
    latency_budget_ms: int = 300,
) -> BenchmarkRun:
    run = BenchmarkRun(corpus=corpus.name)
    load_history(client, corpus, run)

    costs = CostAccumulator(
        token_budget=token_budget, latency_budget_ms=latency_budget_ms
    )
    gold_map = corpus.gold_map()

    for case in corpus.cases:
        try:
            response = client.answer(
                AnswerRequest(
                    clock={"now_ms": case.ask_at_ms},
                    query_id=case.query_id,
                    session_id=case.session_id,
                    question=case.question,
                )
            )
        except ImplementationFailure as exc:
            # Class B: a result, not a protocol error. The unit is scored as failed.
            run.failures.append({"query_id": case.query_id, "detail": str(exc)})
            continue

        retrieval = response.retrieval
        gold_turns = gold_map.get(case.query_id, frozenset())
        injected = []
        for item in retrieval.injected:
            attribution, turn_id = run.attributor.attribute(item.memory_id, gold_turns)
            injected.append(
                InjectedRecord(
                    memory_id=item.memory_id,
                    content=item.content,
                    score=item.score,
                    calibrated_precision=item.calibrated_precision,
                    effective_trust=item.effective_trust.value,
                    fidelity=item.fidelity.value,
                    attribution=attribution,
                    turn_id=turn_id,
                )
            )

        costs.add_retrieval(
            retrieval.cost.retrieval_tokens, retrieval.cost.latency_ms.total
        )
        costs.add_generation(
            response.cost.prompt_tokens, response.cost.completion_tokens
        )

        run.records.append(
            QueryRecord(
                query_id=case.query_id,
                session_id=case.session_id,
                category=case.category,
                is_abstention=case.is_abstention,
                temporally_clean=case.temporally_clean,
                injected=tuple(injected),
                considered=retrieval.considered,
                retrieval_abstained=retrieval.abstained,
                abstention_reason=(
                    retrieval.abstention_reason.value
                    if retrieval.abstention_reason
                    else None
                ),
                answered=response.answered,
                abstained=response.abstained,
                answer=response.answer,
                gold_answer=case.gold_answer,
                correct=None,
                retrieval_tokens=retrieval.cost.retrieval_tokens,
                latency_ms=retrieval.cost.latency_ms.total,
                prompt_tokens=response.cost.prompt_tokens,
                completion_tokens=response.cost.completion_tokens,
                gate_threshold=retrieval.gate.threshold,
                gate_version=retrieval.gate.version,
            )
        )

    run.cost = costs.summary()
    return run
