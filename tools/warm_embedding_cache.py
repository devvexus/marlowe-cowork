"""Fill the embedding cache before a scoring run, so no cold ingest hits the 30 s deadline.

    python tools/warm_embedding_cache.py --heldout-only --reranking models/ms-marco-MiniLM-L-2-v2-ft-session-j

Why: on a cold cache one section 4.6 ingest embeds a whole LongMemEval haystack (~493 turns).
With few embedder workers some cases take longer than the harness's 30 s floor (section 4.0.7)
and the run aborts with `deadline_exceeded` before scoring anything.

What it does: runs `score_longmemeval.py` with the same arguments, with the harness deadline
lifted IN THIS PROCESS ONLY, into a throwaway output directory. Vectors are a pure function of
their text (content-addressed cache keyed on the model digests), so warming cannot change any
quality number. It does make the scoring run's retrieval latency a WARM read; measure cold
latency against an empty `--embedding-cache` instead.

Nothing this writes is a result. The output goes to data/runs/_warm and is not read by anything.
"""

from __future__ import annotations

import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(REPO / "eval" / "src"))
sys.path.insert(0, str(REPO / "tools"))

import marlowe_eval.adapter.subprocess_ndjson as transport  # noqa: E402
import score_longmemeval  # noqa: E402

WARM_OUT = REPO / "data" / "runs" / "_warm"


def main() -> int:
    if any(a == "--out" or a.startswith("--out=") for a in sys.argv[1:]):
        raise SystemExit("--out is fixed for a warm pass; its output is never a result")
    transport.DEADLINE_FLOOR_S = 6 * 3600.0
    sys.argv = [sys.argv[0], *sys.argv[1:], "--out", str(WARM_OUT)]
    print("WARM PASS: harness deadline lifted in this process; numbers below are NOT results.")
    return score_longmemeval.main()


if __name__ == "__main__":
    raise SystemExit(main())
