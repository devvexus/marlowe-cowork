"""Answer accuracy and abstention.

Two things kept apart on purpose:

  * A wrong answer and a refusal on an answerable question are both failures, and they are
    different failures. Folding them together hides the trade an implementation is making.
  * An abstention on the abstention subset is a SUCCESS. K2 puts a floor on it precisely
    because confident fabrication from memory is the characteristic failure of these
    systems.

On grading. LongMemEval's published protocol grades with an LLM judge, so the default
deterministic grader here does NOT produce numbers comparable to published results, and
says so in the report rather than hoping the reader remembers. Every accuracy figure carries
the name of the grader that produced it -- there is no unattributed accuracy.
"""

from __future__ import annotations

import re
from dataclasses import dataclass
from typing import Any, Protocol, Sequence

from .cost import CostSummary, Scored
from .records import QueryRecord

_WORD = re.compile(r"[a-z0-9]+")
_STOP = frozenset(
    "a an the of to in on at is was were are be been it its this that you your i my "
    "and or for with from as by not no yes do did does have has had will would can".split()
)


def _content_tokens(text: str) -> list[str]:
    return [w for w in _WORD.findall(text.lower()) if w not in _STOP]


class AnswerGrader(Protocol):
    name: str
    comparable_to_published: bool

    def grade(self, question: str, gold: str, answer: str) -> bool: ...


@dataclass(frozen=True)
class ContainmentGrader:
    """Deterministic: what fraction of the gold answer's content words appear?

    Strict, cheap, seed-free and reproducible -- which is what M0a needs to test itself.
    It is a regression signal, not a benchmark result.
    """

    threshold: float = 0.6
    name: str = "containment-v1"
    comparable_to_published: bool = False

    def grade(self, question: str, gold: str, answer: str) -> bool:
        gold_tokens = _content_tokens(gold)
        if not gold_tokens:
            return False
        answer_tokens = set(_content_tokens(answer))
        hits = sum(1 for t in gold_tokens if t in answer_tokens)
        return (hits / len(gold_tokens)) >= self.threshold


@dataclass(frozen=True)
class AccuracyBreakdown:
    correct: int
    wrong: int
    abstained_when_answerable: int
    n: int
    by_category: dict[str, dict[str, float]]

    @property
    def value(self) -> float:
        return (self.correct / self.n) if self.n else 0.0


def grade_records(records: Sequence[QueryRecord], grader: AnswerGrader) -> dict[str, bool | None]:
    out: dict[str, bool | None] = {}
    for rec in records:
        if rec.is_abstention:
            # Correct iff it declined cleanly. Section 4.7 already forbids a populated
            # answer alongside answered=false, so a hedge cannot reach here as a pass.
            out[rec.query_id] = bool(rec.abstained and not rec.answered)
        elif not rec.answered:
            out[rec.query_id] = False
        else:
            out[rec.query_id] = grader.grade(
                "", rec.gold_answer or "", rec.answer or ""
            )
    return out


def accuracy(
    records: Sequence[QueryRecord], grader: AnswerGrader, cost: CostSummary
) -> Scored:
    answerable = [r for r in records if not r.is_abstention]
    graded = grade_records(answerable, grader)

    correct = sum(1 for r in answerable if graded[r.query_id])
    abstained = sum(1 for r in answerable if not r.answered)
    wrong = len(answerable) - correct - abstained

    per_cat: dict[str, dict[str, float]] = {}
    for category in sorted({r.category for r in answerable}):
        subset = [r for r in answerable if r.category == category]
        hits = sum(1 for r in subset if graded[r.query_id])
        per_cat[category] = {"accuracy": hits / len(subset), "n": float(len(subset))}

    # The temporally-clean subset, reported beside the headline and never instead of it.
    # A case whose history contains turns dated after its own question penalises a system
    # that honours the clock relative to one that ignores it, so a large gap between these
    # two numbers is a signal about clock handling rather than about recall. The headline
    # stays the full set, because that is what a published LongMemEval number means.
    clean = [r for r in answerable if r.temporally_clean]
    clean_correct = sum(1 for r in clean if graded[r.query_id])

    return Scored(
        name="answer_accuracy",
        value=(correct / len(answerable)) if answerable else 0.0,
        n=len(answerable),
        cost=cost,
        detail={
            "grader": grader.name,
            "temporally_clean_subset": {
                "accuracy": round((clean_correct / len(clean)) if clean else 0.0, 6),
                "n": len(clean),
                "excluded": len(answerable) - len(clean),
                "note": (
                    "diagnostic, not the headline. Excluded cases hold memories dated "
                    "after their own question, so a clock-honouring system is penalised "
                    "relative to one that ignores the clock. A large gap between this and "
                    "the headline is a signal about clock handling"
                ),
            },
            "comparable_to_published": grader.comparable_to_published,
            "comparability_note": (
                "graded deterministically; LongMemEval's published protocol uses an LLM "
                "judge, so this figure is a regression signal and not a comparable score"
            )
            if not grader.comparable_to_published
            else "",
            "correct": correct,
            "wrong": wrong,
            "abstained_when_answerable": abstained,
            "by_category": per_cat,
        },
    )


def abstention_accuracy(records: Sequence[QueryRecord], cost: CostSummary) -> Scored:
    subset = [r for r in records if r.is_abstention]
    correct = sum(1 for r in subset if r.abstained and not r.answered)
    fabricated = sum(1 for r in subset if r.answered)
    return Scored(
        name="abstention_accuracy",
        value=(correct / len(subset)) if subset else 0.0,
        n=len(subset),
        cost=cost,
        detail={
            "correct_refusals": correct,
            "fabrications": fabricated,
            "note": "an answered question here is a confident fabrication about an event "
            "that never happened",
        },
    )


def as_dict(scored: Scored) -> dict[str, Any]:
    return scored.as_dict()
