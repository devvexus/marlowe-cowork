"""Research eval: FACT-style citation accuracy plus key-point coverage for deep-research reports.

    python tools/research_eval.py score    --bundle B.json [--question q01]   # one report
    python tools/research_eval.py run      --bundles DIR --label L            # the 10-question set
    python tools/research_eval.py validate                                    # the two hand-made fixtures

A bundle (schema "marlowe.research-bundle/1", see tools/research_eval_fixtures/) is one run's
report plus the stored evidence for every reference:

    {"schema", "question_id", "question", "report_markdown",
     "references": [{"n", "title", "url", "identity", "passages": [{"text", "locator"}],
                     "document_text"?}],
     "verification"?: {"citations": [{"sentence", "n", "support"}]},   # the run's own check
     "channels_used"?: [...], "stats"?: {...}}

Citation accuracy follows FACT (DeepResearch Bench, Du et al. 2025): every (cited sentence,
reference) pair is extracted from the report, duplicates are merged, and a judge model decides
whether the reference's stored text supports the sentence. Accuracy = supported / judged pairs;
effective citations = distinct supported pairs per report. Unlike FACT, the evidence is the
passage the run stored (what VERIFY re-reads), not a fresh fetch, so the score is reproducible
offline. A marker naming no reference counts as unsupported.

The judge is a local Ollama model (default marlowe-dusk:27b-super, temperature 0). Verdicts are
cached by prompt digest under data/runs/research-eval/judge-cache/, so re-scoring is free and
deterministic. Live judging checks the GPU first and refuses when another job holds it.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import shlex
import subprocess
import sys
import time
import urllib.error
import urllib.request
from dataclasses import dataclass, field
from pathlib import Path
from typing import Callable

ROOT = Path(__file__).resolve().parent.parent
RUNS = ROOT / "data" / "runs" / "research-eval"
CACHE = RUNS / "judge-cache"
QUESTIONS = ROOT / "tools" / "research_questions.json"
FIXTURES = ROOT / "tools" / "research_eval_fixtures"

DEFAULT_JUDGE = "marlowe-dusk:27b-super"
OLLAMA = "http://127.0.0.1:11434"
TARGET_ACCURACY = 0.95  # ADR-037
EVIDENCE_CHARS = 6000
REPORT_CHARS = 24000
VERDICTS = ("supported", "partial", "unsupported")

# ── report parsing ───────────────────────────────────────────────────────────────────────────

# [1], [1, 3], [1-3], [1–3], [^2]; never a markdown link's [text](url).
_MARKER = re.compile(r"\[\^?(\d+(?:\s*(?:[,;]|[-–])\s*\^?\d+)*)\](?!\()")
_REF_HEADING = re.compile(r"^\s{0,3}#{1,6}\s*(references|sources|bibliography|works cited)\b", re.I)
_SENT_END = re.compile(r"(?<=[.!?])\s+(?=[\"“(\[A-Z0-9])")
_ABBREV = re.compile(r"\b(e\.g|i\.e|et al|cf|vs|approx|fig|eq|no|vol|pp|dr|mr|ms)\.$", re.I)


def _expand(group: str) -> list[int]:
    out: list[int] = []
    for part in re.split(r"\s*[,;]\s*", group.replace("^", "")):
        m = re.match(r"(\d+)\s*[-–]\s*(\d+)$", part)
        if m:
            a, b = int(m.group(1)), int(m.group(2))
            if 0 < b - a < 50:
                out.extend(range(a, b + 1))
                continue
        if part.isdigit():
            out.append(int(part))
    return out


def body_lines(markdown: str) -> list[str]:
    """Report lines that carry prose: no code, no table rules, nothing after the references heading."""
    out, in_code = [], False
    for line in markdown.splitlines():
        if line.strip().startswith("```"):
            in_code = not in_code
            continue
        if _REF_HEADING.match(line):
            break
        if in_code:
            continue
        s = line.strip()
        if not s or re.fullmatch(r"[|:\-\s]+", s):
            continue
        out.append(s)
    return out


def sentences(markdown: str) -> list[str]:
    """Prose sentences with their markers; a marker that opens a sentence belongs to the one before."""
    out: list[str] = []
    for line in body_lines(markdown):
        if line.startswith("#"):
            continue
        line = re.sub(r"^([-*+]|\d+\.)\s+", "", line).strip("| ")
        parts = _SENT_END.split(line)
        merged: list[str] = []
        for p in parts:
            if merged and _ABBREV.search(merged[-1]):
                merged[-1] += " " + p
            else:
                merged.append(p)
        for p in merged:
            lead = re.match(r"^((?:\s*\[\^?\d[^\]]*\])+)\s*(.*)$", p)
            if lead and out:
                out[-1] += " " + lead.group(1).strip()
                p = lead.group(2)
            if p.strip():
                out.append(p.strip())
    return out


def strip_markers(sentence: str) -> str:
    s = _MARKER.sub("", sentence)
    s = re.sub(r"\*\*|__|`", "", s)
    return re.sub(r"\s+([.,;:!?])", r"\1", re.sub(r"\s{2,}", " ", s)).strip()


@dataclass
class Pair:
    sentence: str  # markers stripped
    n: int


def cited_pairs(markdown: str) -> tuple[list[Pair], list[str]]:
    """Distinct (sentence, reference) pairs, and the substantive sentences that cite nothing."""
    pairs: list[Pair] = []
    seen: set[tuple[str, int]] = set()
    uncited: list[str] = []
    for s in sentences(markdown):
        ns = [n for g in _MARKER.findall(s) for n in _expand(g)]
        clean = strip_markers(s)
        if not ns:
            if len(clean.split()) >= 8:
                uncited.append(clean)
            continue
        for n in ns:
            key = (re.sub(r"\W+", " ", clean.lower()).strip(), n)
            if key not in seen:
                seen.add(key)
                pairs.append(Pair(clean, n))
    return pairs, uncited


# ── evidence ─────────────────────────────────────────────────────────────────────────────────

_WORD = re.compile(r"\w+")


def _overlap(a: str, b: set[str]) -> int:
    return len(set(_WORD.findall(a.lower())) & b)


def evidence_for(ref: dict, sentence: str, limit: int = EVIDENCE_CHARS) -> str:
    """Stored passages first, then the best-overlapping chunks of the stored document text."""
    parts: list[str] = []
    for p in ref.get("passages") or []:
        loc = f" ({p['locator']})" if p.get("locator") else ""
        parts.append(f"[passage{loc}] {p['text'].strip()}")
    doc = (ref.get("document_text") or "").strip()
    if doc:
        words = set(_WORD.findall(sentence.lower()))
        chunks = [doc[i:i + 1200] for i in range(0, len(doc), 1000)]
        chunks.sort(key=lambda c: -_overlap(c, words))
        for c in chunks[:3]:
            if c.strip() and all(c.strip()[:200] not in x for x in parts):
                parts.append(f"[document excerpt] {c.strip()}")
    text = "\n\n".join(parts)
    return text[:limit]


# ── judge ────────────────────────────────────────────────────────────────────────────────────

CITATION_SYSTEM = (
    "You check citations in research reports. You are given one STATEMENT from a report and the "
    "EVIDENCE stored for the source it cites. Decide whether the evidence supports the statement.\n"
    "- supported: every factual claim in the statement is stated in, or directly entailed by, the evidence.\n"
    "- partial: nothing in the statement conflicts with the evidence, but part of it is not covered, or the "
    "statement is more specific or more certain than the evidence.\n"
    "- unsupported: the evidence conflicts with any part of the statement, does not contain its main claim, "
    "or is about something else.\n"
    "A number, name, comparison or direction of effect that differs from the evidence is a conflict, so the "
    "verdict is unsupported, not partial. General background knowledge does not count as evidence. "
    "Reply with JSON only."
)

KEYPOINT_SYSTEM = (
    "You grade research reports against reviewer key points. Given a REPORT and one KEY POINT, decide "
    "whether the report conveys the key point (wording may differ).\n"
    "- covered: the report states the point or something that clearly entails it.\n"
    "- partial: the report states part of it, or only vaguely.\n"
    "- missing: the report does not state it.\n"
    "Set contradicted to true if the report asserts the opposite. Reply with JSON only."
)

CITATION_SCHEMA = {"type": "object", "properties": {"verdict": {"type": "string", "enum": list(VERDICTS)},
                                                    "reason": {"type": "string"}},
                   "required": ["verdict", "reason"]}
KEYPOINT_SCHEMA = {"type": "object", "properties": {"verdict": {"type": "string", "enum": ["covered", "partial", "missing"]},
                                                    "contradicted": {"type": "boolean"},
                                                    "reason": {"type": "string"}},
                   "required": ["verdict", "contradicted", "reason"]}

Judge = Callable[[str, str, dict], dict]


def gpu_busy() -> str | None:
    """A reason to back off when another job holds the GPU (Matthew's training runs), else None."""
    try:
        q = subprocess.run(["nvidia-smi", "--query-compute-apps=pid,process_name", "--format=csv,noheader"],
                           capture_output=True, text=True, timeout=20)
    except (FileNotFoundError, subprocess.TimeoutExpired):
        return None
    for line in q.stdout.splitlines():
        name = line.split(",", 1)[-1].strip().lower().replace("\\", "/").rsplit("/", 1)[-1]
        if any(k in name for k in ("python", "torchrun", "accelerate", "deepspeed", "jupyter")):
            return f"GPU in use by {line.strip()}"
    return None


def ollama_judge(model: str, url: str = OLLAMA, cache: Path = CACHE, check_gpu: bool = True) -> Judge:
    checked = {"done": not check_gpu}

    def judge(system: str, prompt: str, schema: dict) -> dict:
        body = {"model": model, "system": system, "prompt": prompt, "stream": False, "format": schema,
                "options": {"temperature": 0, "seed": 7, "num_ctx": 32768}, "keep_alive": "10m"}
        key = hashlib.sha256(json.dumps(body, sort_keys=True).encode()).hexdigest()
        path = cache / f"{key}.json"
        if path.exists():
            return json.loads(path.read_text(encoding="utf-8"))
        if not checked["done"]:
            busy = gpu_busy()
            if busy:
                raise SystemExit(f"backing off: {busy}. Re-run when the GPU is free (or --no-gpu-check).")
            checked["done"] = True
        req = urllib.request.Request(f"{url}/api/generate", data=json.dumps(body).encode(),
                                     headers={"Content-Type": "application/json"})
        t = time.time()
        with urllib.request.urlopen(req, timeout=900) as r:
            resp = json.loads(r.read())
        try:
            out = json.loads(resp.get("response") or "{}")
        except json.JSONDecodeError:
            out = {"verdict": "unparsed", "reason": (resp.get("response") or "")[:300]}
        out["_model"] = model
        out["_seconds"] = round(time.time() - t, 2)
        cache.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(out), encoding="utf-8")
        return out
    return judge


