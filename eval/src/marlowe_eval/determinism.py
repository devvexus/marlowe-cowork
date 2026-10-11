"""Seeded randomness and the synthetic clock.

Two rules the whole harness rests on:

  1. Nothing here or anywhere else in `marlowe_eval` reads a system clock. A test scans the
     package AST for `time.time`, `datetime.now` and friends. Section 4.5 makes this binding
     for the implementation; it is just as binding for the harness, because a harness that
     reads a wall clock cannot verify that the implementation does not.

  2. Every random draw comes from a phase-derived stream. Adding a new phase must not
     perturb the draws of existing ones, or every past run's hash silently stops
     reproducing and nobody knows whether the change or the code did it.
"""

from __future__ import annotations

import hashlib
import random
from dataclasses import dataclass


def stream(root_seed: int, phase: str) -> random.Random:
    """A Random derived from (root_seed, phase).

    Derivation rather than sequential consumption: `stream(7, "sampler")` returns the same
    generator whether or not `stream(7, "poisoning")` was ever created. That independence is
    what lets a suite be added without invalidating the reproducibility of the others.
    """
    digest = hashlib.blake2b(
        f"{root_seed}:{phase}".encode("utf-8"), digest_size=32
    ).digest()
    return random.Random(int.from_bytes(digest, "big"))


@dataclass(frozen=True)
class SyntheticClock:
    """The only source of time on any path reachable from sections 4.1, 4.6 and 4.7.

    Frozen and explicit: time moves when a caller says it moves, and `advance` returns a new
    clock rather than mutating this one, so a probe cannot accidentally leak its time travel
    into the scenario that follows it.
    """

    now_ms: int

    def advance(self, ms: int) -> SyntheticClock:
        if ms < 0:
            raise ValueError("the synthetic clock does not run backwards")
        return SyntheticClock(self.now_ms + ms)

    def at(self, now_ms: int) -> SyntheticClock:
        return SyntheticClock(now_ms)


MINUTE_MS = 60_000
HOUR_MS = 60 * MINUTE_MS
DAY_MS = 24 * HOUR_MS
WEEK_MS = 7 * DAY_MS
