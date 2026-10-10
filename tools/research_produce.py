"""Produce one research-eval bundle by running deep research v2 through the daemon.

    python tools/research_produce.py <qid> <question> <bundle.json> [--breadth 3] [--depth 1]
                                     [--sources 16] [--timeout-min 150]

Used as `research_eval.py run --produce "python tools/research_produce.py {qid} {question} {bundle}"`.
Dispatches the researcher with the deep_research playbook against $MARLOWE_HOME's daemon, waits
for the plan, approves it narrowed to the caps (a cap never widens the plan), waits for the
result, and copies the run's bundle.json to <bundle.json> with question_id set. The plan and the
quote are written next to the bundle (<qid>.plan.txt). Exit 0 only when a bundle was written.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
EXE = Path(os.environ.get("MARLOWE_EXE", ROOT / "target" / "debug" / "marlowe.exe"))
POLL = 20


def marlowe(*args: str, check: bool = False) -> subprocess.CompletedProcess:
    p = subprocess.run([str(EXE), *args], capture_output=True, text=True, encoding="utf-8", errors="replace")
    if check and p.returncode != 0:
        raise RuntimeError(f"marlowe {' '.join(args[:2])}: {p.stderr.strip() or p.stdout.strip()}")
    return p


def status(run: str) -> str:
    for line in marlowe("crew", "list", "100").stdout.splitlines():
        if line.startswith(run):
            m = re.match(r"^\S+\s+\S+\s+(needs you|\w+)", line)
            return m.group(1) if m else "?"
    return "missing"


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("qid")
    ap.add_argument("question")
    ap.add_argument("bundle")
    ap.add_argument("--breadth", type=int, default=3)
    ap.add_argument("--depth", type=int, default=1)
    ap.add_argument("--sources", type=int, default=16)
    ap.add_argument("--timeout-min", type=float, default=150)
    ap.add_argument("--workspace", default=str(Path(os.environ.get("MARLOWE_HOME", ".")).parent / "ws"))
    a = ap.parse_args()
    out = Path(a.bundle)
    out.parent.mkdir(parents=True, exist_ok=True)
    Path(a.workspace).mkdir(parents=True, exist_ok=True)
    deadline = time.time() + a.timeout_min * 60

    d = marlowe("crew", "dispatch", "researcher", a.question, "--playbook", "deep_research",
                "--workspace", a.workspace, check=True)
    run = d.stdout.split()[0]
    print(f"{a.qid}: run {run}", flush=True)

    # The plan.
    plan = None
    while time.time() < deadline:
        p = marlowe("crew", "plan", run, "--json")
        if p.returncode == 0:
            plan = json.loads(p.stdout)
            break
        if status(run) in ("failed", "cancelled", "done", "missing"):
            print(f"{a.qid}: the run ended before proposing a plan", file=sys.stderr)
            return 1
        time.sleep(POLL)
    if plan is None:
        print(f"{a.qid}: no plan within the timeout", file=sys.stderr)
        return 1
    shown = marlowe("crew", "plan", run).stdout
    out.with_suffix(".plan.txt").write_text(shown, encoding="utf-8")
    narrow = []
    for flag, key, cap in (("--breadth", "breadth", a.breadth), ("--depth", "depth", a.depth),
                           ("--sources", "source_budget", a.sources)):
        if plan[key] > cap:
            narrow += [flag, str(cap)]
    marlowe("crew", "plan", run, "approve", *narrow, check=True)
    print(f"{a.qid}: plan approved {' '.join(narrow) or 'as proposed'}", flush=True)

    # The result.
    while time.time() < deadline:
        st = status(run)
        show = marlowe("crew", "show", run).stdout
        if "── result" in show or st in ("failed", "cancelled", "done"):
            break
        time.sleep(POLL)
    else:
        print(f"{a.qid}: no result within the timeout (run {run} left running)", file=sys.stderr)
        return 1
    record = next((l.split(" ", 1)[1].strip() for l in show.splitlines()
                   if l.startswith("artifact ") and l.strip().endswith("research.json")), None)
    if record is None:
        print(f"{a.qid}: run {run} ended {status(run)} without a report", file=sys.stderr)
        out.with_suffix(".failed.txt").write_text(show, encoding="utf-8")
        return 1
    src = Path(record).with_name("bundle.json")
    bundle = json.loads(src.read_text(encoding="utf-8"))
    bundle["question_id"] = a.qid
    bundle.setdefault("stats", {})["run"] = run
    out.write_text(json.dumps(bundle, indent=2, ensure_ascii=False), encoding="utf-8")
    shutil.copyfile(Path(record).with_name("report.md"), out.with_suffix(".report.md"))
    print(f"{a.qid}: wrote {out}", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
