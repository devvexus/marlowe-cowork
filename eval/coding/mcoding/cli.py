"""``marlowe eval ...`` / ``python eval/coding/run.py ...``: the commands of the coding eval."""

from __future__ import annotations

import argparse
import json
import os
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path
from typing import Optional

from . import arms as armsmod
from . import cc as ccmod
from . import iact, own, polyglot, report, toolchains
from .adapters import MarloweAdapter
from .model import load_records, load_tasks
from .runner import RunOptions, run_matrix
from .util import write_json
from .workspace import Context

HERE = Path(__file__).resolve().parent.parent  # eval/coding
REPO = HERE.parent.parent
SUITES = ("micro", "own", "polyglot", "iact")


def find_marlowe(explicit: Optional[str] = None) -> str:
    if explicit:
        return explicit
    env = os.environ.get("MARLOWE_BIN")
    if env:
        return env
    exe = "marlowe.exe" if sys.platform == "win32" else "marlowe"
    for base in filter(None, [os.environ.get("CARGO_TARGET_DIR"), str(REPO / "target")]):
        for profile in ("release", "debug"):
            p = Path(base) / profile / exe
            if p.is_file():
                return str(p)
    return exe


def suite_tasks(args) -> list:
    if args.suite == "micro":
        tasks = load_tasks(HERE / "micro")
    elif args.suite == "own":
        path = Path(args.own_file or HERE / "own" / "candidates.json")
        if not path.is_file():
            raise SystemExit(f"no own-repo task file at {path} (draft one with `own-mine`)")
        tasks = own.load_tasks(path, approved_only=args.approved_only, ids=args.ids.split(",") if args.ids else None)
    elif args.suite == "polyglot":
        if not args.polyglot_dir:
            raise SystemExit("--polyglot-dir is needed: a checkout of Aider-AI/polyglot-benchmark (nothing is downloaded here)")
        tasks = polyglot.discover(args.polyglot_dir, args.langs.split(",") if args.langs else None)
        if args.sample:
            tasks = polyglot.sample(tasks, args.sample, args.sample_seed)
    else:
        raise SystemExit(f"{args.suite} is not a task suite")
    if args.ids and args.suite != "own":
        want = set(args.ids.split(","))
        tasks = [t for t in tasks if t.id in want]
    if args.limit:
        tasks = tasks[: args.limit]
    return tasks


def arm_config(args, out: Path) -> armsmod.ArmConfig:
    settings = Path(args.settings) if args.settings else HERE / "settings.json"
    return armsmod.ArmConfig(
        out_dir=out,
        marlowe=find_marlowe(args.marlowe),
        model=args.model,
        settings=settings if settings.is_file() else None,
        cc_bin=args.cc_bin,
        base_url=args.base_url,
        thinking=args.thinking,
        max_budget_usd=args.max_budget,
        env_extra=dict(kv.split("=", 1) for kv in (args.env or [])),
    )


def cmd_arms(args) -> int:
    print("arms:")
    for a in armsmod.ARMS.values():
        print(f"  {a.name:<16} {a.note}")
    print("\nablations:")
    for k in armsmod.KNOBS:
        state = f"arm {k['arm']}" if k["wired"] else "NOT WIRED"
        print(f"  {k['name']:<34} {state:<18} {k['switch']}")
    return 0


def cmd_toolchains(args) -> int:
    for lang, tc in toolchains.detect_all().items():
        print(f"  {lang:<11} {'yes' if tc.available else 'NO ':<4} {tc.detail}")
    return 0


def cmd_tasks(args) -> int:
    if args.suite == "iact":
        for s in iact.load_scenarios(HERE / "iact" / "scenarios.json"):
            print(f"{s['id']}{'  (marlowe only)' if s.get('marlowe_only') else ''}")
        return 0
    for t in suite_tasks(args):
        skip = f"  [not scored: {t.meta['skip']}]" if t.meta.get("skip") else ""
        print(f"{t.id}  ({t.language}, {t.attempts} attempt{'s' if t.attempts != 1 else ''}){skip}")
    return 0


def _summary(records: list) -> dict:
    graded = [r for r in records if r.graded]
    return {"runs": len(graded), "passed": sum(1 for r in graded if r.passed), "skipped": len(records) - len(graded)}


