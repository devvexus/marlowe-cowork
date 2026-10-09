"""Generated corpora for the probes and for instrument calibration.

Distinct from the fixtures, and the distinction matters:

  * fixtures test the ADAPTERS -- can we read the native schema? They are tiny and hand
    written, and they cannot validate themselves (see verify.py).
  * synthetic corpora test the HARNESS -- does the arithmetic recover a known input? They
    need volume, not realism, and they are generated from a seed so they carry no schema
    assumptions at all.

Using a fixture for calibration would be a mistake in both directions: too few cases to
bound a rate, and a schema misreading would show up as a metric error.
"""

from __future__ import annotations

from ..contract import Channel, Origin, Speaker, Turn
from ..determinism import DAY_MS, HOUR_MS, stream
from .model import Case, Corpus, SessionHistory

_TOPICS = (
    "the ingest job", "the billing service", "the staging cluster", "the retry policy",
    "the nightly backup", "the auth gateway", "the search index", "the deploy script",
    "the rate limiter", "the metrics pipeline", "the config loader", "the queue worker",
)
_FACTS = (
    "runs on a two-minute timer", "was moved to eu-west", "is owned by Priya",
    "logs to the shared bucket", "has a thirty-second timeout", "retries three times",
    "was rewritten in March", "is behind the feature flag",
)

BASE_MS = 1_700_000_000_000
"""A fixed epoch anchor. Nothing here derives a time from the machine."""


def build(
    *,
    cases: int = 120,
    seed: int = 11,
    turns_per_session: int = 12,
    gold_per_case: int = 3,
    abstention_fraction: float = 0.25,
) -> Corpus:
    """A corpus with a known answer key and a controlled abstention share.

    `gold_per_case` defaults to 3 to match the stub's default injected-set size, and that
    is a calibration requirement rather than a taste: if a case carries fewer gold turns
    than the implementation injects, the gold pool runs dry mid-query and measured evidence
    precision is capped below the knob no matter what the knob says. The instrument would
    then read low against a correct implementation, which is the worst kind of wrong.
    """
    rng = stream(seed, "synthetic-corpus")
    sessions: list[SessionHistory] = []
    case_list: list[Case] = []

    for i in range(cases):
        sid = f"syn-s-{i:04d}"
        start = BASE_MS + i * DAY_MS
        turns: list[Turn] = []
        for t in range(turns_per_session):
            topic = _TOPICS[rng.randrange(len(_TOPICS))]
            fact = _FACTS[rng.randrange(len(_FACTS))]
            turns.append(
                Turn(
                    turn_id=f"{sid}-t{t:02d}",
                    speaker=Speaker.USER if t % 2 == 0 else Speaker.ASSISTANT,
                    text=f"{topic} {fact}.",
                    occurred_at_ms=start + t * HOUR_MS,
                    origin=Origin(channel=Channel.TERMINAL, actor="user:primary", ref=None),
                )
            )

        is_abstention = rng.random() < abstention_fraction
        gold = (
            ()
            if is_abstention
            else tuple(
                turns[idx].turn_id
                for idx in sorted(rng.sample(range(turns_per_session), gold_per_case))
            )
        )
        sessions.append(
            SessionHistory(session_id=sid, turns=tuple(turns), ingest_at_ms=start)
        )
        case_list.append(
            Case(
                query_id=f"syn-q-{i:04d}",
                session_id=sid,
                question=f"What do you know about {_TOPICS[i % len(_TOPICS)]}?",
                category="abstention" if is_abstention else "synthetic",
                gold_answer=None if is_abstention else "the recorded fact",
                gold_turn_ids=gold,
                ask_at_ms=start + (turns_per_session + 1) * HOUR_MS,
                is_abstention=is_abstention,
            )
        )

    return Corpus(
        name="synthetic",
        sessions=tuple(sessions),
        cases=tuple(case_list),
        categories=("synthetic", "abstention"),
    )
