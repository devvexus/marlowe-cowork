"""The run report: per arm and tier, what passed and what it cost, then the paired gates."""

from __future__ import annotations

import statistics
from collections import defaultdict
from typing import Iterable, Optional

from . import stats
from .model import RunRecord


def _avg(values: Iterable[Optional[float]]) -> Optional[float]:
    v = [x for x in values if x is not None]
    return statistics.fmean(v) if v else None


def _fmt(x: Optional[float], digits: int = 1) -> str:
    return "-" if x is None else f"{x:.{digits}f}"


def summary(records: list) -> dict:
    """``{(arm, tier): {...numbers...}}`` over graded runs; skipped runs are counted apart."""
    groups = defaultdict(list)
    skipped = defaultdict(int)
    for r in records:
        if r.graded:
            groups[(r.arm, r.tier)].append(r)
        else:
            skipped[(r.arm, r.tier, r.skip_reason)] += 1
    out = {}
    for (arm, tier), rs in sorted(groups.items()):
        agent = [r.agent for r in rs]
        out[(arm, tier)] = {
            "runs": len(rs),
            "tasks": len({r.task for r in rs}),
            "passed": sum(1 for r in rs if r.passed),
            "pass_rate": stats.mean_pass(records, arm, tier),
            "steps": _avg(a.get("steps") for a in agent),
            "tokens_in": _avg(a.get("input_tokens") for a in agent),
            "tokens_out": _avg(a.get("output_tokens") for a in agent),
            "cost_usd": _avg(a.get("cost_usd") for a in agent),
            "seconds": _avg(a.get("seconds") for a in agent),
            "tool_calls": _avg(a.get("tool_calls") for a in agent),
            "tool_errors": _avg(a.get("tool_errors") for a in agent),
            "repairs": _avg(a.get("repairs") for a in agent),
            "timeouts": sum(1 for a in agent if a.get("timed_out")),
            "violations": sorted({v for a in agent for v in a.get("violations", [])}),
        }
    out["skipped"] = dict(skipped)  # type: ignore[assignment]
    return out


def render(records: list, *, title: str = "Coding eval", extra_gates: Optional[list] = None) -> str:
    s = summary(records)
    skipped = s.pop("skipped")  # type: ignore[arg-type]
    lines = [f"# {title}", ""]
    if not s:
        lines.append("No graded runs.")
    else:
        lines += [
            "| arm | tier | runs | tasks | pass | steps | tok in | tok out | cost $ | secs | tool err | repairs | timeouts |",
            "|---|---|---|---|---|---|---|---|---|---|---|---|---|",
        ]
        for (arm, tier), d in s.items():
            pr = "-" if d["pass_rate"] is None else f"{100 * d['pass_rate']:.1f}% ({d['passed']}/{d['runs']})"
            lines.append(
                f"| {arm} | {tier} | {d['runs']} | {d['tasks']} | {pr} | {_fmt(d['steps'])} | {_fmt(d['tokens_in'], 0)} | {_fmt(d['tokens_out'], 0)} | "
                f"{_fmt(d['cost_usd'], 3)} | {_fmt(d['seconds'], 0)} | {_fmt(d['tool_errors'])} | {_fmt(d['repairs'])} | {d['timeouts']} |"
            )
        bad = [(k, d["violations"]) for k, d in s.items() if d["violations"]]
        for (arm, tier), v in bad:
            lines += ["", f"WARNING: arm {arm} ({tier}) called tools it should not have had: {', '.join(v)}"]
    if skipped:
        lines += ["", "Not scored (left out, not counted as failures):"]
        for (arm, tier, why), n in sorted(skipped.items()):
            lines.append(f"- {n} {tier} run(s) of {arm}: {why}")
    gate_rows = (extra_gates if extra_gates is not None else stats.gates(records)) if records else []
    if gate_rows:
        lines += ["", "## Gates", ""]
        for g in gate_rows:
            mark = "n/a" if g["pass"] is None else ("PASS" if g["pass"] else "FAIL")
            lines.append(f"- {g['id']}: {mark} - {g['detail']}")
    return "\n".join(lines) + "\n"
