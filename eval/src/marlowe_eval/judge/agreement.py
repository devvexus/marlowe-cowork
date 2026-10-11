"""Agreement against the human label set -- and the only path to a judge-derived number.

ROADMAP.md and HP1 both say the same thing from different angles: the offline judge is
permitted for tracking, and *its agreement rate must be published alongside any number it
produces*. This module is where that stops being a rule someone has to remember.

`JudgedPrecision` has a non-optional `agreement` field and a constructor that refuses an
agreement computed from zero overlapping labels. There is no code path that produces a
judge precision without one, and no renderer that can print the value without the pair --
because the value only exists inside the pair. Same treatment as `cost` in section 4.2b.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any, Sequence

from ..labels.schema import HumanLabelSet, SampleDraw
from ..metrics.precision import decile_of
from .protocol import Verdict


@dataclass(frozen=True)
class Agreement:
    """How often the judge and the human said the same thing."""

    n: int
    raw_agreement: float
    cohens_kappa: float
    judge_positive_rate: float
    human_positive_rate: float
    by_decile: dict[str, dict[str, float]]

    def as_dict(self) -> dict[str, Any]:
        return {
            "n": self.n,
            "raw_agreement": round(self.raw_agreement, 6),
            "cohens_kappa": round(self.cohens_kappa, 6),
            "judge_positive_rate": round(self.judge_positive_rate, 6),
            "human_positive_rate": round(self.human_positive_rate, 6),
            "by_decile": {
                k: {kk: round(vv, 6) for kk, vv in v.items()}
                for k, v in sorted(self.by_decile.items())
            },
        }


class NoOverlap(RuntimeError):
    """The judge and the human label set share no packets.

    Raised rather than reporting agreement over an empty set, because an agreement of 0/0
    rendered as 1.0 would be the most flattering possible lie.
    """


def compute_agreement(
    verdicts: Sequence[Verdict], labels: HumanLabelSet, draws: Sequence[SampleDraw]
) -> Agreement:
    by_packet = {d.packet_id: d for d in draws}
    pairs: list[tuple[str, bool, bool]] = []
    for verdict in verdicts:
        human = labels.get(verdict.packet_id)
        if human is None:
            continue
        pairs.append((verdict.packet_id, verdict.relevant, human.relevant))

    if not pairs:
        raise NoOverlap(
            f"the judge produced {len(verdicts)} verdicts and the label set holds "
            f"{len(labels)} labels, but they share no packet ids; agreement is undefined"
        )

    n = len(pairs)
    both_yes = sum(1 for _, j, h in pairs if j and h)
    both_no = sum(1 for _, j, h in pairs if not j and not h)
    judge_yes = sum(1 for _, j, _ in pairs if j)
    human_yes = sum(1 for _, _, h in pairs if h)

    observed = (both_yes + both_no) / n
    # Cohen's kappa: agreement corrected for what chance alone would produce.
    p_yes = (judge_yes / n) * (human_yes / n)
    p_no = ((n - judge_yes) / n) * ((n - human_yes) / n)
    expected = p_yes + p_no
    kappa = 1.0 if expected >= 1.0 else (observed - expected) / (1.0 - expected)

    per_decile: dict[str, list[int]] = {}
    for packet_id, j, h in pairs:
        draw = by_packet.get(packet_id)
        decile = draw.decile if draw else "unknown"
        per_decile.setdefault(decile, []).append(1 if j == h else 0)

    return Agreement(
        n=n,
        raw_agreement=observed,
        cohens_kappa=kappa,
        judge_positive_rate=judge_yes / n,
        human_positive_rate=human_yes / n,
        by_decile={
            k: {"agreement": sum(v) / len(v), "n": float(len(v))}
            for k, v in per_decile.items()
            if v
        },
    )


@dataclass(frozen=True)
class JudgedPrecision:
    """A judge-derived injection precision. Unconstructible without its agreement.

    Note the name. There is no `injection_precision` field anywhere in this harness; the
    human-judged headline, the evidence proxy and this are three separately named numbers,
    so no one can fill the headline in with whichever they happened to have.
    """

    value: float
    judged: int
    relevant: int
    judge_id: str
    agreement: Agreement

    def __post_init__(self) -> None:
        if self.agreement.n <= 0:
            raise ValueError(
                "a judge-derived precision requires agreement against the human label "
                "set; HP1 permits this number only when its agreement is published with it"
            )

    def as_dict(self) -> dict[str, Any]:
        return {
            "metric": "injection_precision_judge",
            "value": round(self.value, 6),
            "note": (
                "offline LLM judge (HP1 tier 2). Permitted for gate training signal and "
                "for tracking between human label refreshes ONLY. This is not the K1 "
                "headline; see injection_precision_human."
            ),
            "judged": self.judged,
            "relevant": self.relevant,
            "judge_id": self.judge_id,
            "agreement_vs_human": self.agreement.as_dict(),
        }


def judged_precision(
    verdicts: Sequence[Verdict],
    labels: HumanLabelSet,
    draws: Sequence[SampleDraw],
    *,
    judge_id: str,
) -> JudgedPrecision:
    """The only constructor. Computes agreement first and refuses without overlap.

    Injected memories only: decoys carry no gate score and were never injected, so counting
    them would measure something other than injection precision.
    """
    agreement = compute_agreement(verdicts, labels, draws)
    injected = {d.packet_id for d in draws if d.was_injected}
    scored = [v for v in verdicts if v.packet_id in injected]
    relevant = sum(1 for v in scored if v.relevant)
    return JudgedPrecision(
        value=(relevant / len(scored)) if scored else 0.0,
        judged=len(scored),
        relevant=relevant,
        judge_id=judge_id,
        agreement=agreement,
    )
