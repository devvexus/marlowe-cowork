"""Per-query records -- what the runner collects and every metric reads.

One join in here is worth explaining, because getting it wrong would quietly invalidate
evidence precision against any real implementation.

The harness cannot assume an injected `memory_id` encodes the turn it came from: the stub
happens to build ids from turn ids, and M0b will not. The mapping comes from the section 4.6
ingest response, which returns `written: [{turn_id, memory_ids}]` for exactly this reason.
The runner inverts that mapping and joins on it.

An injected memory absent from the mapping is **unattributable**, not a false positive. A
consolidated or merged belief has an id that was never returned by an ingest, and scoring it
as wrong would penalise consolidation for existing. Unattributable injections are counted
and reported on their own line instead -- a real number about a real gap, rather than a
guess folded into the headline.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any

GOLD = "gold"
DISTRACTOR = "distractor"
UNATTRIBUTABLE = "unattributable"


@dataclass(frozen=True)
class InjectedRecord:
    memory_id: str
    content: str
    score: float
    calibrated_precision: float
    effective_trust: str
    fidelity: str
    attribution: str
    turn_id: str | None = None

    def as_dict(self) -> dict[str, Any]:
        return {
            "memory_id": self.memory_id,
            "score": self.score,
            "calibrated_precision": self.calibrated_precision,
            "effective_trust": self.effective_trust,
            "fidelity": self.fidelity,
            "attribution": self.attribution,
            "turn_id": self.turn_id,
        }


@dataclass(frozen=True)
class QueryRecord:
    query_id: str
    session_id: str
    category: str
    is_abstention: bool
    temporally_clean: bool
    injected: tuple[InjectedRecord, ...]
    considered: int
    retrieval_abstained: bool
    abstention_reason: str | None
    answered: bool
    abstained: bool
    answer: str | None
    gold_answer: str | None
    correct: bool | None
    retrieval_tokens: int
    latency_ms: int
    prompt_tokens: int
    completion_tokens: int
    gate_threshold: float
    gate_version: str
    notes: tuple[str, ...] = field(default=())

    def as_dict(self) -> dict[str, Any]:
        return {
            "query_id": self.query_id,
            "session_id": self.session_id,
            "category": self.category,
            "is_abstention": self.is_abstention,
            "temporally_clean": self.temporally_clean,
            "injected": [i.as_dict() for i in self.injected],
            "considered": self.considered,
            "retrieval_abstained": self.retrieval_abstained,
            "abstention_reason": self.abstention_reason,
            "answered": self.answered,
            "abstained": self.abstained,
            "answer": self.answer,
            "correct": self.correct,
            "retrieval_tokens": self.retrieval_tokens,
            "gate": {"version": self.gate_version, "threshold": self.gate_threshold},
            "notes": list(self.notes),
        }


class Attributor:
    """turn_id <-> memory_id, built from section 4.6 ingest responses."""

    def __init__(self) -> None:
        self._turn_of: dict[str, str] = {}
        self._memories_of: dict[str, list[str]] = {}

    def record(self, turn_id: str, memory_ids: list[str]) -> None:
        for mid in memory_ids:
            self._turn_of[mid] = turn_id
        self._memories_of.setdefault(turn_id, []).extend(memory_ids)

    def turn_of(self, memory_id: str) -> str | None:
        return self._turn_of.get(memory_id)

    def memories_of(self, turn_id: str) -> list[str]:
        """Forward lookup. A turn may produce several memories, or none."""
        return list(self._memories_of.get(turn_id, ()))

    def attribute(self, memory_id: str, gold_turns: frozenset[str]) -> tuple[str, str | None]:
        turn = self._turn_of.get(memory_id)
        if turn is None:
            return UNATTRIBUTABLE, None
        return (GOLD if turn in gold_turns else DISTRACTOR), turn