# ── scoring ──────────────────────────────────────────────────────────────────────────────────

@dataclass
class Scored:
    question_id: str
    pairs: list[dict] = field(default_factory=list)
    uncited: list[str] = field(default_factory=list)
    key_points: list[dict] = field(default_factory=list)

    def summary(self) -> dict:
        judged = [p for p in self.pairs if p["verdict"] in VERDICTS]
        n = len(judged)
        sup = sum(1 for p in judged if p["verdict"] == "supported")
        part = sum(1 for p in judged if p["verdict"] == "partial")
        s = {
            "question_id": self.question_id,
            "citations": len(self.pairs),
            "judged": n,
            "supported": sup,
            "partial": part,
            "unsupported": n - sup - part,
            "dangling": sum(1 for p in self.pairs if p.get("dangling")),
            "unjudged": len(self.pairs) - n,
            "citation_accuracy": sup / n if n else None,
            "citation_accuracy_partial_half": (sup + 0.5 * part) / n if n else None,
            "effective_citations": sup,
            "supported_references": len({p["n"] for p in judged if p["verdict"] == "supported"}),
            "uncited_sentences": len(self.uncited),
        }
        if self.key_points:
            w = {"covered": 1.0, "partial": 0.5}
            s["key_points"] = len(self.key_points)
            s["key_point_coverage"] = sum(w.get(k["verdict"], 0.0) for k in self.key_points) / len(self.key_points)
            s["key_points_contradicted"] = sum(1 for k in self.key_points if k.get("contradicted"))
        return s


