"""Suites: the things that actually drive an implementation.

  benchmark    a corpus end to end, through ingest and answer
  conformance  does it obey section 4, on all three interfaces
  clock        does it obey section 4.5, and can we tell
  staleness    the half-life sweep
  poisoning    MINJA / MemoryGraft / laundering / delayed trigger / unsigned write
"""

from . import benchmark, clock, conformance, poisoning, scenarios, staleness

__all__ = [
    "benchmark",
    "clock",
    "conformance",
    "poisoning",
    "scenarios",
    "staleness",
]
