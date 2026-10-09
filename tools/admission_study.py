"""Admission study: an absolute rank-1 floor and a rank-2..3 band on the shipped rerank logits.

    # 1. collect slates through the shipped pipeline (a DEBUG marlowe.exe; never the release one)
    python tools/admission_study.py collect --split fit
    python tools/admission_study.py collect --split heldout --max-cases 40
    # 2. choose t and d on fit, read them once on the held-out slice, freeze the artifact
    python tools/admission_study.py analyze

Collection reuses `score_longmemeval.score_one` unchanged: the same harness, the same frames,
the same `--eval-adapter` flags and the same gate-feature dump. Only three things differ from a
scoring run, and none can move a logit:
  * the binary is `target/debug/marlowe.exe` (same source, unoptimised);
  * the embedding cache is a private copy (`data/runs/admission-study/cache`), so this process
    never appends to the cache another run is writing;
  * the harness deadline is lifted in this process, as `warm_embedding_cache.py` does, because a
    debug build may exceed 30 s on a cold ingest.

Analysis reads each pass's dump and transcript, joins gold through the harness `Attributor`, and
orders candidates with `recall_at_k.shipped_key` (the five-level key in `retrieve.rs`).
"""

from __future__ import annotations

import argparse
import hashlib
import json
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(REPO / "eval" / "src"))
sys.path.insert(0, str(REPO / "tools"))

import marlowe_eval.adapter.subprocess_ndjson as transport  # noqa: E402
import score_longmemeval as slm  # noqa: E402
from marlowe_eval.datasets import longmemeval  # noqa: E402
from marlowe_eval.metrics.records import Attributor  # noqa: E402
from recall_at_k import iter_ndjson, shipped_key  # noqa: E402

OUT = REPO / "data" / "runs" / "admission-study"
DEBUG_BINARY = REPO / "target" / "debug" / "marlowe.exe"
ARTIFACT = REPO / "crates" / "marlowe-memory" / "artifacts" / "admission-floor-v1.json"
FLOOR_TARGET = 0.90
BAND_TARGET = 0.80
BAND_RANKS = (2, 3)


# ---------------------------------------------------------------- collection


def collect(split_name: str, max_cases: int | None, cache: Path) -> None:
    if DEBUG_BINARY.resolve() == slm.BINARY.resolve():
        raise SystemExit("refusing: the study never drives the release binary")
    split = json.loads(slm.SPLIT_PATH.read_text(encoding="utf-8"))
    corpus = longmemeval.load(REPO / split["corpus_path"])
    ids = sorted(split[split_name])
    if max_cases is not None:
        ids = ids[:max_cases]  # first N in sorted id order: deterministic, chosen before reading
    transport.DEADLINE_FLOOR_S = 6 * 3600.0
    slm.BINARY = DEBUG_BINARY.resolve()
    slm._cache_dir = cache
    slm._reranking = slm.SHIPPED_RERANKER
    out = OUT / split_name
    out.mkdir(parents=True, exist_ok=True)
    slm._record_binary_identity(out)
    print(f"collecting {split_name}: {len(ids)} cases -> {out}")
    slm.score_one(slm.subset(corpus, set(ids), f"-{split_name}"), out, 7, 1_780_000_000_000)
    (out / "CASES.json").write_text(json.dumps({"split": split_name, "query_ids": ids}, indent=2) + "\n")


# ---------------------------------------------------------------- slates


