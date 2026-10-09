"""The clock-conformance probe.

Section 4.5 is binding: *"on any path reachable from sections 4.1, 4.6 or 4.7, the
implementation MUST NOT read a system clock."* Everything decay-dependent rests on it, and
staleness half-life is unmeasurable without it.

You cannot prove the absence of a clock read from outside the process. What you can do is
make a system that reads one behave observably differently, and that is what this probe
does. Two tests, and the second is the one with teeth:

  A. **Translation invariance.** Run an identical scenario twice, with every supplied
     timestamp shifted by ten years. Only relative time changed nothing, so a conforming
     implementation must produce identical output. A system-clock reader computes ages as
     (wall_now - occurred_at), which differs by the shift, so its output moves.

  B. **Time dependence.** Ingest, then query immediately and again a year later on the
     synthetic clock. A conforming implementation differs across that gap, because section
     4.3 exclusion (3) withholds unmatured beliefs from auto-injection. A system-clock
     reader sees the same wall instant for both queries and returns the same thing.

Test B is why the probe is decisive rather than merely suggestive. An implementation with no
observable time dependence at all fails it -- and deserves to: section 4.3 calls maturation
the cheapest available defence against single-exposure poisoning, and warns that an
implementation which drops it *"has silently removed that defence while still passing every
latency and precision test."* This probe is the test it does not pass.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any, Callable

from ..adapter.base import Client, MemorySystem
from ..contract import IngestRequest, RetrievalRequest
from ..datasets.model import Corpus
from ..determinism import DAY_MS
from .scenarios import clock_corpus

TEN_YEARS_MS = 10 * 365 * DAY_MS
ONE_YEAR_MS = 365 * DAY_MS

PASS = "pass"
FAIL_TRANSLATION = "fail_translation_variance"
FAIL_NO_TIME_DEPENDENCE = "fail_no_time_dependence"


@dataclass(frozen=True)
class ClockProbeResult:
    verdict: str
    translation_identical: bool
    time_dependent: bool
    detail: dict[str, Any]

    @property
    def passed(self) -> bool:
        return self.verdict == PASS

    def as_dict(self) -> dict[str, Any]:
        return {
            "probe": "clock_conformance",
            "verdict": self.verdict,
            "translation_invariant": self.translation_identical,
            "time_dependent": self.time_dependent,
            "explanation": {
                PASS: "output depends on the supplied clock and only on the supplied clock",
                FAIL_TRANSLATION: (
                    "shifting every supplied timestamp by ten years changed the result; "
                    "something on the section 4.1/4.6 path is reading a system clock"
                ),
                FAIL_NO_TIME_DEPENDENCE: (
                    "a year of synthetic time changed nothing. Either the implementation "
                    "reads a system clock, or it does not implement section 4.3's "
                    "maturation exclusion. Both are failures, and the second is a silently "
                    "removed poisoning defence"
                ),
            }[self.verdict],
            "detail": self.detail,
        }


MakeSystem = Callable[[Corpus], MemorySystem]


def _fingerprint(
    make: MakeSystem, corpus: Corpus, offset_ms: int, gap_ms: int
) -> tuple[tuple[str, ...], bool]:
    """Ingest and query at `offset`, return (injected ids, abstained) after `gap`."""
    client = Client(make(corpus))
    session = corpus.sessions[0]
    client.ingest(
        IngestRequest(
            clock={"now_ms": session.ingest_at_ms + offset_ms},
            session_id=session.session_id,
            turns=[
                t.model_copy(update={"occurred_at_ms": t.occurred_at_ms + offset_ms})
                for t in session.turns
            ],
        )
    )
    case = corpus.cases[0]
    result = client.retrieve(
        RetrievalRequest(
            query_id=case.query_id,
            session_id=case.session_id,
            turn_index=0,
            query_text=case.question,
            clock={"now_ms": session.ingest_at_ms + offset_ms + gap_ms},
            budget={"max_tokens": 7000, "max_latency_ms": 300},
        )
    )
    client.close()
    return tuple(i.memory_id for i in result.injected), result.abstained


def run(make: MakeSystem, corpus: Corpus | None = None) -> ClockProbeResult:
    corpus = corpus or clock_corpus()

    # A: identical scenario, ten years apart on the synthetic clock.
    here = _fingerprint(make, corpus, 0, ONE_YEAR_MS)
    shifted = _fingerprint(make, corpus, TEN_YEARS_MS, ONE_YEAR_MS)
    translation_identical = here == shifted

    # B: same ingest, queried immediately and a year later.
    immediate = _fingerprint(make, corpus, 0, 0)
    later = here
    time_dependent = immediate != later

    if not translation_identical:
        verdict = FAIL_TRANSLATION
    elif not time_dependent:
        verdict = FAIL_NO_TIME_DEPENDENCE
    else:
        verdict = PASS

    return ClockProbeResult(
        verdict=verdict,
        translation_identical=translation_identical,
        time_dependent=time_dependent,
        detail={
            "at_epoch": {"injected": list(here[0]), "abstained": here[1]},
            "shifted_ten_years": {"injected": list(shifted[0]), "abstained": shifted[1]},
            "queried_immediately": {
                "injected": list(immediate[0]),
                "abstained": immediate[1],
            },
            "queried_after_one_year": {"injected": list(later[0]), "abstained": later[1]},
        },
    )
