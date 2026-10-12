"""The micro tasks and the matrix runner, with stand-in agents."""

from __future__ import annotations

import json
import shutil
from pathlib import Path

from mcoding import report, stats
from mcoding.adapters import CallableAdapter
from mcoding.model import Task, load_records, load_tasks
from mcoding.runner import RunOptions, run_matrix
from mcoding.workspace import Context, count_passed, grade, materialize

from helpers import MICRO, fake_marlowe, records


def oracle(task_root: Path):
    """An agent that applies the task's reference solution."""

    def fn(ws: Path, prompt: str) -> None:
        for p in (task_root / "solution").rglob("*"):
            if p.is_file():
                target = ws / p.relative_to(task_root / "solution")
                target.parent.mkdir(parents=True, exist_ok=True)
                shutil.copyfile(p, target)

    return fn


def test_there_are_ten_micro_tasks_each_with_a_reference_solution():
    tasks = load_tasks(MICRO)
    assert len(tasks) == 10
    for t in tasks:
        assert (t.root / "solution").is_dir(), t.id
        assert t.prompt and t.tests and t.hidden


def test_no_micro_task_passes_untouched_and_every_one_passes_with_its_solution(tmp_path):
    tasks = load_tasks(MICRO)
    for t in tasks:
        ws = materialize(t, tmp_path / "before" / t.id)
        g = grade(t, ws)
        assert not g.passed, f"{t.id} passes without any work: {g.output_tail[-300:]}"
    for t in tasks:
        ws = materialize(t, tmp_path / "after" / t.id)
        oracle(t.root)(ws, "")
        g = grade(t, ws)
        assert g.passed, f"{t.id} fails with its reference solution: {g.output_tail[-600:]}"
        assert g.tests_passed and g.tests_passed >= 3


def test_the_hidden_tests_are_not_in_the_workspace_until_grading(tmp_path):
    t = load_tasks(MICRO)[0]
    ws = materialize(t, tmp_path / "ws")
    assert not (ws / "test_hidden.py").exists()
    assert not (ws / "solution").exists()
    grade(t, ws)
    assert (ws / "test_hidden.py").is_file()


def test_the_workspace_has_a_one_commit_history_to_diff_against(tmp_path):
    import subprocess

    t = load_tasks(MICRO)[0]
    ws = materialize(t, tmp_path / "ws")
    log = subprocess.run(["git", "log", "--oneline"], cwd=ws, capture_output=True, text=True).stdout.strip().splitlines()
    assert len(log) == 1


def test_the_matrix_records_every_run_and_resumes_without_rerunning(tmp_path):
    tasks = load_tasks(MICRO)[:3]
    adapters = {"noop": CallableAdapter(None, name="noop")}
    messages = []
    opts = RunOptions(out_dir=tmp_path / "out", seeds=2, run_id="r1", progress=messages.append, ctx=Context())
    recs = run_matrix(tasks, adapters, opts)
    assert len(recs) == 6
    assert all(r.graded and not r.passed for r in recs)
    assert {r.seed for r in recs} == {0, 1}
    again = run_matrix(tasks, adapters, opts)
    assert len(again) == 6 and len(adapters["noop"].calls) == 6, "a resumed run must not repeat finished runs"
    lines = (tmp_path / "out" / "records.jsonl").read_text(encoding="utf-8").splitlines()
    assert len(lines) == 6 and all(json.loads(l)["arm"] == "noop" for l in lines)


def test_a_solving_agent_passes_and_a_crashing_one_is_a_failed_run_not_a_dead_matrix(tmp_path):
    tasks = load_tasks(MICRO)[:2]

    def boom(ws, prompt):
        raise RuntimeError("agent exploded")

    ok = CallableAdapter(lambda ws, p: oracle(next(t for t in tasks if (t.root.name in str(ws))).root)(ws, p), name="solver")
    opts = RunOptions(out_dir=tmp_path / "out", run_id="r2", progress=lambda m: None, keep_workspaces=False)
    recs = run_matrix(tasks, {"solver": ok, "boom": CallableAdapter(boom, name="boom")}, opts)
    by = {(r.task, r.arm): r for r in recs}
    assert all(by[(t.id, "solver")].passed for t in tasks)
    assert not any(by[(t.id, "boom")].passed for t in tasks)
    assert "agent exploded" in by[(tasks[0].id, "boom")].agent["error"]
    assert not (tmp_path / "out" / "work" / "solver" / f"{tasks[0].id}-s0").exists(), "workspaces are removed unless kept"


