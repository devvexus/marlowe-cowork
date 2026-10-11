"""The harness's internal shape for a benchmark.

Every adapter -- LongMemEval, LoCoMo, the poisoning suites, the probes -- normalizes into
this. Metrics and suites are written against `Corpus` and never against a benchmark's native
JSON, so adding a benchmark is an adapter and nothing else.

`gold_turn_ids` is the answer key. It is what makes evidence-precision computable, and it is
what the reference stub reads instead of searching.
"""

from __future__ import annotations

from dataclasses import dataclass, field

from ..contract import Turn


@dataclass(frozen=True)
class SessionHistory:
    """One history to load through section 4.6."""

    session_id: str
    turns: tuple[Turn, ...]
    ingest_at_ms: int


@dataclass(frozen=True)
class Case:
    """One scored question."""

    query_id: str
    session_id: str
    question: str
    category: str
    gold_answer: str | None
    gold_turn_ids: tuple[str, ...]
    ask_at_ms: int
    is_abstention: bool = False
    """LongMemEval's abstention subset: the correct behaviour is a refusal, so a confident
    answer is wrong no matter how fluent it is."""

    temporally_clean: bool = True
    """False when the case's own history contains turns dated after `ask_at_ms`.

    Set by the adapter from the released data, not inferred at scoring time. An affected
    case asks a question while the implementation holds memories from the future of the
    query, so **a system that honours the clock is penalised relative to one that ignores
    it** -- exactly backwards. Scored both ways: the headline covers every case for
    comparability, and the clean subset is reported beside it as the number that says
    whether clock handling works.
    """

    notes: str = ""


@dataclass(frozen=True)
class Corpus:
    name: str
    sessions: tuple[SessionHistory, ...]
    cases: tuple[Case, ...]
    categories: tuple[str, ...] = field(default=())

    def session(self, session_id: str) -> SessionHistory:
        for s in self.sessions:
            if s.session_id == session_id:
                return s
        raise KeyError(session_id)

    def turn_text(self) -> dict[str, str]:
        """turn_id -> text, across every session. Used by suites that need to quote a turn."""
        return {t.turn_id: t.text for s in self.sessions for t in s.turns}

    def gold_map(self) -> dict[str, frozenset[str]]:
        """query_id -> the set of turn_ids that are genuinely relevant to it."""
        return {c.query_id: frozenset(c.gold_turn_ids) for c in self.cases}

    def all_categories(self) -> tuple[str, ...]:
        if self.categories:
            return self.categories
        return tuple(sorted({c.category for c in self.cases}))