def slates(pass_dir: Path, corpus) -> list[dict]:
    """One row per answerable case in the pass: the reranked slate in shipped order, with gold."""
    attributor = Attributor()
    for frame in iter_ndjson(pass_dir / "run.jsonl"):
        body = frame.get("body")
        if frame.get("op") == "ingest" and isinstance(body, dict) and "written" in body:
            for written in body["written"]:
                attributor.record(written["turn_id"], list(written["memory_ids"]))
    rows: dict[str, list[dict]] = {}
    for row in iter_ndjson(pass_dir / "scored-candidates.ndjson"):
        rows.setdefault(row["query_id"], []).append(row)
    cases = {c.query_id: c for c in corpus.cases}
    gold_map = corpus.gold_map()
    asked = json.loads((pass_dir / "CASES.json").read_text())["query_ids"]
    out = []
    for qid in asked:
        case = cases[qid]
        cands = sorted(rows.get(qid, []), key=shipped_key)
        gold = gold_map.get(qid, frozenset())
        slate = [
            {
                "logit": c["rerank_score"],
                "gold": attributor.attribute(c["memory_id"], gold)[0] == "gold",
            }
            for c in cands
            if c["survived_pruning"] and c["rerank_score"] is not None
        ]
        out.append(
            {
                "query_id": qid,
                "category": case.category,
                "abstention": case.is_abstention,
                "gold_in_dump": any(attributor.attribute(c["memory_id"], gold)[0] == "gold" for c in cands),
                "slate": slate,
            }
        )
    return out


# ---------------------------------------------------------------- statistics


def clopper_pearson(k: int, n: int, alpha: float = 0.05) -> list[float | None]:
    from scipy.stats import beta

    if n == 0:
        return [None, None]
    lo = 0.0 if k == 0 else float(beta.ppf(alpha / 2, k, n - k + 1))
    hi = 1.0 if k == n else float(beta.ppf(1 - alpha / 2, k + 1, n - k))
    return [round(lo, 4), round(hi, 4)]


def floor_curve(cases: list[dict]) -> list[dict]:
    """Precision of rank 1 over {logit1 >= v}, for every observed rank-1 logit v (descending)."""
    pts = sorted(((c["slate"][0]["logit"], c["slate"][0]["gold"]) for c in cases if c["slate"]), reverse=True)
    curve, k = [], 0
    for i, (v, g) in enumerate(pts, 1):
        k += g
        curve.append({"logit": v, "admitted": i, "gold": k, "precision": k / i})
    return curve


def choose_floor(curve: list[dict], target: float) -> tuple[float | None, dict | None]:
    """Lowest cut (most coverage) whose cumulative precision is >= target: the task's rule read
    literally. The threshold sits midway between the chosen logit and the next lower one, so float
    noise cannot flip a fit admission. Support is NOT required here; it is reported beside it.
    """
    chosen = None
    for i, p in enumerate(curve):
        # Ties: only a cut at the last index of a tied run is a real cut.
        if i + 1 < len(curve) and curve[i + 1]["logit"] == p["logit"]:
            continue
        if p["precision"] >= target:
            chosen = i
    if chosen is None:
        return None, None
    v = curve[chosen]["logit"]
    nxt = curve[chosen + 1]["logit"] if chosen + 1 < len(curve) else v - 1.0
    return (v + nxt) / 2, curve[chosen]


def band_events(cases: list[dict], floor: float | None) -> list[tuple]:
    """(gap = logit1 - logit_k, gold_k, k, query_id, rank1_gold) for ranks 2..3, on cases whose
    rank 1 is admitted: by the floor when `floor` is given, else every case with a slate."""
    ev = []
    for c in cases:
        s = c["slate"]
        if not s or (floor is not None and s[0]["logit"] < floor):
            continue
        for k in BAND_RANKS:
            if len(s) >= k:
                ev.append((s[0]["logit"] - s[k - 1]["logit"], s[k - 1]["gold"], k, c["query_id"], s[0]["gold"]))
    return sorted(ev)


def choose_band(ev: list[tuple], target: float) -> tuple[float, dict]:
    """Largest d whose cumulative precision on {gap <= d} is >= target (the task's rule, literal).
    d = 0.0 with nothing admitted when no band reaches the target."""
    chosen, k = None, 0
    for i, e in enumerate(ev, 1):
        k += e[1]
        if i < len(ev) and ev[i][0] == e[0]:
            continue
        if k / i >= target:
            chosen = (i, k, e[0])
    if chosen is None:
        return 0.0, {"admitted": 0, "gold": 0, "precision": None}
    i, k, gap = chosen
    nxt = ev[i][0] if i < len(ev) else gap + 1.0
    return (gap + nxt) / 2, {"admitted": i, "gold": k, "precision": k / i}


