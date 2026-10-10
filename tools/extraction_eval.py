"""Extraction eval: html.rs baseline vs main-content candidates on WCXB and ScrapingHub.

    python tools/extraction_eval.py fetch            # clone both benchmarks at pinned commits into data/extraction/
    python tools/extraction_eval.py run   --label L  # build + run the Rust harness, then score
    python tools/extraction_eval.py score --label L  # rescore an existing raw.jsonl

Outputs go to data/runs/extraction-eval/<label>/: manifest.tsv, raw.jsonl (harness output),
pages.jsonl (per page x candidate scores) and summary.json (aggregates + pass-bar verdicts).

Metrics:
- token P/R/F1: WCXB's own word_f1 (lowercased \\w+ multiset overlap), macro over pages.
- WCXB with/without: fraction of required snippets present / boilerplate snippets present.
- ScrapingHub shingle P/R/F1: that benchmark's 4-gram shingle metric, normalised per page.
- link recall (anchor proxy): links from html.rs's page link list whose anchor text sits inside
  the ground-truth main content are the gold set; a link is kept when its anchor text survives in
  the candidate's output. Pages with no gold link are skipped.
- thin/empty rate: output under 250 chars (main_content::THIN_CHARS) / no output or a failure.
- p50/p95/max ms: wall clock per call, single-threaded harness.

Pass bar to switch the default (M4.5 E1), against the baseline on WCXB test:
F1 >= baseline + 0.08, forum F1 >= 0.75, no page type regresses by more than 0.02, p95 <= 50 ms.
"""

from __future__ import annotations

import argparse
import json
import math
import re
import subprocess
import sys
from collections import Counter, defaultdict
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DATA = ROOT / "data" / "extraction"
RUNS = ROOT / "data" / "runs" / "extraction-eval"

# Pinned upstream commits (fetched 2026-10-09). WCXB is CC-BY-4.0; ScrapingHub's benchmark is MIT.
SOURCES = {
    "wcxb": ("https://github.com/Murrough-Foley/web-content-extraction-benchmark.git",
             "c039d5ee9f5a3a984a0e167e63aacd04e76e78a9"),
    "scrapinghub": ("https://github.com/scrapinghub/article-extraction-benchmark.git",
                    "4a3bc979f76c0df73cb95fe272e2fc1b96f9f010"),
}

THIN_CHARS = 250
PAGE_TYPES = ["article", "forum", "product", "collection", "listing", "documentation", "service"]
BAR = {"f1_gain": 0.08, "forum_f1": 0.75, "max_type_regression": 0.02, "p95_ms": 50.0}
HARNESS = ROOT / "target" / "release" / "examples" / "extract_eval.exe"

_TOKEN = re.compile(r"\w+")


def tokens(text: str) -> list[str]:
    return _TOKEN.findall((text or "").lower())


def word_prf(pred: str, ref: str) -> tuple[float, float, float]:
    """WCXB evaluate.py word_f1, verbatim semantics."""
    p, r = tokens(pred), tokens(ref)
    if not r:
        return (1.0, 1.0, 1.0) if not p else (0.0, 0.0, 0.0)
    if not p:
        return (0.0, 0.0, 0.0)
    overlap = sum((Counter(p) & Counter(r)).values())
    prec, rec = overlap / len(p), overlap / len(r)
    f1 = 2 * prec * rec / (prec + rec) if prec + rec > 0 else 0.0
    return prec, rec, f1


def snippet_rate(text: str, snippets: list[str]) -> float | None:
    if not snippets:
        return None
    low = (text or "").lower()
    return sum(1 for s in snippets if s.lower() in low) / len(snippets)


def shingle_tp_fp_fn(true: str, pred: str, n: int = 4) -> tuple[float, float, float]:
    """ScrapingHub evaluate.py string_shingle_matching (case-sensitive \\w+ tokens)."""
    def grams(t: str) -> Counter:
        toks = _TOKEN.findall(t or "")
        return Counter(tuple(toks[i:i + n]) for i in range(0, max(1, len(toks) - n + 1)) if toks[i:i + n])
    ts, ps = grams(true), grams(pred)
    tp = fp = fn = 0.0
    for k in set(ts) | set(ps):
        a, b = ts.get(k, 0), ps.get(k, 0)
        tp += min(a, b)
        fp += max(0, b - a)
        fn += max(0, a - b)
    s = tp + fp + fn
    return (tp / s, fp / s, fn / s) if s > 0 else (0.0, 0.0, 0.0)


