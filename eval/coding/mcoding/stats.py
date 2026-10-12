"""Paired comparison of arms and the A1 / A3 gates.

Runs are noisy (the design budgets a ~9 point spread), so arms are compared per task: each task's
score is its pass rate over the seeds, the difference is taken task by task, and a bootstrap over
tasks gives the confidence interval. Tasks one arm did not run are left out of that comparison.
"""

from __future__ import annotations

import random
import statistics
from collections import defaultdict
from typing import Iterable, Optional

from .model import RunRecord


def graded(records: Iterable[RunRecord]) -> list:
    return [r for r in records if r.graded]


def task_scores(records: Iterable[RunRecord], arm: str, tier: Optional[str] = None) -> dict:
    """task id -> pass rate over its seeds, for the graded runs of ``arm``."""
    by = defaultdict(list)
    for r in graded(records):
        if r.arm == arm and (tier is None or r.tier == tier):
            by[r.task].append(1.0 if r.passed else 0.0)
    return {t: sum(v) / len(v) for t, v in by.items()}


def mean_pass(records: Iterable[RunRecord], arm: str, tier: Optional[str] = None) -> Optional[float]:
    s = task_scores(records, arm, tier)
    return statistics.fmean(s.values()) if s else None


def paired_diffs(records: list, a: str, b: str, tier: Optional[str] = None) -> list:
    """Per-task ``score(a) - score(b)`` for tasks both arms ran."""
    sa, sb = task_scores(records, a, tier), task_scores(records, b, tier)
    return [sa[t] - sb[t] for t in sorted(set(sa) & set(sb))]


def bootstrap(diffs: list, *, n: int = 10000, seed: int = 0, alpha: float = 0.05) -> tuple:
    """(mean, low, high) of the mean of ``diffs`` by resampling tasks with replacement."""
    if not diffs:
        return (0.0, 0.0, 0.0)
    rng = random.Random(seed)
    k = len(diffs)
    means = sorted(statistics.fmean(rng.choices(diffs, k=k)) for _ in range(n))
    lo = means[int((alpha / 2) * n)]
    hi = means[min(n - 1, int((1 - alpha / 2) * n))]
    return (statistics.fmean(diffs), lo, hi)


def gate_row(gate_id: str, passed: Optional[bool], detail: str) -> dict:
    """``passed`` None means the numbers needed are not there (not run, no data), which is not a pass."""
    return {"id": gate_id, "pass": passed, "detail": detail}


def pts(x: float) -> str:
    return f"{100 * x:+.1f} pts"


def a1(records: list, marlowe: str, cc: str, *, tiers=("own", "polyglot"), floor: float = -0.10) -> list:
    """A1: pooled mean no worse than Claude Code's; per tier the interval's lower bound above -10 pts; iact >= CC."""
    rows = []
    pooled = []
    for tier in tiers:
        d = paired_diffs(records, marlowe, cc, tier)
        if not d:
            rows.append(gate_row(f"A1/{tier}", None, f"no paired {tier} runs of {marlowe} and {cc}"))
            continue
        pooled += d
        mean, lo, hi = bootstrap(d)
        rows.append(gate_row(f"A1/{tier}", lo >= floor, f"{marlowe} - {cc}: {pts(mean)} (95% CI {pts(lo)} to {pts(hi)}, {len(d)} tasks; need lower bound >= {pts(floor)})"))
    if pooled:
        m = statistics.fmean(pooled)
        rows.append(gate_row("A1/pooled", m >= 0, f"pooled mean difference {pts(m)} over {len(pooled)} tasks (need >= 0)"))
    else:
        rows.append(gate_row("A1/pooled", None, "no paired runs"))
    ia, ib = task_scores(records, marlowe, "iact"), task_scores(records, cc, "iact")
    common = sorted(set(ia) & set(ib))
    if common:
        sa, sb = sum(ia[t] for t in common), sum(ib[t] for t in common)
        rows.append(gate_row("A1/iact", sa >= sb, f"{marlowe} {sa:.1f}/{len(common)} vs {cc} {sb:.1f}/{len(common)} scripted scenarios (need >=)"))
    else:
        rows.append(gate_row("A1/iact", None, "no paired iact runs"))
    return rows


def a3(records: list, full: str, shell: str, tier: str = "own", margin: float = 0.10) -> dict:
    """A3: the full tool set beats the shell-only baseline by 10 points with the interval above zero."""
    d = paired_diffs(records, full, shell, tier)
    if not d:
        return gate_row("A3", None, f"no paired {tier} runs of {full} and {shell}")
    mean, lo, hi = bootstrap(d)
    return gate_row("A3", mean >= margin and lo > 0, f"{full} - {shell}: {pts(mean)} (95% CI {pts(lo)} to {pts(hi)}, {len(d)} tasks; need mean >= {pts(margin)} and lower bound > 0)")


def gates(records: list, *, marlowe: str = "marlowe-full", cc: str = "cc-local", shell: str = "marlowe-shell") -> list:
    return a1(records, marlowe, cc) + [a3(records, marlowe, shell)]


def merge_gate(baseline: list, reruns: list, arm: str, *, tier: Optional[str] = None) -> dict:
    """The wave merge gate: no task that passed at baseline may fail in 2 of 2 reruns.

    ``baseline`` and each element of ``reruns`` are record lists. A task "passed at baseline" when
    at least half its baseline runs passed; it "fails a rerun" when none of that rerun's runs of it
    passed. With fewer than two reruns the gate cannot be decided.
    """
    base = {t: s for t, s in task_scores(baseline, arm, tier).items() if s >= 0.5}
    if len(reruns) < 2:
        return gate_row("merge-gate", None, f"needs 2 reruns, got {len(reruns)}") | {"regressions": []}
    scores = [task_scores(r, arm, tier) for r in reruns]
    regressions = sorted(t for t in base if all(s.get(t) is not None and s[t] == 0.0 for s in scores))
    detail = f"{len(base)} task(s) passed at baseline; " + (f"{len(regressions)} failed every rerun: {', '.join(regressions)}" if regressions else "none failed every rerun")
    return gate_row("merge-gate", not regressions, detail) | {"regressions": regressions}