def cmd_run(args) -> int:
    out = Path(args.out) if args.out else HERE / "results" / time.strftime("%Y%m%d-%H%M%S")
    out.mkdir(parents=True, exist_ok=True)
    cfg = arm_config(args, out)
    names = [a for a in args.arm.split(",") if a]
    adapters = {n: armsmod.make_adapter(n, cfg) for n in names}
    ctx = Context(
        cargo_idle=args.cargo_idle or str(REPO / "scripts" / "cargo-idle.ps1"),
        target_dir=args.target_dir or str(out / "cargo-target"),
    )
    tasks = [] if args.suite == "iact" else suite_tasks(args)
    scenarios = iact.load_scenarios(HERE / "iact" / "scenarios.json") if args.suite == "iact" else []
    if args.suite == "iact" and args.ids:
        scenarios = [s for s in scenarios if s["id"] in set(args.ids.split(","))]
    if args.suite == "iact" and args.limit:
        scenarios = scenarios[: args.limit]
    if not tasks and not scenarios:
        print("no tasks selected", file=sys.stderr)
        return 1
    if args.dry_run:
        print(f"suite {args.suite}: {len(tasks) or len(scenarios)} item(s) x arms {names} x {args.seeds} seed(s) -> {out}")
        for n, a in adapters.items():
            probs = a.preflight()
            print(f"  {n}: {'ready' if not probs else 'NOT READY: ' + '; '.join(probs)}")
            try:
                print(ccmod.describe(a.invocation("<prompt>", Path("<workspace>"), run_dir=out / "dry", tag="dry")))
            except NotImplementedError:
                pass
        return 0
    if not args.skip_preflight:
        for n, a in adapters.items():
            probs = a.preflight()
            if probs:
                print(f"arm {n} cannot run: " + "; ".join(probs), file=sys.stderr)
                return 2
    for n, a in adapters.items():
        if isinstance(a, MarloweAdapter) and not args.no_seed:
            for w in armsmod.seed_marlowe_home(a, thinking=args.thinking):
                print(f"warning: {n}: {w}", file=sys.stderr)
    write_json(out / "meta.json", {"argv": sys.argv[1:], "model": args.model, "suite": args.suite, "arms": names, "seeds": args.seeds, "cc": str(ccmod.find_claude(args.cc_bin) or ""), "started": time.strftime("%Y-%m-%dT%H:%M:%S")})
    opts = RunOptions(out_dir=out, seeds=args.seeds, ctx=ctx, run_id=out.name, keep_workspaces=args.keep, resume=not args.no_resume, timeout=args.timeout)
    if args.suite == "iact":
        recs = _run_iact(scenarios, adapters, opts)
    else:
        recs = run_matrix(tasks, adapters, opts)
    text = report.render(recs, title=f"Coding eval: {args.suite} on {args.model}")
    (out / "report.md").write_text(text, encoding="utf-8")
    print(text)
    if args.summary_json:
        write_json(args.summary_json, _summary(recs))
    return 0


def _run_iact(scenarios: list, adapters: dict, opts: RunOptions) -> list:
    done = {(r.task, r.arm, r.seed) for r in load_records(opts.records_path)} if opts.resume else set()
    for scn in scenarios:
        for name, adapter in adapters.items():
            for seed in range(opts.seeds):
                if (scn["id"], name, seed) in done:
                    continue
                opts.progress(f"{scn['id']} {name} seed {seed}")
                rec = iact.run_scenario(scn, adapter, opts.out_dir, run_id=opts.run_id, seed=seed, keep=opts.keep_workspaces)
                with open(opts.records_path, "a", encoding="utf-8", newline="\n") as f:
                    f.write(rec.to_json() + "\n")
                opts.progress("    " + ("PASS" if rec.passed else "skip" if not rec.graded else "fail"))
    return load_records(opts.records_path)


def cmd_report(args) -> int:
    recs = []
    for d in args.dirs:
        recs += load_records(Path(d) / "records.jsonl")
    print(report.render(recs, title="Coding eval"))
    return 0


def cmd_merge_gate(args) -> int:
    from . import stats

    baseline = load_records(Path(args.baseline) / "records.jsonl")
    reruns = [load_records(Path(d) / "records.jsonl") for d in args.rerun]
    g = stats.merge_gate(baseline, reruns, args.arm, tier=args.tier)
    print(f"{g['id']}: {'n/a' if g['pass'] is None else 'PASS' if g['pass'] else 'FAIL'} - {g['detail']}")
    return 0 if g["pass"] else 1


