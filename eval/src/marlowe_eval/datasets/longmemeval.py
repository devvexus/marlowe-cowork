"""LongMemEval adapter.

UNVERIFIED AGAINST REAL DATA. This adapter is written from the published schema
description, and the fixtures were authored from the same reading by the same hand -- so a
green fixture suite proves the adapter is self-consistent and proves nothing about whether
it matches the release. `marlowe-eval verify-corpus` against a real download is the check
that closes that gap, and STATE.md records that it has not been run.

Native instance shape this adapter expects:

    {
      "question_id": "...",              # "_abs" suffix marks the abstention subset
      "question_type": "single-session-user" | "multi-session" | ...,
      "question": "...",
      "answer": "...",
      "question_date": "2023/05/20 (Sat) 02:36",
      "haystack_dates": ["...", ...],           # parallel to haystack_sessions
      "haystack_session_ids": ["...", ...],     # parallel to haystack_sessions
      "haystack_sessions": [[{"role": "user", "content": "...",
                              "has_answer": true}, ...], ...],
      "answer_session_ids": ["...", ...]
    }

`has_answer` on a turn is the gold-evidence marker and therefore the answer key.
"""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any

from ..contract import Channel, Origin, Speaker, Turn
from . import timeparse
from .errors import CorpusFormatError
from .model import Case, Corpus, SessionHistory

DATASET = "longmemeval-s"

EXPECTED_COUNTS = {
    "longmemeval-s": 500,
    "longmemeval-m": 500,
}

CATEGORIES = (
    "single-session-user",
    "single-session-assistant",
    "single-session-preference",
    "multi-session",
    "temporal-reasoning",
    "knowledge-update",
    "abstention",
)
"""The six LongMemEval categories the roadmap requires reporting on, plus abstention.

`abstention` is not a native `question_type`: the release marks abstention instances with an
`_abs` suffix on `question_id`. It is promoted to a category here because ROADMAP.md scores
the abstention subset separately and K2 sets a floor on it.
"""

_ROLE_TO_SPEAKER = {
    "user": Speaker.USER,
    "assistant": Speaker.ASSISTANT,
    "system": Speaker.ASSISTANT,
    "tool": Speaker.TOOL,
}


def _require(obj: dict[str, Any], key: str, where: str) -> Any:
    if key not in obj:
        raise CorpusFormatError(DATASET, f"missing required field {key!r}", where=where)
    return obj[key]


def _origin_for(role: str) -> Origin:
    """Declare where the bytes came from. Never a trust class -- section 4.6.

    A benchmark history is a record of a conversation on an authenticated surface, so user
    and assistant turns are terminal-origin. Tool turns are declared as tool output, and the
    poisoning suite is what introduces web origin deliberately.
    """
    if role == "tool":
        return Origin(channel=Channel.TOOL_OUTPUT, actor="tool:benchmark", ref=None)
    return Origin(channel=Channel.TERMINAL, actor=f"user:{role}", ref=None)


def load(path: Path, *, dataset: str = DATASET) -> Corpus:
    raw = json.loads(Path(path).read_text(encoding="utf-8"))
    if not isinstance(raw, list):
        raise CorpusFormatError(dataset, f"expected a JSON array of instances, got {type(raw).__name__}")

    sessions: list[SessionHistory] = []
    cases: list[Case] = []
    seen_sessions: set[str] = set()

    for idx, inst in enumerate(raw):
        where = f"instance[{idx}]"
        qid = str(_require(inst, "question_id", where))
        question = str(_require(inst, "question", where))
        qtype = str(_require(inst, "question_type", where))
        is_abstention = qid.endswith("_abs")
        category = "abstention" if is_abstention else qtype

        ask_at_ms = timeparse.parse(
            _require(inst, "question_date", where), dataset=dataset, where=f"{where}.question_date"
        )

        haystack = _require(inst, "haystack_sessions", where)
        session_ids = _require(inst, "haystack_session_ids", where)
        dates = _require(inst, "haystack_dates", where)
        if not (len(haystack) == len(session_ids) == len(dates)):
            raise CorpusFormatError(
                dataset,
                f"haystack_sessions ({len(haystack)}), haystack_session_ids "
                f"({len(session_ids)}) and haystack_dates ({len(dates)}) are not parallel",
                where=where,
            )

        gold_turn_ids: list[str] = []
        # One case may span many haystack sessions. The harness ingests each as its own
        # section 4.6 history and scopes the question to a synthetic per-question session,
        # so multi-session cases stay multi-session rather than being flattened.
        case_session = f"{qid}"

        merged_turns: list[Turn] = []
        for s_idx, (sid, session, date) in enumerate(zip(session_ids, haystack, dates)):
            session_at = timeparse.parse(
                date, dataset=dataset, where=f"{where}.haystack_dates[{s_idx}]"
            )
            for t_idx, turn in enumerate(session):
                t_where = f"{where}.haystack_sessions[{s_idx}][{t_idx}]"
                role = str(_require(turn, "role", t_where))
                content = str(_require(turn, "content", t_where))
                turn_id = f"{sid}-{t_idx}"
                merged_turns.append(
                    Turn(
                        turn_id=turn_id,
                        speaker=_ROLE_TO_SPEAKER.get(role, Speaker.USER),
                        text=content,
                        occurred_at_ms=session_at + t_idx * 1000,
                        origin=_origin_for(role),
                    )
                )
                if turn.get("has_answer"):
                    gold_turn_ids.append(turn_id)

        if case_session in seen_sessions:
            raise CorpusFormatError(dataset, f"duplicate question_id {qid!r}", where=where)
        seen_sessions.add(case_session)

        sessions.append(
            SessionHistory(
                session_id=case_session,
                turns=tuple(merged_turns),
                ingest_at_ms=min((t.occurred_at_ms for t in merged_turns), default=ask_at_ms),
            )
        )
        # Reproduce the source faithfully; do not correct it. The released corpus dates some
        # haystack sessions after their question -- verified against the real release, where
        # 76 of 500 cases are affected and 43 have gold evidence postdating the question.
        # Flagged per case rather than repaired, so the headline stays comparable and the
        # clean subset can be reported beside it.
        temporally_clean = all(t.occurred_at_ms <= ask_at_ms for t in merged_turns)
        cases.append(
            Case(
                query_id=qid,
                session_id=case_session,
                question=question,
                category=category,
                gold_answer=None if is_abstention else str(inst.get("answer", "")),
                gold_turn_ids=tuple(gold_turn_ids),
                ask_at_ms=ask_at_ms,
                is_abstention=is_abstention,
                temporally_clean=temporally_clean,
            )
        )

    return Corpus(
        name=dataset,
        sessions=tuple(sessions),
        cases=tuple(cases),
        categories=CATEGORIES,
    )


def abstention_subset(corpus: Corpus) -> Corpus:
    """The subset K2 puts a floor on: correctly declining on events that never happened."""
    cases = tuple(c for c in corpus.cases if c.is_abstention)
    keep = {c.session_id for c in cases}
    return Corpus(
        name=f"{corpus.name}-abstention",
        sessions=tuple(s for s in corpus.sessions if s.session_id in keep),
        cases=cases,
        categories=("abstention",),
    )
