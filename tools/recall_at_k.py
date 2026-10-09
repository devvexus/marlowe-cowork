"""Held-out R@k from one scoring pass: the numbers the reproduction is judged on.

    python tools/recall_at_k.py data/runs/longmemeval/heldout

Reads the pass's own `run.jsonl` (for the turn -> memory join, through the harness's
`Attributor`) and `scored-candidates.ndjson` (the binary's dump). Nothing is re-scored.

Population, as every published R@k was computed (old `tools/reach_pools.py`): non-abstention
cases with at least one gold memory in the dump. On the held-out split that is n = 229.

Ranking is `retrieve.rs`'s five-level key, read off the dump's own columns:
survived pruning, reranked before unreranked, rerank logit desc, cue z desc, cue margin desc,
memory id asc. R@k = fraction of cases whose best gold memory sits at rank <= k.

Driver-side, not a harness metric.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(REPO / "eval" / "src"))

from marlowe_eval.datasets import longmemeval  # noqa: E402
from marlowe_eval.datasets.model import Corpus  # noqa: E402
from marlowe_eval.metrics.records import Attributor  # noqa: E402

KS = (1, 3, 5, 10)
SPLIT_PATH = REPO / "tools" / "split.json"
PUBLISHED_HELDOUT = {"n": 229, "R@1": 0.6725, "R@3": 0.8515, "R@5": 0.8865, "R@10": 0.9039}


def iter_ndjson(path: Path):
    # File iteration, never splitlines(): transcripts contain U+2028/U+2029.
    with path.open("r", encoding="utf-8", newline="\n") as fh:
        for line in fh:
            line = line.strip()
            if line:
                yield json.loads(line)


def shipped_key(row: dict) -> tuple:
    reranked = row.get("rerank_score") is not None
    return (
        0 if row["survived_pruning"] else 1,
        0 if reranked else 1,
        -(row["rerank_score"] if reranked else 0.0),
        -row["score"],
        -row["margin"],
        row["memory_id"],
    )


def best_gold_ranks(pass_dir: Path, corpus: Corpus) -> dict[str, int]:
    """query_id -> 1-based rank of the best gold memory, over the scored population."""
    attributor = Attributor()
    for frame in iter_ndjson(pass_dir / "run.jsonl"):
        body = frame.get("body")
        if frame.get("op") == "ingest" and isinstance(body, dict) and "written" in body:
            for written in body["written"]:
                attributor.record(written["turn_id"], list(written["memory_ids"]))

    rows: dict[str, list[dict]] = {}
    for row in iter_ndjson(pass_dir / "scored-candidates.ndjson"):
        for required in ("score", "margin", "survived_pruning", "memory_id"):
            if required not in row:
                raise SystemExit(f"dump row has no {required!r}; not a gated --eval-adapter dump")
        rows.setdefault(row["query_id"], []).append(row)

    gold_map = corpus.gold_map()
    cases = {c.query_id: c for c in corpus.cases}
    ranks: dict[str, int] = {}
    for qid, cands in rows.items():
        case = cases.get(qid)
        if case is None or case.is_abstention:
            continue
        gold = gold_map.get(qid, frozenset())
        is_gold = [attributor.attribute(c["memory_id"], gold)[0] == "gold" for c in cands]
        if not any(is_gold):
            continue  # no gold survived to the dump: excluded, as in every published read
        order = sorted(range(len(cands)), key=lambda i: shipped_key(cands[i]))
        ranks[qid] = next(r for r, i in enumerate(order, 1) if is_gold[i])
    return ranks


def recall_at_k(pass_dir: Path, corpus: Corpus) -> dict:
    ranks = best_gold_ranks(pass_dir, corpus)
    n = len(ranks)
    out = {
        "_what": "driver-side R@k on the shipped ranking key, read from the binary's own dump",
        "pass_dir": str(pass_dir),
        "n": n,
    }
    for k in KS:
        out[f"R@{k}"] = round(sum(1 for r in ranks.values() if r <= k) / n, 4) if n else None
    return out


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("pass_dir", type=Path, help="a pass directory holding run.jsonl and scored-candidates.ndjson")
    ap.add_argument("--corpus", type=Path, default=None, help="default: tools/split.json corpus_path")
    args = ap.parse_args()
    corpus_path = args.corpus or REPO / json.loads(SPLIT_PATH.read_text(encoding="utf-8"))["corpus_path"]
    result = recall_at_k(args.pass_dir, longmemeval.load(corpus_path))
    print(json.dumps(result, indent=2))
    print(f"published held-out: {PUBLISHED_HELDOUT}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