def test_a_task_with_a_missing_toolchain_is_recorded_as_not_scored(tmp_path):
    t = load_tasks(MICRO)[0]
    t.meta["skip"] = "go toolchain unavailable"
    recs = run_matrix([t], {"noop": CallableAdapter(None, name="noop")}, RunOptions(out_dir=tmp_path / "o", progress=lambda m: None))
    assert recs[0].graded is False and "unavailable" in recs[0].skip_reason
    assert stats.mean_pass(recs, "noop") is None
    assert "Not scored" in report.render(recs)


def test_two_attempts_retry_with_the_test_output_and_keep_the_session(tmp_path):
    t = load_tasks(MICRO)[1]
    t.attempts = 2
    t.meta["retry_template"] = "Still failing:\n{errors}\nFix it."
    solver = oracle(t.root)
    state = {"n": 0}

    def second_time_lucky(ws, prompt):
        state["n"] += 1
        if state["n"] == 2:
            solver(ws, prompt)

    a = CallableAdapter(second_time_lucky, name="lucky")
    recs = run_matrix([t], {"lucky": a}, RunOptions(out_dir=tmp_path / "o", progress=lambda m: None))
    assert recs[0].passed and recs[0].attempts_used == 2
    assert a.calls[0][1] is False and a.calls[1][1] is True, "the retry continues the session"
    assert a.calls[1][0].startswith("Still failing:") and "assert" in a.calls[1][0]


def test_count_passed_reads_pytest_cargo_and_jest_summaries():
    assert count_passed("===== 4 passed in 0.02s =====") == 4
    assert count_passed("test result: ok. 3 passed; 0 failed\n...\ntest result: ok. 2 passed; 0 failed") == 5
    assert count_passed("Tests:       7 passed, 7 total") == 7
    assert count_passed("ok  \texample.com/a\t0.1s\nok  \texample.com/b\t0.2s") == 2
    assert count_passed("nothing recognisable") is None


def test_a_filter_that_matches_no_test_does_not_pass(tmp_path):
    t = Task(id="x", tier="own", prompt="p", root=tmp_path, tests=[["{python}", "-c", "print('running 0 tests\\ntest result: ok. 0 passed; 0 failed')"]])
    ws = tmp_path / "ws"
    ws.mkdir()
    g = grade(t, ws)
    assert not g.passed and g.reason == "no test ran"


def test_an_agent_and_a_marlowe_style_binary_are_measured_the_same_way(tmp_path):
    """Steps, tokens, tool calls and errors come from the stream; repairs from the stats file."""
    t = load_tasks(MICRO)[0]
    sol = (t.root / "solution" / "util.py").read_text(encoding="utf-8")
    plan = [{"tools": ["read", "edit", "bash"], "fail_tool": True, "writes": {"util.py": sol}, "result": "fixed"}]
    a, record = fake_marlowe(tmp_path, plan)
    recs = run_matrix([t], {"marlowe-full": a}, RunOptions(out_dir=tmp_path / "o", progress=lambda m: None))
    r = recs[0]
    assert r.passed
    ag = r.agent
    assert (ag["steps"], ag["input_tokens"], ag["output_tokens"], ag["cached_tokens"]) == (4, 100, 20, 5)
    assert (ag["tool_calls"], ag["tool_errors"]) == (3, 1)
    assert ag["repairs"] == 2, "repaired + from_text, read from MARLOWE_EVAL_STATS"
    assert ag["final_text"] == "fixed"
    row = records(record)[0]
    assert row["stdin"].startswith("`clamp`") and "-p" in row["argv"] and "--standalone" in row["argv"] and "--no-memory" in row["argv"]
    assert load_records(tmp_path / "o" / "records.jsonl")[0].agent["tool_calls"] == 3