def shingle_prf(rows: list[tuple[float, float, float]]) -> dict:
    def prec(tp, fp, fn):
        return 1.0 if fp == fn == 0 else (0.0 if tp == fp == 0 else tp / (tp + fp))

    def rec(tp, fp, fn):
        return 1.0 if fp == fn == 0 else (0.0 if tp == fn == 0 else tp / (tp + fn))
    ps = [prec(*x) for x in rows if x[0] + x[1] > 0]
    rs = [rec(*x) for x in rows if x[0] + x[2] > 0]
    p = sum(ps) / len(ps) if ps else 0.0
    r = sum(rs) / len(rs) if rs else 0.0
    return {"precision": p, "recall": r, "f1": 2 * p * r / (p + r) if p + r else 0.0}


def anchor_key(text: str) -> str | None:
    t = tokens(text)
    joined = " ".join(t)
    if len(t) >= 2 or len(joined) >= 6:
        return f" {joined} "
    return None


def link_recall(links: list[list[str]], gt: str, pred: str) -> tuple[int, int]:
    """(kept, gold) over distinct URLs whose anchor text lies inside the ground truth."""
    gt_s = f" {' '.join(tokens(gt))} "
    pred_s = f" {' '.join(tokens(pred))} "
    gold: dict[str, str] = {}
    for url, text in links:
        key = anchor_key(text)
        if key and key in gt_s and url not in gold:
            gold[url] = key
    kept = sum(1 for key in gold.values() if key in pred_s)
    return kept, len(gold)


def url_link_recall(links: list[list[str]], gt: str, markdown: str) -> tuple[int, int]:
    """(kept, gold) where kept means the gold URL itself is inline in the markdown as `](url)`."""
    gt_s = f" {' '.join(tokens(gt))} "
    gold = set()
    for url, text in links:
        key = anchor_key(text)
        if key and key in gt_s:
            gold.add(url)
    enc = lambda u: u.replace(" ", "%20").replace("(", "%28").replace(")", "%29")
    kept = sum(1 for u in gold if f"]({enc(u)})" in markdown)
    return kept, len(gold)


def pct(values: list[float], q: float) -> float:
    if not values:
        return float("nan")
    v = sorted(values)
    k = max(0, min(len(v) - 1, math.ceil(q * len(v)) - 1))
    return v[k]


# ── datasets ─────────────────────────────────────────────────────────────────────────────────

def git_head(path: Path) -> str | None:
    try:
        return subprocess.run(["git", "-C", str(path), "rev-parse", "HEAD"], capture_output=True,
                              text=True, check=True).stdout.strip()
    except (subprocess.CalledProcessError, FileNotFoundError):
        return None


def fetch() -> None:
    DATA.mkdir(parents=True, exist_ok=True)
    for name, (url, sha) in SOURCES.items():
        dest = DATA / name
        if not dest.exists():
            subprocess.run(["git", "clone", "--filter=blob:none", url, str(dest)], check=True)
        if git_head(dest) != sha:
            subprocess.run(["git", "-C", str(dest), "fetch", "--depth", "1", "origin", sha], check=True)
            subprocess.run(["git", "-C", str(dest), "checkout", "--detach", sha], check=True)
        head = git_head(dest)
        print(f"{name}: {head} {'ok' if head == sha else 'MISMATCH (expected ' + sha + ')'}")


