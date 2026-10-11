"""LoCoMo adapter.

UNVERIFIED AGAINST REAL DATA -- same caveat as the LongMemEval adapter, same remedy
(`marlowe-eval verify-corpus`).

Baseline only. The brief is explicit that LoCoMo is not to be treated as sufficient: several
systems' published numbers on it are not comparable, which is part of why M0a exists.

Native shape this adapter expects:

    {
      "sample_id": "...",
      "conversation": {
        "speaker_a": "Alice", "speaker_b": "Bob",
        "session_1_date_time": "1:56 pm on 8 May, 2023",
        "session_1": [{"speaker": "Alice", "dia_id": "D1:1", "text": "..."}, ...],
        ...
      },
      "qa": [{"question": "...", "answer": "...", "evidence": ["D1:2"], "category": 1}]
    }

`evidence` holds dialogue ids and is therefore the answer key. Category 5 is the adversarial
/ unanswerable class and maps to abstention.
"""

from __future__ import annotations

import json
import re
from pathlib import Path
from typing import Any

from ..contract import Channel, Origin, Speaker, Turn
from . import timeparse
from .errors import CorpusFormatError
from .model import Case, Corpus, SessionHistory

DATASET = "locomo"
EXPECTED_QUESTION_COUNT = 1540

CATEGORIES = (
    "multi-hop",
    "temporal",
    "open-domain",
    "single-hop",
    "adversarial",
)

_CATEGORY_BY_CODE = {
    1: "multi-hop",
    2: "temporal",
    3: "open-domain",
    4: "single-hop",
    5: "adversarial",
}

_SESSION_KEY = re.compile(r"^session_(\d+)$")


def _require(obj: dict[str, Any], key: str, where: str) -> Any:
    if key not in obj:
        raise CorpusFormatError(DATASET, f"missing required field {key!r}", where=where)
    return obj[key]


def load(path: Path) -> Corpus:
    raw = json.loads(Path(path).read_text(encoding="utf-8"))
    if not isinstance(raw, list):
        raise CorpusFormatError(
            DATASET, f"expected a JSON array of samples, got {type(raw).__name__}"
        )

    sessions: list[SessionHistory] = []
    cases: list[Case] = []

    for idx, sample in enumerate(raw):
        where = f"sample[{idx}]"
        sample_id = str(_require(sample, "sample_id", where))
        conversation = _require(sample, "conversation", where)

        turns: list[Turn] = []
        first_ms: int | None = None
        for key in sorted(
            (k for k in conversation if _SESSION_KEY.match(k)),
            key=lambda k: int(_SESSION_KEY.match(k).group(1)),  # type: ignore[union-attr]
        ):
            date_key = f"{key}_date_time"
            if date_key not in conversation:
                raise CorpusFormatError(
                    DATASET, f"session {key!r} has no {date_key!r}", where=where
                )
            session_at = timeparse.parse(
                conversation[date_key], dataset=DATASET, where=f"{where}.{date_key}"
            )
            first_ms = session_at if first_ms is None else min(first_ms, session_at)

            for t_idx, utterance in enumerate(conversation[key]):
                t_where = f"{where}.{key}[{t_idx}]"
                dia_id = str(_require(utterance, "dia_id", t_where))
                turns.append(
                    Turn(
                        turn_id=dia_id,
                        speaker=Speaker.USER,
                        text=str(_require(utterance, "text", t_where)),
                        occurred_at_ms=session_at + t_idx * 1000,
                        origin=Origin(
                            channel=Channel.MESSAGING,
                            actor=f"user:{utterance.get('speaker', 'unknown')}",
                            ref=None,
                        ),
                    )
                )

        if not turns:
            raise CorpusFormatError(DATASET, "sample has no session turns", where=where)

        sessions.append(
            SessionHistory(
                session_id=sample_id,
                turns=tuple(turns),
                ingest_at_ms=first_ms or 0,
            )
        )

        last_ms = max(t.occurred_at_ms for t in turns)
        for q_idx, qa in enumerate(_require(sample, "qa", where)):
            q_where = f"{where}.qa[{q_idx}]"
            code = qa.get("category")
            category = _CATEGORY_BY_CODE.get(code, f"category-{code}")
            is_abstention = category == "adversarial"
            evidence = qa.get("evidence") or []
            if isinstance(evidence, str):
                evidence = [evidence]
            cases.append(
                Case(
                    query_id=f"{sample_id}-q{q_idx}",
                    session_id=sample_id,
                    question=str(_require(qa, "question", q_where)),
                    category=category,
                    gold_answer=None if is_abstention else str(qa.get("answer", "")),
                    gold_turn_ids=tuple(str(e) for e in evidence),
                    # Asked a day after the last session, on the synthetic clock. LoCoMo
                    # carries no per-question timestamp, so this is the harness's declared
                    # choice rather than a value read from the data. A day rather than a
                    # minute because section 4.3 maturation is a real exclusion: querying
                    # immediately after ingest would score every implementation that
                    # correctly withholds unmatured beliefs as though it had failed.
                    ask_at_ms=last_ms + 86_400_000,
                    is_abstention=is_abstention,
                )
            )

    return Corpus(
        name=DATASET,
        sessions=tuple(sessions),
        cases=tuple(cases),
        categories=CATEGORIES,
    )
