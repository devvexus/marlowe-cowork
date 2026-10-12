"""The first-try tool probe: does a local model call each tool right on its first try?

  python -I eval/tool_probe/probe.py --go                       dawn and dusk, typeless (B) and typed (C)
  python -I eval/tool_probe/probe.py --go --models marlowe-dawn:9b --arms B --only web_fetch-url,cancel-workflow
  python -I eval/tool_probe/probe.py --grade DIR                grade the .jsonl files of an earlier run again

Each (model, arm) is one `marlowe headless --probe-first-call` process on the real binary: the
real specs, persona, blocks and prefix fit, the Ollama wire, every call answered `probe: not run`
(nothing executes, no approval). Arm B sends the typeless specs; arm C sets
MARLOWE_OLLAMA_TYPED_TOOLS=1 and sends them typed. Ollama only, never a paid model.

It loads the models onto the GPU, so it does not start without `--go` (the owner's go-ahead).
Writes report.json and report.md next to the run files (default eval/tool_probe/out/<stamp>/).
"""

import argparse
import collections
import json
import os
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import grade as g  # noqa: E402

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent
DEFAULT_MODELS = ["marlowe-dawn:9b", "marlowe-dusk:27b-super"]
ARMS = {"B": {"MARLOWE_OLLAMA_TYPED_TOOLS": "0"}, "C": {"MARLOWE_OLLAMA_TYPED_TOOLS": "1"}}
# The pass criteria of the design (strict, lenient), per model family.
CRITERIA = {"dusk": (0.95, 0.98), "dawn": (0.85, 0.98)}


def load_cases(path):
    cases = []
    for line in Path(path).read_text(encoding="utf-8").splitlines():
        if line.strip() and not line.lstrip().startswith("//"):
            cases.append(json.loads(line))
    return cases


def find_binary(explicit):
    if explicit:
        return Path(explicit)
    for rel in ("target/release/marlowe.exe", "target/debug/marlowe.exe", "target/release/marlowe", "target/debug/marlowe"):
        if (ROOT / rel).is_file():
            return ROOT / rel
    sys.exit("no marlowe binary under target/; build one or pass --binary")


def run_arm(binary, cases_path, model, arm, out_dir, args):
    """One process: every case, `--temps` runs each. Returns the lines it wrote."""
    out = out_dir / f"{model.replace(':', '_').replace('/', '_')}.{arm}.jsonl"
    env = {k: v for k, v in os.environ.items() if k != "MARLOWE_OLLAMA_TYPED_TOOLS"}
    env.update(ARMS[arm])
    cmd = [str(binary), "headless", "--probe-first-call", str(cases_path), "--model", model, "--num-ctx", str(args.num_ctx),
           "--temps", args.temps, "--timeout", str(args.timeout), "--out", str(out)]
    if args.only:
        cmd += ["--only", args.only]
    if args.calibrate:
        cmd.append("--calibrate")
    print(f"[{time.strftime('%H:%M:%S')}] {model} arm {arm}: {' '.join(cmd[1:])}", flush=True)
    subprocess.run(cmd, env=env, check=True)
    manifest = out_dir / "manifest.json"
    seen = json.loads(manifest.read_text()) if manifest.is_file() else {}
    seen[out.name] = {"model": model, "arm": arm}
    manifest.write_text(json.dumps(seen, indent=1))
    return out


def read_runs(path):
    lines = [json.loads(line) for line in Path(path).read_text(encoding="utf-8").splitlines() if line.strip()]
    surfaces = {l["key"]: l for l in lines if l.get("kind") == "surface"}
    runs = [l for l in lines if l.get("kind") == "run"]
    return surfaces, runs


def family(model):
    return "dusk" if "dusk" in model else "dawn" if "dawn" in model else "other"


def grade_file(cases, path):
    """Per-case results for one (model, arm) file."""
    by_id = {c["id"]: c for c in cases}
    surfaces, runs = read_runs(path)
    per_case = collections.defaultdict(list)
    for run in runs:
        case = by_id.get(run["case"])
        if case is None:
            continue
        res = g.grade(case, run, surfaces)
        res["run"] = run["run"]
        res["error"] = run.get("error")
        per_case[run["case"]].append(res)
    return surfaces, runs, per_case


def pct(k, n):
    return 100.0 * k / n if n else 0.0