def read_floor(cases: list[dict], t: float) -> dict:
    adm = [c for c in cases if c["slate"] and c["slate"][0]["logit"] >= t]
    k = sum(c["slate"][0]["gold"] for c in adm)
    return {
        "n": len(cases),
        "admitted": len(adm),
        "gold": k,
        "precision": round(k / len(adm), 4) if adm else None,
        "ci95": clopper_pearson(k, len(adm)),
        "coverage": round(len(adm) / len(cases), 4) if cases else None,
    }


def read_band(cases: list[dict], t: float | None, d: float) -> dict:
    ev = [e for e in band_events(cases, t) if e[0] <= d]
    k = sum(e[1] for e in ev)
    # A rescue: rank 1 is not gold and a band admission is, so the query gains its first gold.
    rescued = {e[3] for e in ev if e[1] and not e[4]}
    return {
        "admitted": len(ev),
        "gold": k,
        "precision": round(k / len(ev), 4) if ev else None,
        "ci95": clopper_pearson(k, len(ev)),
        "per_rank": {
            str(r): {"admitted": sum(1 for e in ev if e[2] == r), "gold": sum(1 for e in ev if e[2] == r and e[1])}
            for r in BAND_RANKS
        },
        "queries_touched": len({e[3] for e in ev}),
        "queries_rescued": len(rescued),
    }


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def auc(scores: list[float], labels: list[bool]) -> float | None:
    """P(score of a random positive > a random negative), ties half. Rank-sum form."""
    pos = [s for s, y in zip(scores, labels) if y]
    neg = [s for s, y in zip(scores, labels) if not y]
    if not pos or not neg:
        return None
    from scipy.stats import mannwhitneyu

    return round(float(mannwhitneyu(pos, neg).statistic) / (len(pos) * len(neg)), 4)


def declared_margin() -> float:
    art = REPO / "crates" / "marlowe-memory" / "artifacts" / "precision-coverage-heldout-v1.json"
    return float(json.loads(art.read_text(encoding="utf-8"))["declared_operating_point"]["margin_threshold"])


# ---------------------------------------------------------------- analysis


