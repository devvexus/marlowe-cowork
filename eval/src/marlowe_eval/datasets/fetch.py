"""Corpus acquisition, with integrity pinned.

Two rules:

  * **Never vendored.** The corpora are large and separately licensed; the repo carries a
    manifest and tiny fixtures, not the data.
  * **Fails on mismatch. Never warns.** A silently-different corpus produces numbers that
    look fine and are not comparable to anything, which is the exact failure mode M0a exists
    to prevent in other people's published results.

Where a URL is not pinned, this module says so and refuses rather than guessing one. An
invented download URL is worse than no download URL: it either 404s or, much worse, fetches
something plausible.
"""

from __future__ import annotations

import hashlib
from dataclasses import dataclass
from pathlib import Path


@dataclass(frozen=True)
class DatasetSpec:
    name: str
    version: str
    filename: str
    sha256: str | None
    url: str | None
    source_note: str
    expected_cases: int | None = None


MANIFEST: dict[str, DatasetSpec] = {
    "longmemeval-s": DatasetSpec(
        name="longmemeval-s",
        version="cleaned",
        filename="longmemeval_s_cleaned.json",
        sha256="d6f21ea9d60a0d56f34a05b609c79c88a451d2ae03597821ea3d5a9678c3a442",
        url=None,
        source_note=(
            "LongMemEval-S, **cleaned** variant. This is the maintainer's own published "
            "replacement, not a third-party fork: xiaowu0162/longmemeval carries a "
            "deprecation notice pointing to it, and the change removes noisy history "
            "sessions that interfered with answer correctness. Same author, upgrade not "
            "fork.\n"
            "Verified 2026-08-02 by `marlowe-eval verify-corpus`: 500 questions, 500 "
            "sessions, 246,750 turns, 30 abstention, all seven categories populated.\n"
            "COMPARABILITY: results differ by roughly 0.5-2 pp between the original and "
            "cleaned variants, so a comparison against any published LongMemEval-S number "
            "is invalid unless that number's variant is known -- and many vendor reports "
            "do not state it. Ours always states `cleaned`."
        ),
        expected_cases=500,
    ),
    "longmemeval-m": DatasetSpec(
        name="longmemeval-m",
        version="UNPINNED",
        filename="longmemeval_m.json",
        sha256=None,
        url=None,
        source_note="Same source as longmemeval-s. Reported honestly, not gated on.",
        expected_cases=500,
    ),
    "locomo": DatasetSpec(
        name="locomo",
        version="UNPINNED",
        filename="locomo10.json",
        sha256=None,
        url=None,
        source_note=(
            "LoCoMo is released by its authors via their public repository. Download the "
            "conversation+QA JSON manually, then pin its sha256 here."
        ),
        expected_cases=1540,
    ),
}
"""Versions and hashes are UNPINNED until someone downloads a corpus and pins them.

That is a deliberate hole with a loud edge, not an oversight: this session had no verified
release URL or digest to record, and writing a plausible-looking one would defeat the
integrity check it pretends to be.
"""


class IntegrityError(Exception):
    """The file on disk is not the file the manifest pins."""


def sha256_of(path: Path) -> str:
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def verify_file(path: Path, spec: DatasetSpec) -> str:
    """Return the file's digest, or raise. Never returns on a mismatch."""
    path = Path(path)
    if not path.exists():
        raise IntegrityError(f"{spec.name}: no file at {path}")

    actual = sha256_of(path)
    if spec.sha256 is None:
        raise IntegrityError(
            f"{spec.name}: manifest carries no pinned sha256, so this file cannot be "
            f"verified.\n"
            f"  computed: {actual}\n"
            f"  If this download is trusted, pin that digest and the release version in "
            f"marlowe_eval/datasets/fetch.py, then re-run. Scoring against an unverified "
            f"corpus produces numbers that are not comparable to anyone else's."
        )
    if actual != spec.sha256:
        raise IntegrityError(
            f"{spec.name}: digest mismatch.\n"
            f"  expected: {spec.sha256}\n"
            f"  actual:   {actual}\n"
            f"  The corpus on disk is not the pinned release."
        )
    return actual


def fetch(name: str, dest_dir: Path) -> Path:
    spec = MANIFEST[name]
    dest = Path(dest_dir) / spec.filename
    if spec.url is None:
        raise IntegrityError(
            f"{spec.name}: no download URL is pinned.\n  {spec.source_note}\n"
            f"  Place the file at {dest} and run `marlowe-eval verify-corpus`."
        )
    raise NotImplementedError(  # pragma: no cover - unreachable until a URL is pinned
        "download path is written once a real URL and digest are pinned"
    )
