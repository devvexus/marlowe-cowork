"""Cost accounting -- the thing every accuracy number is chained to.

Section 5.7: *"Report the pair. Every accuracy number ships with its token cost and latency,
or it does not ship."* Section 4 makes that structural on the wire by refusing to represent a
response without `cost`. This module extends the same treatment to the report: an accuracy
figure is only constructible as part of a `Scored`, which requires a `CostSummary`.

There is no code path that produces a bare accuracy float. That is the point -- the rule
survives someone who has not read section 5.7.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any, Sequence


def percentile(values: Sequence[float], p: float) -> float:
    """Nearest-rank percentile. Deterministic, no interpolation, no numpy."""
    if not values:
        return 0.0
    ordered = sorted(values)
    rank = max(1, min(len(ordered), int(-(-p * len(ordered) // 1))))
    return float(ordered[rank - 1])


@dataclass(frozen=True)
class CostSummary:
    """Tokens and latency for a set of queries.

    `retrieval_tokens_*` is the number section 5.7's <=7,000 budget applies to. Generation
    tokens are tracked separately and never folded in, because folding them would hide a
    miss inside a bigger number.
    """

    queries: int
    retrieval_tokens_mean: float
    retrieval_tokens_p95: float
    retrieval_tokens_max: int
    generation_tokens_mean: float
    prompt_tokens_mean: float
    latency_p50_ms: float
    latency_p95_ms: float
    latency_max_ms: int
    over_token_budget: int
    over_latency_budget: int
    token_budget: int
    latency_budget_ms: int

    def as_dict(self) -> dict[str, Any]:
        return {
            "queries": self.queries,
            "retrieval_tokens": {
                "mean": round(self.retrieval_tokens_mean, 3),
                "p95": round(self.retrieval_tokens_p95, 3),
                "max": self.retrieval_tokens_max,
                "budget": self.token_budget,
                "over_budget": self.over_token_budget,
            },
            "generation_tokens": {"mean": round(self.generation_tokens_mean, 3)},
            "prompt_tokens": {"mean": round(self.prompt_tokens_mean, 3)},
            # Latency lives under a key on the canonical.py timing allowlist, so it is
            # reported and scored but excluded from the reproduction hash.
            "latency_ms": {
                "p50": self.latency_p50_ms,
                "p95": self.latency_p95_ms,
                "max": self.latency_max_ms,
                "budget": self.latency_budget_ms,
                "over_budget": self.over_latency_budget,
            },
        }


class CostAccumulator:
    """Collects per-query cost as a run proceeds."""

    def __init__(self, *, token_budget: int = 7000, latency_budget_ms: int = 300) -> None:
        self.token_budget = token_budget
        self.latency_budget_ms = latency_budget_ms
        self._retrieval_tokens: list[int] = []
        self._generation_tokens: list[int] = []
        self._prompt_tokens: list[int] = []
        self._latency: list[int] = []

    def add_retrieval(self, retrieval_tokens: int, latency_ms: int) -> None:
        self._retrieval_tokens.append(retrieval_tokens)
        self._latency.append(latency_ms)

    def add_generation(self, prompt_tokens: int, completion_tokens: int) -> None:
        self._prompt_tokens.append(prompt_tokens)
        self._generation_tokens.append(completion_tokens)

    def summary(self) -> CostSummary:
        rt = self._retrieval_tokens
        lat = self._latency
        mean = lambda xs: (sum(xs) / len(xs)) if xs else 0.0  # noqa: E731
        return CostSummary(
            queries=len(rt),
            retrieval_tokens_mean=mean(rt),
            retrieval_tokens_p95=percentile(rt, 0.95),
            retrieval_tokens_max=max(rt, default=0),
            generation_tokens_mean=mean(self._generation_tokens),
            prompt_tokens_mean=mean(self._prompt_tokens),
            latency_p50_ms=percentile(lat, 0.50),
            latency_p95_ms=percentile(lat, 0.95),
            latency_max_ms=max(lat, default=0),
            # A budget miss is a SCORED RESULT, never a protocol error. An implementation
            # exceeding 300 ms is a number the eval exists to produce; it is not a reason
            # the run cannot report. See proposed section 4.0.7.
            over_token_budget=sum(1 for x in rt if x > self.token_budget),
            over_latency_budget=sum(1 for x in lat if x > self.latency_budget_ms),
            token_budget=self.token_budget,
            latency_budget_ms=self.latency_budget_ms,
        )


@dataclass(frozen=True)
class Scored:
    """An accuracy figure that cannot exist without its cost.

    Mirrors section 4.2b's treatment of `RetrievalCost`: not Optional, so "report the pair"
    is enforced by the constructor rather than by whoever writes the renderer.
    """

    name: str
    value: float
    n: int
    cost: CostSummary
    detail: dict[str, Any] | None = None

    def as_dict(self) -> dict[str, Any]:
        out: dict[str, Any] = {
            "metric": self.name,
            "value": round(self.value, 6),
            "n": self.n,
            "cost": self.cost.as_dict(),
        }
        if self.detail:
            out["detail"] = self.detail
        return out