def load_ground_truth() -> dict[str, dict]:
    """id -> {dataset, split, page_type, url, main_content, with, without, html}."""
    gt: dict[str, dict] = {}
    wcxb = DATA / "wcxb"
    for split in ("dev", "test"):
        for f in sorted((wcxb / split / "ground-truth").glob("*.json")):
            d = json.loads(f.read_text(encoding="utf-8"))
            g = d.get("ground_truth") or {}
            pt = ((d.get("_internal") or {}).get("page_type") or {})
            pt = pt.get("primary", "article") if isinstance(pt, dict) else (pt or "article")
            gt[f"wcxb-{split}-{f.stem}"] = {
                "dataset": "wcxb", "split": split, "page_type": "collection" if pt == "category" else pt,
                "url": d.get("url") or "", "main_content": g.get("main_content") or "",
                "with": g.get("with") or [], "without": g.get("without") or [],
                "html": wcxb / split / "html" / f"{f.stem}.html.gz",
            }
    sh = DATA / "scrapinghub"
    if (sh / "ground-truth.json").exists():
        for key, v in json.loads((sh / "ground-truth.json").read_text(encoding="utf-8")).items():
            gt[f"sh-{key}"] = {
                "dataset": "scrapinghub", "split": "all", "page_type": "article", "url": v.get("url") or "",
                "main_content": v.get("articleBody") or "", "with": [], "without": [],
                "html": sh / "html" / f"{key}.html.gz",
            }
    return gt


def write_manifest(gt: dict, path: Path) -> int:
    n = 0
    with path.open("w", encoding="utf-8", newline="\n") as f:
        for pid, g in gt.items():
            if g["html"].exists():
                url = g["url"].replace("\t", " ").replace("\n", " ")
                f.write(f"{pid}\t{url}\t{g['html']}\n")
                n += 1
    return n


# ── scoring ──────────────────────────────────────────────────────────────────────────────────