def calibration(surfaces, runs):
    """Measured tool-schema cost per surface against the chars/3 estimate."""
    rows = []
    for run in runs:
        cal = run.get("calibration")
        if not cal:
            continue
        s = surfaces.get(run.get("surface"), {})
        measured = cal["with_tools"] - cal["without_tools"]
        est_chars3 = s.get("tools_chars", 0) / 3
        rows.append({
            "surface": run.get("surface"), "measured_tokens": measured, "estimate_chars_3": round(est_chars3),
            "estimate_harness": s.get("tools_estimate"), "ratio_to_chars_3": round(measured / est_chars3, 3) if est_chars3 else None,
            "chars_per_token": round(s.get("tools_chars", 0) / measured, 2) if measured > 0 else None,
        })
    return rows


def report(cases, files, out_dir):
    """files: {(model, arm): path}. Builds report.json and report.md."""
    ids = [c["id"] for c in cases]
    data = {"cases": len(cases), "arms": {}, "rows": {}, "calibration": {}}
    graded = {}
    for (model, arm), path in files.items():
        surfaces, runs, per_case = grade_file(cases, path)
        graded[(model, arm)] = per_case
        n = sum(len(v) for v in per_case.values())
        strict = sum(r["strict"] for v in per_case.values() for r in v)
        lenient = sum(r["lenient"] for v in per_case.values() for r in v)
        scen = [c["id"] for c in cases if c.get("scenario") in (1, 2, 3, 4)]
        scen_runs = [r for i in scen for r in per_case.get(i, [])]
        scen_ok = sum(r["strict"] for r in scen_runs)
        fam = family(model)
        want_s, want_l = CRITERIA.get(fam, (None, None))
        data["arms"][f"{model}|{arm}"] = {
            "runs": n, "strict": strict, "lenient": lenient, "strict_pct": round(pct(strict, n), 1), "lenient_pct": round(pct(lenient, n), 1),
            "scenarios_strict": f"{scen_ok}/{len(scen_runs)}", "scenarios_all_pass": bool(scen_runs) and scen_ok == len(scen_runs),
            "meets": None if want_s is None else {"strict": pct(strict, n) >= 100 * want_s, "lenient": pct(lenient, n) >= 100 * want_l},
            "errors": sum(1 for v in per_case.values() for r in v if r["error"]),
            "dawn_flagged": [i for i, v in per_case.items() if fam == "dawn" and sum(not r["strict"] for r in v) >= 2],
        }
        data["calibration"][f"{model}|{arm}"] = calibration(surfaces, runs)
    for i in ids:
        row = {}
        for (model, arm), per_case in graded.items():
            rs = per_case.get(i, [])
            if not rs:
                continue
            wrong = collections.Counter(r["first_call"] for r in rs if not r["strict"] and r["first_call"])
            rew = collections.Counter(w for r in rs for w in r["rewrites"])
            row[f"{model}|{arm}"] = {
                "strict": f"{sum(r['strict'] for r in rs)}/{len(rs)}", "lenient": f"{sum(r['lenient'] for r in rs)}/{len(rs)}",
                "wrong_first_call": wrong.most_common(1)[0][0] if wrong else None,
                "why": next((r["strict_why"] for r in rs if not r["strict"]), None),
                "rewrite": rew.most_common(1)[0][0] if rew else None,
            }
        data["rows"][i] = row
    (out_dir / "report.json").write_text(json.dumps(data, indent=1), encoding="utf-8")
    (out_dir / "report.md").write_text(markdown(cases, data), encoding="utf-8")
    return data


