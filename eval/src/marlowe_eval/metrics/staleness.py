"""Staleness half-life.

Section 5.7 defines it as *"time before a superseded fact stops being retrieved"*. HP4 calls
silent staleness genuinely unsolved and names this measurement as the resolving experiment,
so the number matters even -- especially -- when it is bad.

The measurement is only possible because of section 4.5. Write fact A at T1, write its
replacement A' at T2, query at a sweep of T3 values: all three times must be controlled, and
an implementation that read a system clock would make the sweep meaningless while still
returning plausible results. That is why the clock probe is a sibling of this module rather
than an afterthought.

Reported honestly when it cannot be measured. If the superseded fact never falls below half
its initial retrieval rate inside the sweep horizon, the answer is "not observed within N
days", not a number extrapolated off the end of the data.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any, Sequence


@dataclass(frozen=True)
class StalenessPoint:
    delta_ms: int
    queries: int
    superseded_still_injected: int

    @property
    def rate(self) -> float:
        return (self.superseded_still_injected / self.queries) if self.queries else 0.0


@dataclass(frozen=True)
class StalenessCurve:
    points: tuple[StalenessPoint, ...]
    half_life_ms: int | None
    horizon_ms: int

    def as_dict(self) -> dict[str, Any]:
        return {
            "metric": "staleness_half_life",
            "half_life_ms": self.half_life_ms,
            "half_life_days": (
                round(self.half_life_ms / 86_400_000, 4)
                if self.half_life_ms is not None
                else None
            ),
            "measured": self.half_life_ms is not None,
            "note": (
                ""
                if self.half_life_ms is not None
                else f"the superseded fact did not fall to half its initial retrieval rate "
                f"within the {round(self.horizon_ms / 86_400_000, 2)}-day sweep horizon; "
                f"reported as unmeasured rather than extrapolated"
            ),
            "horizon_ms": self.horizon_ms,
            "curve": [
                {
                    "delta_ms": p.delta_ms,
                    "delta_days": round(p.delta_ms / 86_400_000, 4),
                    "queries": p.queries,
                    "superseded_still_injected": p.superseded_still_injected,
                    "rate": round(p.rate, 6),
                }
                for p in self.points
            ],
        }


def fit(points: Sequence[StalenessPoint]) -> StalenessCurve:
    """Half-life by linear interpolation between the bracketing sweep points.

    No curve family is assumed. Fitting an exponential to five points and quoting its
    parameter would report a decay model the data does not establish; interpolation reports
    only where the observed rate crossed half.
    """
    ordered = tuple(sorted(points, key=lambda p: p.delta_ms))
    horizon = ordered[-1].delta_ms if ordered else 0
    if not ordered:
        return StalenessCurve(points=(), half_life_ms=None, horizon_ms=0)

    initial = ordered[0].rate
    if initial <= 0.0:
        # Never retrieved even at delta 0: supersession was immediate. Half-life is zero,
        # not unmeasured -- an important distinction for the knowledge-update category.
        return StalenessCurve(points=ordered, half_life_ms=0, horizon_ms=horizon)

    target = initial / 2.0
    previous = ordered[0]
    for point in ordered[1:]:
        if point.rate <= target:
            span = point.delta_ms - previous.delta_ms
            drop = previous.rate - point.rate
            if drop <= 0:
                return StalenessCurve(ordered, point.delta_ms, horizon)
            frac = (previous.rate - target) / drop
            return StalenessCurve(
                ordered, int(previous.delta_ms + frac * span), horizon
            )
        previous = point

    return StalenessCurve(points=ordered, half_life_ms=None, horizon_ms=horizon)
