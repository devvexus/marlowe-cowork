"""The blinded, stratified sampler.

Three leaks it has to close, and only the first is obvious:

  1. **The score itself.** Closed by `LabelPacket` having no score field at all.
  2. **Position.** Draw stratified by decile and emit in draw order, and the decile is
     readable off the row number. Closed by a deterministic shuffle under the run seed,
     with packet ids assigned *after* shuffling so the id encodes nothing either.
  3. **Whether it was injected.** ROADMAP.md's blinding rule names this explicitly. Closed
     by mixing decoys -- memories from the same session that were not injected for that
     query -- so that "this is in front of me" carries no information about what the system
     did.

Runnable today against the reference stub, whose synthetic scores spread across the range
and populate every decile. That is the point of running it early: the way to find out
whether blinding actually holds is to produce a real packet file and look at the bytes,
before anyone's judgments depend on it.
"""

from __future__ import annotations

from collections import defaultdict
from dataclasses import dataclass
from typing import Any, Iterable, Sequence

from ..determinism import stream
from ..metrics.precision import decile_of
from ..metrics.records import QueryRecord
from .schema import LabelPacket, SampleDraw


@dataclass(frozen=True)
class SamplePlan:
    draws: tuple[SampleDraw, ...]
    packets: tuple[LabelPacket, ...]
    decoy_fraction: float
    strata: dict[str, int]

    def as_dict(self) -> dict[str, Any]:
        return {
            "packets": len(self.packets),
            "injected": sum(1 for d in self.draws if d.was_injected),
            "decoys": sum(1 for d in self.draws if not d.was_injected),
            "decoy_fraction": round(self.decoy_fraction, 4),
            "strata": dict(sorted(self.strata.items())),
            "blinding": (
                "packets carry query and memory text only; no score, no decile, no "
                "injected flag, and packet order is shuffled under the run seed"
            ),
        }


def draw(
    records: Sequence[QueryRecord],
    *,
    seed: int,
    decoy_pool: dict[str, list[tuple[str, str]]],
    questions: dict[str, str],
    per_stratum: int = 6,
    decoy_fraction: float = 0.35,
) -> SamplePlan:
    """Stratify by (category, gate-score decile), then blind.

    `decoy_pool` maps session_id -> [(memory_id, content)] for memories the harness knows
    exist. `questions` maps query_id -> question text.
    """
    rng = stream(seed, "label-sampler")

    # -- stratify. This is where the score is used, and the last place it appears. ----
    strata: dict[tuple[str, str], list[SampleDraw]] = defaultdict(list)
    for record in records:
        for item in record.injected:
            key = (record.category, decile_of(item.calibrated_precision))
            strata[key].append(
                SampleDraw(
                    packet_id="",
                    query_id=record.query_id,
                    memory_id=item.memory_id,
                    category=record.category,
                    score=item.score,
                    calibrated_precision=item.calibrated_precision,
                    decile=key[1],
                    was_injected=True,
                )
            )

    selected: list[SampleDraw] = []
    for key in sorted(strata):
        bucket = sorted(strata[key], key=lambda d: (d.query_id, d.memory_id))
        take = min(per_stratum, len(bucket))
        selected.extend(rng.sample(bucket, take))

    # -- decoys, so "shown to you" says nothing about "used by the system" -----------
    injected_by_query: dict[str, set[str]] = defaultdict(set)
    for record in records:
        for item in record.injected:
            injected_by_query[record.query_id].add(item.memory_id)

    want_decoys = int(len(selected) * decoy_fraction / max(1e-9, 1 - decoy_fraction))
    candidates: list[SampleDraw] = []
    by_session = {r.query_id: r.session_id for r in records}
    for record in records:
        pool = decoy_pool.get(record.session_id, [])
        for memory_id, _content in pool:
            if memory_id in injected_by_query[record.query_id]:
                continue
            candidates.append(
                SampleDraw(
                    packet_id="",
                    query_id=record.query_id,
                    memory_id=memory_id,
                    category=record.category,
                    # Decoys have no gate score. Zero here is a placeholder that never
                    # reaches a packet and never enters a precision computation; decoys are
                    # excluded from the injected-precision numerator and denominator alike.
                    score=0.0,
                    calibrated_precision=0.0,
                    decile="decoy",
                    was_injected=False,
                )
            )
    candidates.sort(key=lambda d: (d.query_id, d.memory_id))
    if candidates and want_decoys:
        selected.extend(rng.sample(candidates, min(want_decoys, len(candidates))))

    # -- blind: shuffle first, THEN name. An id assigned before the shuffle would carry
    # -- the stratification order in its digits.
    rng.shuffle(selected)
    contents = {
        memory_id: content
        for pool in decoy_pool.values()
        for memory_id, content in pool
    }
    for record in records:
        for item in record.injected:
            contents.setdefault(item.memory_id, item.content)

    draws: list[SampleDraw] = []
    packets: list[LabelPacket] = []
    for index, item in enumerate(selected):
        packet_id = f"p-{index:05d}"
        draws.append(
            SampleDraw(
                packet_id=packet_id,
                query_id=item.query_id,
                memory_id=item.memory_id,
                category=item.category,
                score=item.score,
                calibrated_precision=item.calibrated_precision,
                decile=item.decile,
                was_injected=item.was_injected,
            )
        )
        packets.append(
            LabelPacket(
                packet_id=packet_id,
                query=questions.get(item.query_id, ""),
                memory=contents.get(item.memory_id, ""),
            )
        )

    actual_decoys = sum(1 for d in draws if not d.was_injected)
    return SamplePlan(
        draws=tuple(draws),
        packets=tuple(packets),
        decoy_fraction=(actual_decoys / len(draws)) if draws else 0.0,
        strata={
            f"{cat}|{dec}": len(v) for (cat, dec), v in sorted(strata.items())
        },
    )


def decoy_pool_from(
    attributor_pairs: Iterable[tuple[str, str, str]],
) -> dict[str, list[tuple[str, str]]]:
    """Build session_id -> [(memory_id, content)] from (session_id, memory_id, content)."""
    pool: dict[str, list[tuple[str, str]]] = defaultdict(list)
    for session_id, memory_id, content in attributor_pairs:
        pool[session_id].append((memory_id, content))
    for entries in pool.values():
        entries.sort()
    return dict(pool)
