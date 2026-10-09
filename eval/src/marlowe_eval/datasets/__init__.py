"""Benchmark adapters and the corpora the harness runs against.

Reporting paths for LongMemEval-M, LongMemEval-V2 and BEAM are handled by the same
LongMemEval adapter plus `reporting.py`, which exists to keep "report honestly" from
quietly becoming "omit".
"""

from __future__ import annotations

from pathlib import Path

from . import locomo, longmemeval, reporting, synthetic, verify
from .errors import CorpusFormatError
from .model import Case, Corpus, SessionHistory

FIXTURE_DIR = Path(__file__).parent / "fixtures"


def fixture_longmemeval() -> Corpus:
    """The committed LongMemEval-shaped sample. Small, offline, and self-consistent only."""
    return longmemeval.load(FIXTURE_DIR / "longmemeval_s.sample.json", dataset="longmemeval-s")


def fixture_locomo() -> Corpus:
    return locomo.load(FIXTURE_DIR / "locomo.sample.json")


__all__ = [
    "FIXTURE_DIR",
    "Case",
    "Corpus",
    "CorpusFormatError",
    "SessionHistory",
    "fixture_locomo",
    "fixture_longmemeval",
    "locomo",
    "longmemeval",
    "reporting",
    "synthetic",
    "verify",
]