def cmd_cc_dry_run(args) -> int:
    claude = ccmod.find_claude(args.cc_bin)
    cfg_dir = Path(args.config_dir or (Path(os.environ.get("TEMP", ".")) / "marlowe-cc-config-dryrun"))
    inv = ccmod.invocation(
        claude or Path("claude"),
        Path(args.workspace or "<workspace>"),
        args.prompt,
        cfg_dir,
        base_url=args.base_url,
        model=armsmod.bare_model(args.model),
        thinking=args.thinking,
        max_budget_usd=args.max_budget,
    )
    print(ccmod.describe(inv))
    print(f"\nbinary: {claude or 'NOT FOUND'} (version {ccmod.version_of(claude) if claude else '-'})")
    probs = ccmod.isolation_problems(inv)
    print("isolation: " + ("all checks hold" if not probs else "PROBLEMS\n  " + "\n  ".join(probs)))
    return 1 if probs else 0


def cmd_cc_check(args) -> int:
    """Dry run plus a look at the endpoint (a request that loads nothing: /api/tags lists models)."""
    rc = cmd_cc_dry_run(args)
    try:
        with urllib.request.urlopen(args.base_url.rstrip("/") + "/api/tags", timeout=5) as r:
            tags = json.loads(r.read().decode("utf-8"))
        names = [m.get("name") for m in tags.get("models", [])]
        want = armsmod.bare_model(args.model)
        print(f"endpoint {args.base_url}: up, {len(names)} models, {want} {'present' if want in names else 'NOT FOUND'}")
        if want not in names:
            rc = rc or 1
    except (urllib.error.URLError, OSError, ValueError) as e:
        print(f"endpoint {args.base_url}: not reachable ({e})")
        rc = rc or 1
    return rc


def cmd_own_mine(args) -> int:
    cands = own.mine(args.repo or REPO, limit=args.limit)
    out = Path(args.out or HERE / "own" / "candidates.json")
    own.save_candidates(cands, out)
    print(f"{len(cands)} candidate task(s) written to {out}")
    for c in cands:
        print(f"  {c['id']}  {c['date']}  {len(c['src_files'])} src / {len(c['test_files'])} test file(s)  {c['subject'][:60]}")
    return 0


def cmd_own_validate(args) -> int:
    path = Path(args.file or HERE / "own" / "candidates.json")
    cands = own.load_candidates(path)
    work = Path(args.work or (Path(os.environ.get("TEMP", ".")) / "marlowe-own-validate"))
    ctx = Context(cargo_idle=args.cargo_idle or str(REPO / "scripts" / "cargo-idle.ps1"), target_dir=args.target_dir or str(work / "cargo-target"))
    ids = set(args.ids.split(",")) if args.ids else None
    bad = 0
    for c in cands:
        if ids and c["id"] not in ids:
            continue
        r = own.validate(own.to_task(c, path.parent), work / c["id"], ctx)
        c["validated"] = {k: r[k] for k in ("broken_fails", "fixed_passes", "ok")}
        c["validated"]["at"] = time.strftime("%Y-%m-%dT%H:%M:%S")
        print(f"{c['id']}: {'ok' if r['ok'] else 'NOT SOUND'} (bug fails: {r['broken_fails']}, fix passes: {r['fixed_passes']})")
        bad += 0 if r["ok"] else 1
        own.save_candidates(cands, path)
    return 1 if bad else 0


