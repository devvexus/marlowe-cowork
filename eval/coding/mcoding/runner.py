"""The matrix: every task by every arm by every seed, graded by hidden tests, one record each.

A record is appended to ``<out>/records.jsonl`` as soon as its run is graded, so an interrupted
run resumes where it stopped (``resume``) and a crash loses at most the run in flight.
"""

from __future__ import annotations

import traceback
from dataclasses import asdict, dataclass, field
from pathlib import Path
from typing import Callable, Iterable, Optional

from .adapters import Adapter
from .model import AgentResult, RunRecord, Task, load_records
from .util import rmtree, tail
from .workspace import Context, Grade, diff_of_work, grade, materialize


@dataclass
class RunOptions:
    out_dir: Path
    seeds: int = 1
    ctx: Context = field(default_factory=Context)
    run_id: str = "run"
    keep_workspaces: bool = False
    resume: bool = True
    #: overrides every task's agent timeout (seconds) when set
    timeout: Optional[float] = None
    progress: Callable[[str], None] = print

    @property
    def records_path(self) -> Path:
        return self.out_dir / "records.jsonl"


def retry_prompt(task: Task, last: Grade) -> str:
    template = task.meta.get(
        "retry_template",
        "The tests still fail:\n```\n{errors}\n```\nThe tests are correct; do not change them. Fix the code to make them pass.",
    )
    return template.format(errors=last.output_tail.strip()[-3000:], files=" ".join(task.files) or "the code")


def run_one(task: Task, adapter: Adapter, seed: int, opts: RunOptions) -> RunRecord:
    base = dict(run=opts.run_id, task=task.id, tier=task.tier, arm=adapter.name, seed=seed)
    skip = task.meta.get("skip")
    if skip:
        return RunRecord(**base, passed=False, graded=False, skip_reason=str(skip))
    shared = bool(task.setup.get("reuse"))
    # A reusable workspace keeps one path (cargo fingerprints depend on it) and is brought up to date per task.
    ws = opts.out_dir / "work" / adapter.name / ("shared" if shared else f"{task.id}-s{seed}")
    run_dir = opts.out_dir / "runs" / adapter.name / f"{task.id}-s{seed}"
    try:
        materialize(task, ws, opts.ctx)
    except Exception as e:
        return RunRecord(**base, passed=False, graded=True, test_output_tail=f"could not build the workspace: {e}", meta={"setup_failed": True})
    agent: Optional[AgentResult] = None
    result: Optional[Grade] = None
    used = 0
    timeout = opts.timeout if opts.timeout is not None else task.timeout
    for attempt in range(1, max(1, task.attempts) + 1):
        used = attempt
        prompt = task.prompt if attempt == 1 else retry_prompt(task, result or Grade(False))
        try:
            res = adapter.run(prompt, ws, timeout=timeout, run_dir=run_dir, tag=f"a{attempt}", continue_session=attempt > 1)
        except Exception as e:  # an adapter bug is a failed run with its traceback kept, not a dead matrix
            res = AgentResult(error=f"{type(e).__name__}: {e}", is_error=True)
            run_dir.mkdir(parents=True, exist_ok=True)
            (run_dir / f"adapter-error-a{attempt}.txt").write_text(traceback.format_exc(), encoding="utf-8")
        agent = res if agent is None else agent.merge(res)
        result = grade(task, ws, opts.ctx)
        if result.passed or result.infra:
            break
    assert agent is not None and result is not None
    run_dir.mkdir(parents=True, exist_ok=True)
    (run_dir / "diff-stat.txt").write_text(diff_of_work(ws), encoding="utf-8")
    (run_dir / "test-output.txt").write_text(result.output_tail, encoding="utf-8")
    if not opts.keep_workspaces and not shared:
        rmtree(ws)
    if result.infra:
        # The machine decided, not the agent: left out of every rate, and run again on resume.
        return RunRecord(**base, passed=False, graded=False, skip_reason=result.reason, attempts_used=used,
                         test_output_tail=tail(result.output_tail, 1500), agent=asdict(agent), meta={"infra": True, "reason": result.reason})
    return RunRecord(
        **base,
        passed=result.passed,
        attempts_used=used,
        test_output_tail=tail(result.output_tail, 1500),
        agent=asdict(agent),
        meta={"reason": result.reason, "tests_passed": result.tests_passed},
    )


def _append(path: Path, rec: RunRecord) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with open(path, "a", encoding="utf-8", newline="\n") as f:
        f.write(rec.to_json() + "\n")


def run_matrix(tasks: Iterable[Task], adapters: dict, opts: RunOptions) -> list:
    """Runs every (task, arm, seed) not already in the records file; returns all records."""
    done = {(r.task, r.arm, r.seed) for r in load_records(opts.records_path) if not r.meta.get("infra")} if opts.resume else set()
    tasks = list(tasks)
    total = len(tasks) * len(adapters) * opts.seeds
    n = 0
    for task in tasks:
        for name, adapter in adapters.items():
            for seed in range(opts.seeds):
                n += 1
                if (task.id, name, seed) in done:
                    continue
                opts.progress(f"[{n}/{total}] {task.id} {name} seed {seed}")
                rec = run_one(task, adapter, seed, opts)
                _append(opts.records_path, rec)
                opts.progress(f"    {'PASS' if rec.passed else ('skip' if not rec.graded else 'fail')}"
                              + (f" ({rec.meta.get('reason')})" if rec.meta.get("reason") else ""))
    return load_records(opts.records_path)
