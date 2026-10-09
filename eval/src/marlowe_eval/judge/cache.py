"""Verdict cache, keyed by content hash.

Not an optimization. An uncached LLM judge makes the run non-reproducible, and
"a run at a fixed seed and clock reproduces bit-identically" is an M0a acceptance
criterion -- so the cache is what lets a judged run be replayed at all.

The key is a hash of (judge id, prompt version, query, memory) and deliberately NOT the
packet id: the same (query, memory) pair drawn in a later sample reuses the verdict, and a
changed prompt invalidates every entry rather than silently mixing protocol versions.
"""

from __future__ import annotations

import hashlib
import json
from pathlib import Path

from ..labels.schema import LabelPacket
from .protocol import Verdict


def key_for(judge_id: str, prompt_version: str, packet: LabelPacket) -> str:
    payload = json.dumps(
        {
            "judge": judge_id,
            "prompt": prompt_version,
            "query": packet.query,
            "memory": packet.memory,
        },
        sort_keys=True,
        ensure_ascii=False,
    )
    return hashlib.sha256(payload.encode("utf-8")).hexdigest()


class VerdictCache:
    """A JSON file of hash -> verdict. Small, diffable, and committable."""

    def __init__(self, path: Path | None = None) -> None:
        self.path = Path(path) if path else None
        self._entries: dict[str, dict[str, object]] = {}
        if self.path and self.path.exists():
            self._entries = json.loads(self.path.read_text(encoding="utf-8"))

    def get(self, key: str, packet_id: str) -> Verdict | None:
        entry = self._entries.get(key)
        if entry is None:
            return None
        return Verdict(
            packet_id=packet_id,
            relevant=bool(entry["relevant"]),
            judge_id=str(entry["judge_id"]),
            rationale=str(entry.get("rationale", "")),
            cached=True,
        )

    def put(self, key: str, verdict: Verdict) -> None:
        self._entries[key] = {
            "relevant": verdict.relevant,
            "judge_id": verdict.judge_id,
            "rationale": verdict.rationale,
        }

    def save(self) -> None:
        if self.path is None:
            return
        self.path.parent.mkdir(parents=True, exist_ok=True)
        self.path.write_text(
            json.dumps(self._entries, indent=2, sort_keys=True, ensure_ascii=False) + "\n",
            encoding="utf-8",
        )

    def __len__(self) -> int:
        return len(self._entries)