def build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(prog="marlowe eval", description="The M4.6 coding eval (design section 6).")
    sub = p.add_subparsers(dest="cmd", required=True)

    def common(sp):
        sp.add_argument("--model", default=ccmod.DEFAULT_MODEL, help="the local model both harnesses use")
        sp.add_argument("--base-url", default=ccmod.DEFAULT_BASE_URL, help="Ollama (loopback only)")
        sp.add_argument("--cc-bin", default=None, help="claude binary (default: newest in the VS Code extensions)")
        sp.add_argument("--thinking", action="store_true", help="thinking on for both harnesses (default off for both)")
        sp.add_argument("--max-budget", type=float, default=5.0, help="--max-budget-usd for both harnesses")

    sub.add_parser("arms", help="list the arms and ablations").set_defaults(fn=cmd_arms)
    sub.add_parser("toolchains", help="which polyglot languages can run here").set_defaults(fn=cmd_toolchains)

    t = sub.add_parser("tasks", help="list a suite's tasks")
    t.add_argument("--suite", choices=SUITES, default="micro")
    for sp in (t,):
        sp.add_argument("--polyglot-dir")
        sp.add_argument("--own-file")
        sp.add_argument("--langs")
        sp.add_argument("--sample", type=int, default=0)
        sp.add_argument("--sample-seed", type=int, default=0)
        sp.add_argument("--ids")
        sp.add_argument("--limit", type=int, default=0)
        sp.add_argument("--approved-only", action="store_true")
    t.set_defaults(fn=cmd_tasks)

    r = sub.add_parser("run", help="run a suite on one or more arms")
    r.add_argument("--suite", choices=SUITES, default="micro")
    r.add_argument("--arm", default="marlowe-full", help="comma list: " + ", ".join(armsmod.ARMS))
    r.add_argument("--seeds", type=int, default=1)
    r.add_argument("--limit", type=int, default=0)
    r.add_argument("--ids")
    r.add_argument("--out")
    r.add_argument("--marlowe")
    r.add_argument("--settings")
    r.add_argument("--env", action="append", help="KEY=VALUE added to Marlowe's environment (repeatable)")
    r.add_argument("--polyglot-dir")
    r.add_argument("--own-file")
    r.add_argument("--langs")
    r.add_argument("--sample", type=int, default=0)
    r.add_argument("--sample-seed", type=int, default=0)
    r.add_argument("--approved-only", action="store_true")
    r.add_argument("--timeout", type=float, default=None, help="agent seconds per attempt (default: the task's)")
    r.add_argument("--cargo-idle")
    r.add_argument("--target-dir")
    r.add_argument("--keep", action="store_true", help="keep workspaces")
    r.add_argument("--no-resume", action="store_true")
    r.add_argument("--skip-preflight", action="store_true")
    r.add_argument("--no-seed", action="store_true", help="do not seed the arms' private model libraries")
    r.add_argument("--dry-run", action="store_true")
    r.add_argument("--summary-json", help="write {runs, passed} here when done (marlowe bench reads it)")
    common(r)
    r.set_defaults(fn=cmd_run)

    rp = sub.add_parser("report", help="render one or more result folders")
    rp.add_argument("dirs", nargs="+")
    rp.set_defaults(fn=cmd_report)

    g = sub.add_parser("merge-gate", help="wave merge gate: nothing passing at baseline may fail 2 of 2 reruns")
    g.add_argument("--baseline", required=True, help="result folder of the baseline run")
    g.add_argument("--rerun", action="append", required=True, help="result folder of a rerun (give two or more)")
    g.add_argument("--arm", default="marlowe-full")
    g.add_argument("--tier")
    g.set_defaults(fn=cmd_merge_gate)

    d = sub.add_parser("cc-dry-run", help="show the isolated Claude Code command and check its isolation")
    d.add_argument("--workspace")
    d.add_argument("--prompt", default="<prompt>")
    d.add_argument("--config-dir")
    common(d)
    d.set_defaults(fn=cmd_cc_dry_run)
    c = sub.add_parser("cc-check", help="cc-dry-run plus the Ollama endpoint and model (loads nothing)")
    c.add_argument("--workspace")
    c.add_argument("--prompt", default="<prompt>")
    c.add_argument("--config-dir")
    common(c)
    c.set_defaults(fn=cmd_cc_check)

    m = sub.add_parser("own-mine", help="draft own-repo tasks from git history")
    m.add_argument("--repo")
    m.add_argument("--out")
    m.add_argument("--limit", type=int, default=40)
    m.set_defaults(fn=cmd_own_mine)
    v = sub.add_parser("own-validate", help="prove candidate tasks are sound (runs cargo; a machine-time decision)")
    v.add_argument("--file")
    v.add_argument("--ids")
    v.add_argument("--work")
    v.add_argument("--cargo-idle")
    v.add_argument("--target-dir")
    v.set_defaults(fn=cmd_own_validate)
    return p


def main(argv: Optional[list] = None) -> int:
    args = build_parser().parse_args(argv)
    return int(args.fn(args) or 0)


if __name__ == "__main__":
    raise SystemExit(main())