def markdown(cases, data):
    keys = sorted({k for r in data["rows"].values() for k in r})
    out = ["# Tool probe: first-try calls", "", f"{data['cases']} cases. strict = right tool, schema-exact args, no rewrite; lenient = after the forgiving layer.", "", "## Totals", ""]
    out += ["| model | arm | runs | strict | lenient | scenarios 1-4 strict | meets |", "|---|---|---|---|---|---|---|"]
    for k, a in data["arms"].items():
        m, arm = k.split("|")
        meets = "-" if a["meets"] is None else ("yes" if all(a["meets"].values()) and a["scenarios_all_pass"] else "no")
        out.append(f"| {m} | {arm} | {a['runs']} | {a['strict_pct']}% | {a['lenient_pct']}% | {a['scenarios_strict']} | {meets} |")
    out += ["", "## Per case", "", "| case | request | " + " | ".join(f"{k.replace('|', ' ')} strict/lenient" for k in keys) + " | most common wrong first call | rewrite applied |",
            "|---|---|" + "---|" * (len(keys) + 2)]
    by_id = {c["id"]: c for c in cases}
    for i, row in data["rows"].items():
        cells = [f"{row[k]['strict']} / {row[k]['lenient']}" if k in row else "-" for k in keys]
        wrong = next((row[k]["wrong_first_call"] for k in keys if k in row and row[k]["wrong_first_call"]), "")
        rew = next((row[k]["rewrite"] for k in keys if k in row and row[k]["rewrite"]), "")
        out.append(f"| {i} | {by_id[i]['request'][:60].replace('|', '/')} | " + " | ".join(cells) + f" | {wrong or ''} | {rew or ''} |")
    out += ["", "## Prompt cost of the tool schemas (measured against chars/3)", ""]
    out += ["| model | arm | surface | measured tokens | chars/3 estimate | harness estimate | measured / chars-3 | chars per token |", "|---|---|---|---|---|---|---|---|"]
    for k, rows in data["calibration"].items():
        m, arm = k.split("|")
        for r in rows:
            out.append(f"| {m} | {arm} | {r['surface']} | {r['measured_tokens']} | {r['estimate_chars_3']} | {r['estimate_harness']} | {r['ratio_to_chars_3']} | {r['chars_per_token']} |")
    flagged = {k: a["dawn_flagged"] for k, a in data["arms"].items() if a["dawn_flagged"]}
    if flagged:
        out += ["", "## Revise and re-probe (dawn failed strict on 2 of 3 runs)", ""] + [f"- {k.replace('|', ' arm ')}: {', '.join(v)}" for k, v in flagged.items()]
    return "\n".join(out) + "\n"


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--go", action="store_true", help="the GPU go-ahead: without it nothing is started")
    ap.add_argument("--grade", metavar="DIR", help="grade the .jsonl files in DIR again; no model runs")
    ap.add_argument("--cases", default=str(HERE / "cases.jsonl"))
    ap.add_argument("--models", default=",".join(DEFAULT_MODELS))
    ap.add_argument("--arms", default="B,C")
    ap.add_argument("--num-ctx", type=int, default=32768)
    ap.add_argument("--temps", default="0,default,default", help="one run per entry")
    ap.add_argument("--only", default="")
    ap.add_argument("--timeout", type=int, default=300)
    ap.add_argument("--binary", default="")
    ap.add_argument("--out", default="")
    ap.add_argument("--no-calibrate", dest="calibrate", action="store_false")
    args = ap.parse_args()

    cases = load_cases(args.cases)
    if args.only:
        want = set(args.only.split(","))
        cases = [c for c in cases if c["id"] in want]
    if args.grade:
        out_dir = Path(args.grade)
        manifest = json.loads((out_dir / "manifest.json").read_text())
        files = {(m["model"], m["arm"]): out_dir / name for name, m in manifest.items()}
        report(cases, files, out_dir)
        print(f"wrote {out_dir / 'report.md'}")
        return 0
    models, arms = [m for m in args.models.split(",") if m], [a for a in args.arms.split(",") if a]
    if any("/" in m and not m.startswith("ollama/") for m in models):
        sys.exit("local Ollama models only")
    if bad := [a for a in arms if a not in ARMS]:
        sys.exit(f"unknown arm {bad} (B typeless, C typed)")
    n_runs = len(args.temps.split(","))
    print(f"{len(cases)} cases x {n_runs} runs x {len(models)} models x {len(arms)} arms")
    if not args.go:
        print("not started: this loads the models onto the GPU; run again with --go once it is free")
        return 0
    out_dir = Path(args.out) if args.out else HERE / "out" / time.strftime("%Y%m%d-%H%M%S")
    out_dir.mkdir(parents=True, exist_ok=True)
    binary = find_binary(args.binary)
    files = {}
    for model in models:
        for arm in arms:
            files[(model, arm)] = run_arm(binary, args.cases, model, arm, out_dir, args)
    data = report(cases, files, out_dir)
    for k, a in data["arms"].items():
        print(f"{k}: strict {a['strict_pct']}%  lenient {a['lenient_pct']}%  scenarios {a['scenarios_strict']}")
    print(f"wrote {out_dir / 'report.md'}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
