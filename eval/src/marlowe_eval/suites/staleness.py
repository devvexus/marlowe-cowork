"""The staleness-half-life probe.

Write fact A at T1, its replacement A' at T2, then query at a sweep of T3 values and watch
how long A keeps coming back. Section 4.5 exists so this is possible: all three times are
supplied, so the sweep is over synthetic time and the whole curve can be collected in one
process in milliseconds.

Reported honestly. If A never falls to half its initial retrieval rate inside the horizon,
the answer is "not observed within N days" rather than a number extrapolated off the end of
the data -- HP4 calls silent staleness genuinely unsolved, and a fabricated half-life would
hide exactly the thing the measurement was added to expose.
"""

from __future__ import annotations

from typing import Any, Callable

from ..adapter.base import Client, MemorySystem
from ..contract import IngestRequest, RetrievalRequest
from ..datasets.model import Corpus
from ..determinism import DAY_MS
from ..metrics.records import Attributor
from ..metrics.staleness import StalenessCurve, StalenessPoint, fit
from .scenarios import staleness_scenario

MakeSystem = Callable[[Corpus], MemorySystem]

DEFAULT_SWEEP_DAYS = (0, 1, 2, 3, 5, 7, 10, 14, 21, 30, 45, 60, 90)


def run(
    make: MakeSystem,
    *,
    pairs: int = 24,
    sweep_days: tuple[int, ...] = DEFAULT_SWEEP_DAYS,
) -> tuple[StalenessCurve, dict[str, Any]]:
    scenario = staleness_scenario(pairs=pairs)
    client = Client(make(scenario.corpus))
    attributor = Attributor()

    # T1: the original fact, plus filler.
    for session in scenario.corpus.sessions:
        original = [t for t in session.turns if not t.turn_id.endswith("#v2")]
        response = client.ingest(
            IngestRequest(
                clock={"now_ms": scenario.t1_ms},
                session_id=session.session_id,
                turns=original,
            )
        )
        for written in response.written:
            attributor.record(written.turn_id, list(written.memory_ids))

    # T2: the replacement.
    for session in scenario.corpus.sessions:
        replacement = [t for t in session.turns if t.turn_id.endswith("#v2")]
        response = client.ingest(
            IngestRequest(
                clock={"now_ms": scenario.t2_ms},
                session_id=session.session_id,
                turns=replacement,
            )
        )
        for written in response.written:
            attributor.record(written.turn_id, list(written.memory_ids))

    superseded_ids = {
        mid
        for turn_id in scenario.superseded_turn_ids
        for mid in attributor.memories_of(turn_id)
    }

    points: list[StalenessPoint] = []
    for day in sweep_days:
        delta = day * DAY_MS
        still = 0
        asked = 0
        for case in scenario.corpus.cases:
            result = client.retrieve(
                RetrievalRequest(
                    # The plain query id, not one decorated with the sweep point. Holding
                    # it fixed is what isolates the variable: the only thing changing
                    # across the sweep is the clock, so the curve measures decay rather
                    # than a re-roll of candidate selection.
                    query_id=case.query_id,
                    session_id=case.session_id,
                    turn_index=0,
                    query_text=case.question,
                    clock={"now_ms": scenario.t2_ms + delta},
                    budget={"max_tokens": 7000, "max_latency_ms": 300},
                )
            )
            asked += 1
            if any(i.memory_id in superseded_ids for i in result.injected):
                still += 1
        points.append(
            StalenessPoint(delta_ms=delta, queries=asked, superseded_still_injected=still)
        )

    client.close()
    curve = fit(points)
    return curve, {
        "pairs": pairs,
        "sweep_days": list(sweep_days),
        "superseded_memories_tracked": len(superseded_ids),
        "note": (
            "supersession pairs carry both contradicting text and a `base#vN` turn-id "
            "convention, so the probe measures a real implementation's resolution and an "
            "oracle stub's scripted one without either needing the other's mechanism"
        ),
    }
