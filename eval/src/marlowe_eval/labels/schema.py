"""Label-set types, split so that blinding is a property of the type system.

Two types where a lesser design would have one with a `show_score=False` flag:

  `SampleDraw`   internal. Carries the gate score, the decile, and whether the memory was
                 actually injected. Never serialized to a label file.
  `LabelPacket`  what a human sees. Query, memory text, packet id. **There is no score
                 field, no decile field and no injected flag** -- not hidden ones, absent
                 ones.

The sampler stratifies by gate-score decile, so it must know the score. The presentation
layer must not, or the judgments get anchored to the very thing they exist to validate. A
boolean on one object would put both facts one attribute access apart; two objects put a
serialization boundary between them, and a test asserts nothing leaks across it.

ROADMAP.md's blinding requirement in full: *"The judge sees query + injected memory, never
the score or whether it was injected."* The second clause is why packets mix genuinely
injected memories with decoys drawn from the same session.
"""

from __future__ import annotations

import json
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any


@dataclass(frozen=True)
class SampleDraw:
    """Internal. Never written to a label file."""

    packet_id: str
    query_id: str
    memory_id: str
    category: str
    score: float
    calibrated_precision: float
    decile: str
    was_injected: bool


@dataclass(frozen=True)
class LabelPacket:
    """What the human judge is shown. Everything on it is safe to show."""

    packet_id: str
    query: str
    memory: str

    def as_dict(self) -> dict[str, str]:
        return {"packet_id": self.packet_id, "query": self.query, "memory": self.memory}


@dataclass(frozen=True)
class HumanLabel:
    packet_id: str
    relevant: bool
    judged_by: str = ""
    notes: str = ""


@dataclass
class HumanLabelSet:
    """The human's deliverable, not the agent's.

    ROADMAP.md is explicit: >=400 judged injections, >=50 per LongMemEval category,
    stratified by category and gate-score decile, judge blinded. And: *"Injection precision
    may not be validated against agent-generated relevance labels."* Nothing in this
    repository writes one of these files.
    """

    label_set_id: str
    labels: dict[str, HumanLabel] = field(default_factory=dict)

    def __len__(self) -> int:
        return len(self.labels)

    def get(self, packet_id: str) -> HumanLabel | None:
        return self.labels.get(packet_id)

    @classmethod
    def load(cls, path: Path) -> HumanLabelSet:
        raw = json.loads(Path(path).read_text(encoding="utf-8"))
        labels = {
            str(entry["packet_id"]): HumanLabel(
                packet_id=str(entry["packet_id"]),
                relevant=bool(entry["relevant"]),
                judged_by=str(entry.get("judged_by", "")),
                notes=str(entry.get("notes", "")),
            )
            for entry in raw["labels"]
        }
        return cls(label_set_id=str(raw.get("label_set_id", path.stem)), labels=labels)

    def coverage_warnings(self, draws: list[SampleDraw]) -> tuple[str, ...]:
        """What is thin about this label set, stated on every number it produces.

        ROADMAP.md's sampling rule exists so the calibration curve has support across its
        range *"not just the confident head"*. A label set that is large but concentrated
        in two deciles cannot support a calibrated claim, and silence about that would let
        a headline number look better sourced than it is.
        """
        warnings: list[str] = []
        labelled = [d for d in draws if d.packet_id in self.labels]
        if len(labelled) < 400:
            warnings.append(
                f"{len(labelled)} judged injections; ROADMAP.md sets the target at >=400"
            )

        by_category: dict[str, int] = {}
        by_decile: dict[str, int] = {}
        for draw in labelled:
            by_category[draw.category] = by_category.get(draw.category, 0) + 1
            by_decile[draw.decile] = by_decile.get(draw.decile, 0) + 1

        thin_categories = sorted(k for k, v in by_category.items() if v < 50)
        if thin_categories:
            warnings.append(
                f"fewer than 50 labels in: {', '.join(thin_categories)} "
                "(ROADMAP.md sets >=50 per category)"
            )
        empty_deciles = sorted(set(_ALL_DECILES) - set(by_decile))
        if empty_deciles:
            warnings.append(
                f"no labels in gate-score deciles {', '.join(empty_deciles)}; the "
                "calibration curve has no support there"
            )
        return tuple(warnings)


_ALL_DECILES = tuple(f"{i/10:.1f}-{(i+1)/10:.1f}" for i in range(10))


def write_packets(packets: list[LabelPacket], path: Path) -> None:
    """Write the blinded packet file a human judges from.

    Deliberately minimal. Everything the sampler knew and this file does not is the point.
    """
    payload: dict[str, Any] = {
        "instructions": (
            "For each packet: would a careful person say this memory is relevant to this "
            "query? Answer relevant true or false. You are not being shown a score, a "
            "rank, or whether the system actually used this memory -- that is deliberate."
        ),
        "packets": [p.as_dict() for p in packets],
    }
    Path(path).write_text(
        json.dumps(payload, indent=2, ensure_ascii=False, sort_keys=True) + "\n",
        encoding="utf-8",
    )
