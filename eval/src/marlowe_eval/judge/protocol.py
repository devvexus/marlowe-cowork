"""The judge interface and its verdict type.

HP1 permits an offline LLM judge at tier 2 and constrains it tightly: it exists for gate
training signal and for tracking between human label refreshes, and *"its agreement rate
against the human set must be published alongside any number it produces."*

This package implements that constraint as machinery. Nothing here can emit an
injection-precision figure on its own -- see agreement.py, where the only type that carries
a judge-derived precision requires an agreement measurement to construct.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Protocol

from ..labels.schema import LabelPacket


@dataclass(frozen=True)
class Verdict:
    """One judgment. Deliberately the same shape a human label reduces to."""

    packet_id: str
    relevant: bool
    judge_id: str
    rationale: str = ""
    cached: bool = False


class Judge(Protocol):
    """Anything that can label a blinded packet.

    Takes a `LabelPacket` -- the blinded type -- and not a `SampleDraw`. The judge is held
    to the same blinding as the human: it never sees the gate score, the decile, or whether
    the memory was actually injected.
    """

    judge_id: str

    def judge(self, packet: LabelPacket) -> Verdict: ...


class JudgeUnavailable(RuntimeError):
    """The judge cannot run, and that is a refusal rather than a fallback.

    Raised at CONSTRUCTION, not at call time, so a misconfigured run fails before it
    produces numbers rather than after.
    """
