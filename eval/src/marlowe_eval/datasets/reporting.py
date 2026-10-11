"""Declared benchmarks, and why a number is missing.

ROADMAP.md requires "reporting paths" for LongMemEval-M, LongMemEval-V2 and BEAM-1M/10M, and
the brief is blunt about why: *a low score honestly reported beats a high score on a
saturated benchmark.*

The failure mode that guards against is not lying. It is omission -- a benchmark quietly
absent from a report because nobody had the corpus that week, which reads identically to a
benchmark that was never meant to be there. So every declared benchmark appears in every
report, and one that did not run carries the reason it did not.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any, Iterable


@dataclass(frozen=True)
class BenchmarkDeclaration:
    key: str
    title: str
    role: str
    """`gated` -- a target in ROADMAP acceptance. `reported` -- honest reporting only."""
    note: str


DECLARED: tuple[BenchmarkDeclaration, ...] = (
    BenchmarkDeclaration(
        "longmemeval-s", "LongMemEval-S (500 q)", "gated",
        "K2: >=90% overall, >=85% on the abstention subset",
    ),
    BenchmarkDeclaration(
        "longmemeval-s-abstention", "LongMemEval-S abstention subset", "gated",
        "Correctly declining on events that never happened",
    ),
    BenchmarkDeclaration(
        "locomo", "LoCoMo (1,540 q)", "gated",
        "Baseline only; the brief is explicit that it is not sufficient",
    ),
    BenchmarkDeclaration(
        "longmemeval-m", "LongMemEval-M", "reported",
        "The regime where context-stuffing fails entirely",
    ),
    BenchmarkDeclaration(
        "longmemeval-v2", "LongMemEval-V2", "reported",
        "100M+ token multimodal agent histories; no adapter yet",
    ),
    BenchmarkDeclaration(
        "beam-1m", "BEAM-1M", "reported",
        "Deliberately unsaturated; no adapter yet",
    ),
    BenchmarkDeclaration(
        "beam-10m", "BEAM-10M", "reported",
        "Deliberately unsaturated; no adapter yet",
    ),
)

_NO_ADAPTER = {"longmemeval-v2", "beam-1m", "beam-10m"}


def coverage(ran: Iterable[str]) -> list[dict[str, Any]]:
    """One row per declared benchmark. Never fewer."""
    ran_set = set(ran)
    rows: list[dict[str, Any]] = []
    for decl in DECLARED:
        if decl.key in ran_set:
            status, reason = "scored", ""
        elif decl.key in _NO_ADAPTER:
            status, reason = (
                "not_run",
                "no adapter in M0a; the corpus shape is not pinned in this repo",
            )
        else:
            status, reason = (
                "not_run",
                "corpus not present; run `marlowe-eval verify-corpus` after downloading it",
            )
        rows.append(
            {
                "key": decl.key,
                "title": decl.title,
                "role": decl.role,
                "note": decl.note,
                "status": status,
                "reason": reason,
            }
        )
    return rows