def analyze(write: bool) -> dict:
    split = json.loads(slm.SPLIT_PATH.read_text(encoding="utf-8"))
    corpus = longmemeval.load(REPO / split["corpus_path"])
    fit_all = slates(OUT / "fit", corpus)
    fit = [c for c in fit_all if not c["abstention"]]
    fit_abs = [c for c in fit_all if c["abstention"]]
    with_slate = [c for c in fit if c["slate"]]

    # ---- (a) floor, chosen on fit only
    curve = floor_curve(fit)
    t, at = choose_floor(curve, FLOOR_TARGET)
    if t is None:
        raise SystemExit("no rank-1 cut reaches the floor target on fit")
    floor_fit = read_floor(fit, t)
    supported = {}
    for min_n in (20, 30, 50):
        pts = [p for p in curve if p["admitted"] >= min_n]
        best = max(pts, key=lambda p: (p["precision"], p["admitted"]))
        supported[f"admitted>={min_n}"] = {
            "logit": round(best["logit"], 4),
            "admitted": best["admitted"],
            "gold": best["gold"],
            "precision": round(best["precision"], 4),
            "ci95": clopper_pearson(best["gold"], best["admitted"]),
            "coverage": round(best["admitted"] / len(fit), 4),
        }
    floor_attained = floor_fit["admitted"] >= 20

    # ---- (b) band, chosen on fit only. Primary: every case with a slate, as the rule is phrased.
    ev = band_events(fit, None)
    d, _ = choose_band(ev, BAND_TARGET)
    band_fit = read_band(fit, None, d)
    ev_c = band_events(fit, t)
    d_c, _ = choose_band(ev_c, BAND_TARGET)

    def band_table(events):
        rows = []
        for cut in (0.01, 0.025, 0.05, 0.075, 0.1, 0.25, 0.5, 1.0, 2.0):
            sel = [e for e in events if e[0] <= cut]
            k = sum(e[1] for e in sel)
            rows.append({"d": cut, "admitted": len(sel), "gold": k,
                         "precision": round(k / len(sel), 4) if sel else None,
                         "ci95": clopper_pearson(k, len(sel))})
        return rows

    def floor_table(cases):
        return [{"t": x, **{k: v for k, v in read_floor(cases, x).items() if k != "n"}}
                for x in (-5.0, -5.5, -6.0, -6.25, -6.5, -7.0, -7.5, -8.0)]

    # ---- context: the shipped margin gate on the same slates
    m_thr = declared_margin()
    gate = [c for c in with_slate if c["slate"][0]["logit"] - c["slate"][1]["logit"] >= m_thr]
    gate_k = sum(c["slate"][0]["gold"] for c in gate)
    l1 = [c["slate"][0]["logit"] for c in with_slate]
    mg = [c["slate"][0]["logit"] - c["slate"][1]["logit"] for c in with_slate]
    y1 = [c["slate"][0]["gold"] for c in with_slate]
    cand = [(x["logit"], x["gold"]) for c in with_slate for x in c["slate"]]
    gold_l1 = sorted(v for v, y in zip(l1, y1) if y)
    non_l1 = sorted(v for v, y in zip(l1, y1) if not y)

    def pct(xs, q):
        return round(xs[min(len(xs) - 1, int(q * len(xs)))], 3) if xs else None

    # ---- (c) held-out check: read once, chose nothing
    held = slates(OUT / "heldout", corpus) if (OUT / "heldout" / "CASES.json").exists() else []
    held_ans = [c for c in held if not c["abstention"]]
    held_abs = [c for c in held if c["abstention"]]
    hf = read_floor(held_ans, t) if held_ans else None
    hb = read_band(held_ans, None, d) if held_ans else None

    model = slm.SHIPPED_RERANKER / "model.onnx"
    result = {
        "version": 1,
        "split_digest": split["digest"],
        "n_fit": len(fit),
        "floor_logit": round(t, 4),
        "floor_precision_fit": floor_fit["precision"],
        "floor_ci": floor_fit["ci95"],
        "floor_coverage_fit": floor_fit["coverage"],
        "floor_status": "attained" if floor_attained else "target_not_attained_with_support",
        "band_delta": round(d, 4),
        "band_precision_fit": band_fit["precision"],
        "band_ci": band_fit["ci95"],
        "heldout_check": {
            "n": len(held_ans),
            "floor_precision": hf["precision"] if hf else None,
            "band_precision": hb["precision"] if hb else None,
        },
        "model_sha256": sha256(model),
        "_detail": {
            "what": (
                "Rank-1 floor and rank-2..3 band on the shipped cross-encoder logits "
                "(ms-marco-MiniLM-L-2-v2-ft-session-j), chosen on the FIT split of tools/split.json "
                "and read once on a held-out slice that chose nothing. Produced by "
                "tools/admission_study.py; see docs/research/admission-floor.md."
            ),
            "population": (
                "answerable fit cases (abstention cases excluded). A case whose gold turn never "
                "reaches the reranked slate counts as rank-1 not gold; coverage = admitted / n_fit."
            ),
            "floor_rule": (
                f"admit rank 1 iff logit1 >= floor_logit: the lowest cut whose cumulative fit "
                f"precision is >= {FLOOR_TARGET}, set midway to the next lower rank-1 logit."
            ),
            "floor_status_meaning": (
                "target_not_attained_with_support: the only cuts reaching the target admit fewer "
                "than 20 fit cases, so floor_logit is a degenerate, unsupported cut. "
                "floor_best_supported shows what the logit can actually deliver."
            ),
            "band_rule": (
                "with rank 1 admitted, admit rank k in {2,3} iff logit_k >= logit1 - band_delta: "
                f"the widest band whose pooled cumulative fit precision is >= {BAND_TARGET}, set "
                "midway to the next wider gap."
            ),
            "floor_fit": floor_fit,
            "floor_cut_point": at,
            "floor_best_supported": supported,
            "floor_table_fit": floor_table(fit),
            "band_fit": band_fit,
            "band_events_fit": len(ev),
            "band_table_fit": band_table(ev),
            "band_conditional_on_floor": {
                "events": len(ev_c),
                "delta": round(d_c, 4),
                "read": read_band(fit, t, d_c),
            },
            "separation_fit": {
                "auc_rank1_gold_by_logit1": auc(l1, y1),
                "auc_rank1_gold_by_margin": auc(mg, y1),
                "auc_candidate_gold_by_logit": auc([v for v, _ in cand], [y for _, y in cand]),
                "logit1_gold_p10_p50_p90": [pct(gold_l1, q) for q in (0.1, 0.5, 0.9)],
                "logit1_nongold_p10_p50_p90": [pct(non_l1, q) for q in (0.1, 0.5, 0.9)],
                "abstention_case_logit1": sorted(round(c["slate"][0]["logit"], 3) for c in fit_abs if c["slate"]),
            },
            "shipped_margin_gate_fit": {
                "margin_threshold": m_thr,
                "fires": len(gate),
                "rank1_gold": gate_k,
                "precision": round(gate_k / len(gate), 4) if gate else None,
                "ci95": clopper_pearson(gate_k, len(gate)),
                "coverage": round(len(gate) / len(fit), 4),
                "note": "when this gate fires logit2 <= logit1 - margin_threshold, so any band_delta "
                        "below margin_threshold admits no rank 2..3 alongside it",
            },
            "fit_cases_total": len(fit_all),
            "fit_gold_in_dump": sum(c["gold_in_dump"] for c in fit),
            "fit_gold_in_slate": sum(any(x["gold"] for x in c["slate"]) for c in fit),
            "fit_rank1_gold": sum(1 for c in with_slate if c["slate"][0]["gold"]),
            "fit_abstention_cases": len(fit_abs),
            "fit_abstention_admitted_by_floor": sum(1 for c in fit_abs if c["slate"] and c["slate"][0]["logit"] >= t),
            "heldout_check": {
                "label": "HELD-OUT CHECK: first 40 held-out ids in sorted order; read once, after "
                         "floor_logit and band_delta were frozen; chose nothing",
                "cases": len(held),
                "answerable": len(held_ans),
                "floor": hf,
                "band": hb,
                "abstention_cases": len(held_abs),
                "abstention_admitted_by_floor": sum(1 for c in held_abs if c["slate"] and c["slate"][0]["logit"] >= t),
            },
            "binary": json.loads((OUT / "fit" / "BINARY.json").read_text()),
            "corpus_sha256": split["corpus_sha256"],
        },
    }
    if write:
        ARTIFACT.write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
        print(f"wrote {ARTIFACT}")
    print(json.dumps(result, indent=2))
    return result


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    c = sub.add_parser("collect")
    c.add_argument("--split", choices=["fit", "heldout"], required=True)
    c.add_argument("--max-cases", type=int, default=None)
    c.add_argument("--embedding-cache", type=Path, default=OUT / "cache")
    a = sub.add_parser("analyze")
    a.add_argument("--no-write", action="store_true")
    args = ap.parse_args()
    if args.cmd == "collect":
        if args.split == "heldout" and (args.max_cases is None or args.max_cases > 40):
            raise SystemExit("the held-out read is a <= 40-case sanity check; pass --max-cases <= 40")
        collect(args.split, args.max_cases, args.embedding_cache)
    else:
        analyze(not args.no_write)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
