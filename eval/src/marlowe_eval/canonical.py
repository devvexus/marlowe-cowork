"""Canonical JSON and the reproduction hash.

The acceptance criterion is that a run at a fixed seed and clock reproduces bit-identically.
That cannot hold over the whole report: `cost.latency_ms` is self-reported by the
implementation and varies between runs by nature, and the harness's own wall-clock
observations do too.

So the report is two layers, and the split is DECLARED here rather than fudged at comparison
time:

  * deterministic core -- injected sets, ids, scores, answers, verdicts, token counts, trust
    classes, abstentions. Must hash-match.
  * timing observations -- recorded, reported and scored, and excluded from the hash by the
    allowlist below.

There are no tolerance windows anywhere. If a field is not on this allowlist and it varies
between two runs at the same seed and clock, that is a failure, not a wobble.
"""

from __future__ import annotations

import hashlib
import json
from typing import Any

TIMING_ALLOWLIST: frozenset[str] = frozenset(
    {
        # implementation self-reported, section 4.2 / 4.6 / 4.7 `cost.latency_ms`
        "latency_ms",
    }
)
"""Keys stripped before hashing, wherever they appear. Nothing else is excluded.

It is one entry because the harness core takes no wall-clock measurement of its own: latency
is self-reported by the implementation under section 4.2, and the P95 metric scores that
number. The only component that will need a real clock is the subprocess adapter's
hung-process deadline, which is deferred until section 4.0 is pinned; when it lands, the
run-level `timing_tainted` flag it sets is a scored fact, not an allowlisted one.

Adding to this list widens what the reproduction claim does not cover, so an addition is a
weakening of the acceptance criterion and belongs in a decision record, not in a commit that
also does something else.
"""


def strip_timing(node: Any) -> Any:
    """Remove allowlisted timing from a structure, recursively."""
    if isinstance(node, dict):
        return {
            k: strip_timing(v)
            for k, v in node.items()
            if k not in TIMING_ALLOWLIST
        }
    if isinstance(node, list):
        return [strip_timing(v) for v in node]
    return node


def dumps(node: Any) -> str:
    """Canonical JSON: sorted keys, no incidental whitespace, UTF-8 preserved."""
    return json.dumps(node, sort_keys=True, ensure_ascii=False, separators=(",", ":"))


def pretty(node: Any) -> str:
    """Human-readable canonical JSON, for the artifacts a third party reads."""
    return json.dumps(node, sort_keys=True, ensure_ascii=False, indent=2) + "\n"


def core_hash(report: Any) -> str:
    """sha256 over the deterministic core. This is what `marlowe-eval repro` compares."""
    payload = dumps(strip_timing(report)).encode("utf-8")
    return hashlib.sha256(payload).hexdigest()
