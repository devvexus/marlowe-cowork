"""Injection precision -- three of them, and they must never be confused.

The headline metric, the one K1 sets at >=0.95, is **human-judged**. The other two are not
it, and the report is built so that they cannot be mistaken for it:

  evidence_precision   against the benchmark's own gold-evidence key. Computable offline,
                       on every run, for any implementation. A legitimate proxy and a good
                       regression signal. NOT the headline.
  judge_precision      from the offline LLM judge (HP1, tier 2). Permitted only for gate
                       training signal and for tracking between human label refreshes, and
                       it cannot be constructed at all without an agreement figure against
                       the human set. See judge/agreement.py.
  human_precision      the headline. Requires the human label set, which ROADMAP.md is
                       explicit is the human's deliverable and not the agent's.

ROADMAP.md: *"Injection precision may not be validated against agent-generated relevance
labels. Doing so reintroduces exactly the circularity the M0a/M0b split exists to prevent."*
Keeping three separately-named numbers is how that stays true under time pressure -- there
is no single `injection_precision` field for a tired person to fill in with whichever one
they have.
"""

from __future__ import annotations

from collections import defaultdict
from dataclasses import dataclass
from typing import Any, Iterable, Sequence

from .cost import CostSummary, Scored
from .records import DISTRACTOR, GOLD, UNATTRIBUTABLE, QueryRecord


@dataclass(frozen=True)
class EvidencePrecision:
    """Precision against the benchmark answer key."""

    value: float
    gold: int
    distractor: int
    unattributable: int
    answerable_queries: int
    injections_on_abstention_cases: int
    by_category: dict[str, float]
    by_decile: dict[str, dict[str, float]]

    def as_dict(self) -> dict[str, Any]:
        return {
            "metric": "evidence_precision",
            "value": round(self.value, 6),
            "note": (
                "precision against benchmark gold evidence. NOT the K1 headline, which is "
                "human-judged injection precision."
            ),
            "gold": self.gold,
            "distractor": self.distractor,
            "unattributable": self.unattributable,
            "answerable_queries": self.answerable_queries,
            "injections_on_abstention_cases": self.injections_on_abstention_cases,
            "by_category": {k: round(v, 6) for k, v in sorted(self.by_category.items())},
            "by_decile": {
                k: {kk: round(vv, 6) for kk, vv in v.items()}
                for k, v in sorted(self.by_decile.items())
            },
        }


def decile_of(score: float) -> str:
    """Gate-score decile. The sampler stratifies on this, so it is defined in one place."""
    d = min(9, max(0, int(score * 10)))
    return f"{d/10:.1f}-{(d+1)/10:.1f}"


def evidence_precision(records: Sequence[QueryRecord]) -> EvidencePrecision:
    gold = distractor = unattributable = 0
    on_abstention = 0
    per_category: dict[str, list[int]] = defaultdict(list)
    per_decile: dict[str, list[int]] = defaultdict(list)
    answerable = 0

    for rec in records:
        if rec.is_abstention:
            # Every injection here is a false positive by the answer key, but folding them
            # into the same rate would conflate "picked the wrong evidence" with "produced
            # evidence for a question that has none". They are different failures.
            on_abstention += len(rec.injected)
            continue
        answerable += 1
        for item in rec.injected:
            if item.attribution == UNATTRIBUTABLE:
                unattributable += 1
                continue
            hit = 1 if item.attribution == GOLD else 0
            gold += hit
            distractor += 1 - hit
            per_category[rec.category].append(hit)
            per_decile[decile_of(item.calibrated_precision)].append(hit)

    attributed = gold + distractor
    return EvidencePrecision(
        value=(gold / attributed) if attributed else 0.0,
        gold=gold,
        distractor=distractor,
        unattributable=unattributable,
        answerable_queries=answerable,
        injections_on_abstention_cases=on_abstention,
        by_category={k: sum(v) / len(v) for k, v in per_category.items() if v},
        by_decile={
            k: {"precision": sum(v) / len(v), "n": float(len(v))}
            for k, v in per_decile.items()
            if v
        },
    )


@dataclass(frozen=True)
class HumanJudgedPrecision:
    """The K1 headline. Constructible only from a human label set.

    There is no default, no fallback, and no "estimated from the judge" path. If the label
    set is absent this object does not exist and the report says so in words.
    """

    value: float
    labelled: int
    relevant: int
    label_set_id: str
    by_category: dict[str, float]
    by_decile: dict[str, dict[str, float]]
    coverage_warnings: tuple[str, ...]

    def as_dict(self) -> dict[str, Any]:
        return {
            "metric": "injection_precision_human",
            "value": round(self.value, 6),
            "note": "the K1 headline metric: fraction of auto-injected memories a human "
            "judge rated relevant",
            "labelled": self.labelled,
            "relevant": self.relevant,
            "label_set_id": self.label_set_id,
            "by_category": {k: round(v, 6) for k, v in sorted(self.by_category.items())},
            "by_decile": {
                k: {kk: round(vv, 6) for kk, vv in v.items()}
                for k, v in sorted(self.by_decile.items())
            },
            "coverage_warnings": list(self.coverage_warnings),
        }


def paired(name: str, value: float, n: int, cost: CostSummary, detail: dict[str, Any] | None = None) -> Scored:
    """Every reported rate goes through here, so none of them can lose their cost block."""
    return Scored(name=name, value=value, n=n, cost=cost, detail=detail)


def summarize_trust(records: Iterable[QueryRecord]) -> dict[str, int]:
    """Effective-trust histogram over injected memories.

    Reported on every run because the laundering suite's assertion is only meaningful
    against a background: if nothing untrusted is ever injected, "no untrusted memory was
    mislabelled" is a claim about an empty set.
    """
    counts: dict[str, int] = defaultdict(int)
    for rec in records:
        for item in rec.injected:
            counts[item.effective_trust] += 1
    return dict(sorted(counts.items()))


__all__ = [
    "DISTRACTOR",
    "GOLD",
    "UNATTRIBUTABLE",
    "EvidencePrecision",
    "HumanJudgedPrecision",
    "decile_of",
    "evidence_precision",
    "paired",
    "summarize_trust",
]