def score_bundle(bundle: dict, judge: Judge, key_points: list[dict] | None = None) -> Scored:
    refs = {int(r["n"]): r for r in bundle.get("references") or []}
    pairs, uncited = cited_pairs(bundle["report_markdown"])
    out = Scored(bundle.get("question_id", "?"), uncited=uncited)
    for p in pairs:
        ref = refs.get(p.n)
        row = {"sentence": p.sentence, "n": p.n, "url": ref.get("url") if ref else None}
        if ref is None:
            row.update(verdict="unsupported", reason="the marker names no reference", dangling=True)
        else:
            ev = evidence_for(ref, p.sentence)
            if not ev.strip():
                row.update(verdict="unsupported", reason="no stored text for this reference", no_evidence=True)
            else:
                prompt = (f"SOURCE: {ref.get('title', '')} {ref.get('url', '')}\n\nEVIDENCE:\n{ev}\n\n"
                          f"STATEMENT: {p.sentence}\n\nDoes the evidence support the statement?")
                j = judge(CITATION_SYSTEM, prompt, CITATION_SCHEMA)
                row.update(verdict=j.get("verdict"), reason=j.get("reason"))
        out.pairs.append(row)

    self_checks = {(strip_markers(c.get("sentence", "")), int(c["n"])): c.get("support")
                   for c in (bundle.get("verification") or {}).get("citations") or [] if "n" in c}
    for row in out.pairs:
        claimed = self_checks.get((row["sentence"], row["n"]))
        if claimed:
            row["run_claimed"] = claimed

    report = bundle["report_markdown"][:REPORT_CHARS]
    for kp in key_points or []:
        prompt = f"REPORT:\n{report}\n\nKEY POINT: {kp['text']}\n\nDoes the report convey this key point?"
        j = judge(KEYPOINT_SYSTEM, prompt, KEYPOINT_SCHEMA)
        out.key_points.append({"id": kp["id"], "text": kp["text"], "verdict": j.get("verdict"),
                               "contradicted": bool(j.get("contradicted")), "reason": j.get("reason")})
    return out