def score(out: Path, gt: dict) -> dict:
    links: dict[str, list] = {}
    runs: dict[str, dict[str, dict]] = defaultdict(dict)
    with (out / "raw.jsonl").open(encoding="utf-8") as f:
        for line in f:
            r = json.loads(line)
            if r["kind"] == "page":
                links[r["id"]] = r["links"]
            else:
                runs[r["candidate"]][r["id"]] = r

    per_page = []
    for cand, by_id in runs.items():
        for pid, r in by_id.items():
            g = gt.get(pid)
            if g is None:
                continue
            text = r["text"] if r["ok"] else ""
            p, rc, f1 = word_prf(text, g["main_content"])
            kept, gold = link_recall(links.get(pid, []), g["main_content"], text)
            row = {
                "id": pid, "candidate": cand, "dataset": g["dataset"], "split": g["split"],
                "page_type": g["page_type"], "ok": r["ok"], "error": r.get("error"),
                "chars": r["chars"], "ms": r["ms"], "precision": p, "recall": rc, "f1": f1,
                "with": snippet_rate(text, g["with"]), "without": snippet_rate(text, g["without"]),
                "links_kept": kept, "links_gold": gold, "chosen": r.get("chosen"),
                "predicted_type": r.get("page_type"),
            }
            md = r.get("markdown")
            if md is not None:
                row["url_kept"], row["url_gold"] = url_link_recall(links.get(pid, []), g["main_content"], md)
                row["structured"] = bool(r.get("structured"))
                row["coverage"] = r.get("coverage")
                row["client_rendered"] = bool(r.get("client_rendered"))
            if g["dataset"] == "scrapinghub":
                row["shingle"] = shingle_tp_fp_fn(g["main_content"], text)
            per_page.append(row)

    def agg(rows: list[dict]) -> dict:
        ok = [r for r in rows if r["ok"]]
        n = len(rows)
        if not n:
            return {"n": 0}
        lr = [r["links_kept"] / r["links_gold"] for r in rows if r["links_gold"]]
        ms = [r["ms"] for r in ok]
        w = [r["with"] for r in rows if r["with"] is not None]
        wo = [r["without"] for r in rows if r["without"] is not None]
        a = {
            "n": n, "failed": n - len(ok),
            "precision": sum(r["precision"] for r in rows) / n,
            "recall": sum(r["recall"] for r in rows) / n,
            "f1": sum(r["f1"] for r in rows) / n,
            "with_rate": sum(w) / len(w) if w else None,
            "without_rate": sum(wo) / len(wo) if wo else None,
            "link_recall": sum(lr) / len(lr) if lr else None,
            "link_recall_micro": (sum(r["links_kept"] for r in rows) / max(1, sum(r["links_gold"] for r in rows))),
            "link_pages": len(lr),
            "thin_rate": sum(1 for r in rows if r["chars"] < THIN_CHARS) / n,
            "empty_rate": sum(1 for r in rows if r["chars"] == 0) / n,
            "p50_ms": pct(ms, 0.50), "p95_ms": pct(ms, 0.95), "max_ms": max(ms) if ms else float("nan"),
        }
        md = [r for r in rows if "url_gold" in r]
        if md:
            ur = [r["url_kept"] / r["url_gold"] for r in md if r["url_gold"]]
            a["url_link_recall"] = sum(ur) / len(ur) if ur else None
            a["structured_rate"] = sum(1 for r in md if r["structured"]) / len(md)
            a["client_rendered_rate"] = sum(1 for r in md if r["client_rendered"]) / len(md)
        sh = [r["shingle"] for r in rows if "shingle" in r]
        if sh:
            a["shingle"] = shingle_prf(sh)
        chosen = Counter(r["chosen"] for r in rows if r.get("chosen"))
        if chosen:
            a["chosen"] = dict(chosen)
        return a

    summary: dict = {"candidates": {}, "sources": {k: v[1] for k, v in SOURCES.items()}}
    for cand in sorted(runs):
        rows = [r for r in per_page if r["candidate"] == cand]
        unavailable = all((not r["ok"]) and r["error"] == "unavailable" for r in rows)
        c: dict = {"unavailable": unavailable}
        for label, sel in (("wcxb_test", lambda r: r["dataset"] == "wcxb" and r["split"] == "test"),
                           ("wcxb_dev", lambda r: r["dataset"] == "wcxb" and r["split"] == "dev"),
                           ("scrapinghub", lambda r: r["dataset"] == "scrapinghub")):
            sub = [r for r in rows if sel(r)]
            block = {"all": agg(sub)}
            if label.startswith("wcxb"):
                block["by_type"] = {pt: agg([r for r in sub if r["page_type"] == pt]) for pt in PAGE_TYPES}
            c[label] = block
        c["latency_all"] = {k: agg(rows)[k] for k in ("p50_ms", "p95_ms", "max_ms")} if rows else {}
        summary["candidates"][cand] = c
    summary["verdicts"] = verdicts(summary)

    with (out / "pages.jsonl").open("w", encoding="utf-8") as f:
        for r in per_page:
            f.write(json.dumps(r) + "\n")
    (out / "summary.json").write_text(json.dumps(summary, indent=2), encoding="utf-8")
    return summary


def verdicts(summary: dict) -> dict:
    base = summary["candidates"].get("baseline")
    if not base:
        return {}
    bt = base["wcxb_test"]
    out = {}
    for cand, c in summary["candidates"].items():
        if cand == "baseline" or c["unavailable"]:
            continue
        t = c["wcxb_test"]
        gain = t["all"]["f1"] - bt["all"]["f1"]
        forum = t["by_type"]["forum"].get("f1", 0.0)
        regress = {pt: bt["by_type"][pt]["f1"] - t["by_type"][pt]["f1"]
                   for pt in PAGE_TYPES if t["by_type"][pt].get("n") and bt["by_type"][pt].get("n")}
        worst = max(regress.items(), key=lambda kv: kv[1]) if regress else (None, 0.0)
        # The bar is read on WCXB test like its other checks; the all-pages p95 is the A budget.
        p95 = t["all"].get("p95_ms", float("inf"))
        p95_all = c["latency_all"].get("p95_ms", float("inf"))
        checks = {
            "f1_gain": (round(gain, 4), gain >= BAR["f1_gain"]),
            "forum_f1": (round(forum, 4), forum >= BAR["forum_f1"]),
            "worst_type_regression": (worst[0], round(worst[1], 4), worst[1] <= BAR["max_type_regression"]),
            "p95_ms_wcxb_test": (round(p95, 2), p95 <= BAR["p95_ms"]),
        }
        out[cand] = {"checks": checks, "pass": all(v[-1] for v in checks.values()),
                     "p95_ms_all_pages": (round(p95_all, 2), p95_all <= BAR["p95_ms"])}
    return out


