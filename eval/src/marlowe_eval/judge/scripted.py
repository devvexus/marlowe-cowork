"""A deterministic judge, for testing the judge machinery without an API.

Not a relevance model and not a stand-in for one. It exists so the cache, the agreement
computation, the blinding test and the report path can all be exercised in CI, offline, at
a fixed seed. Any number it produces is meaningless as a relevance measurement, which is
why its `judge_id` says so out loud -- a stray `scripted-*` id in a published report is
then self-identifying rather than plausible.
"""

from __future__ import annotations

import re

from ..labels.schema import LabelPacket
from .protocol import Verdict

_WORD = re.compile(r"[a-z0-9]+")
_STOP = frozenset(
    "a an the of to in on at is was were are be do did does what when where which who "
    "you your my i it its this that and or for with from as by not no yes about know".split()
)


def _content_words(text: str) -> set[str]:
    return {w for w in _WORD.findall(text.lower()) if w not in _STOP}


class ScriptedJudge:
    """Calls a memory relevant when it shares content words with the query."""

    judge_id = "scripted-overlap-v1 (NOT A RELEVANCE MODEL; test double only)"

    def __init__(self, *, threshold: int = 1) -> None:
        self.threshold = threshold

    def judge(self, packet: LabelPacket) -> Verdict:
        overlap = _content_words(packet.query) & _content_words(packet.memory)
        return Verdict(
            packet_id=packet.packet_id,
            relevant=len(overlap) >= self.threshold,
            judge_id=self.judge_id,
            rationale=f"shared terms: {sorted(overlap)[:5]}",
        )