def aggregate(results: list[dict]) -> dict:
    judged = sum(r["judged"] for r in results)
    sup = sum(r["supported"] for r in results)
    part = sum(r["partial"] for r in results)
    accs = [r["citation_accuracy"] for r in results if r["citation_accuracy"] is not None]
    cov = [r["key_point_coverage"] for r in results if "key_point_coverage" in r]
    micro = sup / judged if judged else None
    return {
        "reports": len(results),
        "citation_accuracy_micro": micro,
        "citation_accuracy_macro": sum(accs) / len(accs) if accs else None,
        "citation_accuracy_partial_half_micro": (sup + 0.5 * part) / judged if judged else None,
        "effective_citations_per_report": sum(r["effective_citations"] for r in results) / len(results) if results else None,
        "key_point_coverage": sum(cov) / len(cov) if cov else None,
        "target_citation_accuracy": TARGET_ACCURACY,
        "meets_target": micro is not None and micro >= TARGET_ACCURACY,
    }


def load_questions() -> dict[str, dict]:
    return {q["id"]: q for q in json.loads(QUESTIONS.read_text(encoding="utf-8"))["questions"]}


def write(out: Path, name: str, scored: Scored) -> dict:
    out.mkdir(parents=True, exist_ok=True)
    s = scored.summary()
    (out / f"{name}.json").write_text(json.dumps({"summary": s, "pairs": scored.pairs, "uncited": scored.uncited,
                                                  "key_points": scored.key_points}, indent=2), encoding="utf-8")
    return s


def print_report(name: str, s: dict, scored: Scored) -> None:
    def f(x):
        return "-" if x is None else f"{x:.3f}"
    print(f"\n{name}: citations {s['citations']} (judged {s['judged']}, supported {s['supported']}, partial "
          f"{s['partial']}, unsupported {s['unsupported']}, dangling {s['dangling']})  accuracy "
          f"{f(s['citation_accuracy'])}  effective {s['effective_citations']}  uncited sentences "
          f"{s['uncited_sentences']}" + (f"  key-point coverage {f(s['key_point_coverage'])}" if "key_points" in s else ""))
    for p in scored.pairs:
        if p["verdict"] != "supported":
            print(f"  [{p['n']}] {p['verdict'].upper()}: {p['sentence'][:110]}\n       judge: {(p.get('reason') or '')[:160]}")
    for k in scored.key_points:
        if k["verdict"] != "covered" or k["contradicted"]:
            print(f"  key point {k['id']} {k['verdict']}{' CONTRADICTED' if k['contradicted'] else ''}")


# ── commands ─────────────────────────────────────────────────────────────────────────────────