def print_summary(s: dict) -> None:
    def f(x, d=3):
        return "  -  " if x is None or (isinstance(x, float) and math.isnan(x)) else f"{x:.{d}f}"
    for cand, c in s["candidates"].items():
        if c["unavailable"]:
            print(f"\n{cand}: not compiled into this build")
            continue
        print(f"\n{cand}")
        for label in ("wcxb_test", "wcxb_dev", "scrapinghub"):
            a = c[label]["all"]
            if not a.get("n"):
                continue
            extra = ""
            if "shingle" in a:
                extra = f"  shingleF1 {f(a['shingle']['f1'])}"
            if "url_link_recall" in a:
                extra += (f"  md-url-links {f(a['url_link_recall'])}  structured {f(a['structured_rate'])}"
                          f"  spa {f(a['client_rendered_rate'])}")
            print(f"  {label:<12} n={a['n']:<5} F1 {f(a['f1'])}  P {f(a['precision'])}  R {f(a['recall'])}"
                  f"  with {f(a['with_rate'])}  without {f(a['without_rate'])}  links {f(a['link_recall'])}"
                  f"  thin {f(a['thin_rate'])}  empty {f(a['empty_rate'])}"
                  f"  p50 {f(a['p50_ms'], 2)}ms  p95 {f(a['p95_ms'], 2)}ms{extra}")
            for pt, b in (c[label].get("by_type") or {}).items():
                if b.get("n"):
                    print(f"    {pt:<14} n={b['n']:<4} F1 {f(b['f1'])}  P {f(b['precision'])}  R {f(b['recall'])}"
                          f"  links {f(b['link_recall'])}  thin {f(b['thin_rate'])}  p95 {f(b['p95_ms'], 2)}ms")
        lat = c["latency_all"]
        print(f"  latency (all pages): p50 {f(lat.get('p50_ms'), 2)}ms  p95 {f(lat.get('p95_ms'), 2)}ms"
              f"  max {f(lat.get('max_ms'), 1)}ms")
    for cand, v in s.get("verdicts", {}).items():
        print(f"\npass bar for {cand}: {'PASS' if v['pass'] else 'FAIL'}  {v['checks']}"
              f"  all-pages p95 budget {v['p95_ms_all_pages']}")


def run_harness(out: Path, threads: int, limit: int | None, build: bool) -> None:
    if build:
        subprocess.run(["powershell", "-NoProfile", "-File", str(ROOT / "scripts" / "cargo-idle.ps1"), "build",
                        "--release", "--example", "extract_eval", "--package", "marlowe-extract", "-j", "2"],
                       check=True, cwd=ROOT)
    cmd = [str(HARNESS), "--manifest", str(out / "manifest.tsv"), "--out", str(out / "raw.jsonl"),
           "--threads", str(threads)]
    if limit:
        cmd += ["--limit", str(limit)]
    subprocess.run(cmd, check=True, cwd=ROOT)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("command", choices=["fetch", "run", "score"])
    ap.add_argument("--label", default="baseline")
    ap.add_argument("--threads", type=int, default=1)
    ap.add_argument("--limit", type=int)
    ap.add_argument("--no-build", action="store_true")
    a = ap.parse_args()
    if a.command == "fetch":
        fetch()
        return 0
    for name, (_, sha) in SOURCES.items():
        if git_head(DATA / name) != sha:
            print(f"{name} is missing or not at {sha}; run `python tools/extraction_eval.py fetch`", file=sys.stderr)
            return 2
    gt = load_ground_truth()
    out = RUNS / a.label
    out.mkdir(parents=True, exist_ok=True)
    if a.command == "run":
        n = write_manifest(gt, out / "manifest.tsv")
        print(f"manifest: {n} pages")
        run_harness(out, a.threads, a.limit, not a.no_build)
    s = score(out, gt)
    print_summary(s)
    print(f"\nwrote {out / 'summary.json'}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