def cmd_score(a) -> int:
    bundle = json.loads(Path(a.bundle).read_text(encoding="utf-8"))
    qid = a.question or bundle.get("question_id")
    kps = load_questions().get(qid, {}).get("key_points") if qid else None
    scored = score_bundle(bundle, ollama_judge(a.judge_model, a.ollama, check_gpu=not a.no_gpu_check), kps)
    s = write(RUNS / a.label, Path(a.bundle).stem, scored)
    print_report(Path(a.bundle).stem, s, scored)
    return 0


def cmd_run(a) -> int:
    """Scores <bundles>/<qid>.json for every question; --produce makes missing bundles first."""
    qs = load_questions()
    bdir, out = Path(a.bundles), RUNS / a.label
    judge = ollama_judge(a.judge_model, a.ollama, check_gpu=not a.no_gpu_check)
    results, missing = [], []
    only = {x.strip() for x in (a.questions or "").split(",") if x.strip()}
    for qid, q in qs.items():
        if only and qid not in only:
            continue
        path = bdir / f"{qid}.json"
        if not path.exists() and a.produce:
            cmd = [c.format(qid=qid, question=q["question"], bundle=str(path)) for c in shlex.split(a.produce)]
            print(f"{qid}: producing via {cmd[0]}")
            subprocess.run(cmd, check=False, cwd=ROOT)
        if not path.exists():
            missing.append(qid)
            continue
        bundle = json.loads(path.read_text(encoding="utf-8"))
        scored = score_bundle(bundle, judge, q["key_points"])
        s = write(out, qid, scored)
        used = set(bundle.get("channels_used") or [])
        if used:
            s["expected_channels_used"] = sorted(used & set(q["expected_channels"]))
        results.append(s)
        print_report(qid, s, scored)
    agg = aggregate(results)
    agg["missing"] = missing
    agg["judge_model"] = a.judge_model
    (out / "summary.json").write_text(json.dumps({"aggregate": agg, "reports": results}, indent=2), encoding="utf-8")
    print(f"\naggregate: {json.dumps(agg)}\nwrote {out / 'summary.json'}")
    return 0 if not missing else 1


def cmd_validate(a) -> int:
    """The scorer must pass the good fixture and flag exactly the planted citations in the others."""
    judge = ollama_judge(a.judge_model, a.ollama, check_gpu=not a.no_gpu_check)
    kps = load_questions()["q01"]["key_points"]
    out = RUNS / "validation"
    ok = True
    for name in ("good", "bad", "subtle"):
        bundle = json.loads((FIXTURES / f"{name}.json").read_text(encoding="utf-8"))
        scored = score_bundle(bundle, judge, kps)
        s = write(out, name, scored)
        print_report(name, s, scored)
        planted = {(strip_markers(x["sentence"]), x["n"]) for x in bundle.get("planted_unsupported", [])}
        flagged = {(p["sentence"], p["n"]) for p in scored.pairs if p["verdict"] != "supported"}
        caught = planted <= {(p["sentence"], p["n"]) for p in scored.pairs if p["verdict"] == "unsupported"}
        false_flags = flagged - planted
        verdict = caught and not false_flags
        print(f"  expect: planted {len(planted)} caught={caught} false flags={len(false_flags)} -> "
              f"{'PASS' if verdict else 'FAIL'}")
        ok &= verdict
    print(f"\nvalidation {'PASS' if ok else 'FAIL'} (judge {a.judge_model}); details in {out}")
    return 0 if ok else 1


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    for name in ("score", "run", "validate"):
        p = sub.add_parser(name)
        p.add_argument("--judge-model", default=DEFAULT_JUDGE)
        p.add_argument("--ollama", default=OLLAMA)
        p.add_argument("--no-gpu-check", action="store_true")
        p.add_argument("--label", default="latest")
        if name == "score":
            p.add_argument("--bundle", required=True)
            p.add_argument("--question")
        if name == "run":
            p.add_argument("--bundles", required=True)
            p.add_argument("--produce", help="command template with {qid} {question} {bundle}")
            p.add_argument("--questions", help="comma-separated ids (default: all ten)")
    a = ap.parse_args()
    try:
        return {"score": cmd_score, "run": cmd_run, "validate": cmd_validate}[a.cmd](a)
    except urllib.error.URLError as e:
        print(f"judge unreachable at {a.ollama}: {e}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
