"""Paired statistics and the A1/A3 gates, the arms, and the command line."""

from __future__ import annotations

import json
import random

import pytest

from mcoding import arms, cli, report, stats
from mcoding.model import RunRecord, load_records

from helpers import fake_marlowe, records


def rec(task, arm, passed, tier="own", seed=0, **agent) -> RunRecord:
    return RunRecord(run="r", task=task, tier=tier, arm=arm, seed=seed, passed=passed, agent=agent)


def matrix(tier, tasks, a_rate, b_rate, a="marlowe-full", b="cc-local", seeds=3, seed=7):
    rng = random.Random(seed)
    out = []
    for t in range(tasks):
        for s in range(seeds):
            out.append(rec(f"{tier}-{t}", a, rng.random() < a_rate, tier, s))
            out.append(rec(f"{tier}-{t}", b, rng.random() < b_rate, tier, s))
    return out


def test_scores_are_per_task_pass_rates_over_seeds_and_skips_do_not_count():
    rs = [rec("t1", "a", True, seed=0), rec("t1", "a", False, seed=1), rec("t2", "a", True), RunRecord(run="r", task="t3", tier="own", arm="a", seed=0, passed=False, graded=False, skip_reason="x")]
    assert stats.task_scores(rs, "a") == {"t1": 0.5, "t2": 1.0}
    assert stats.mean_pass(rs, "a") == 0.75
    assert stats.mean_pass(rs, "nobody") is None


def test_the_bootstrap_interval_contains_the_mean_and_narrows_with_more_tasks():
    d_small = [1, 0, 0, 1, 0, 1, 0, 0]
    d_big = d_small * 10
    m1, lo1, hi1 = stats.bootstrap([float(x) for x in d_small])
    m2, lo2, hi2 = stats.bootstrap([float(x) for x in d_big])
    assert lo1 <= m1 <= hi1 and lo2 <= m2 <= hi2
    assert (hi2 - lo2) < (hi1 - lo1)
    assert stats.bootstrap([]) == (0.0, 0.0, 0.0)
    assert stats.bootstrap([0.3] * 5)[1:] == (0.3, 0.3)
    assert stats.bootstrap([1.0, 0.0, 1.0], seed=3) == stats.bootstrap([1.0, 0.0, 1.0], seed=3), "seeded, so a rerun reports the same interval"


def test_a1_passes_when_marlowe_matches_claude_code_and_fails_when_it_is_clearly_worse():
    even = matrix("own", 40, 0.6, 0.6) + matrix("polyglot", 40, 0.5, 0.5)
    rows = {g["id"]: g for g in stats.a1(even, "marlowe-full", "cc-local")}
    assert rows["A1/own"]["pass"] is True and rows["A1/polyglot"]["pass"] is True
    worse = matrix("own", 40, 0.2, 0.8) + matrix("polyglot", 40, 0.2, 0.8)
    rows = {g["id"]: g for g in stats.a1(worse, "marlowe-full", "cc-local")}
    assert rows["A1/own"]["pass"] is False and rows["A1/pooled"]["pass"] is False
    assert "95% CI" in rows["A1/own"]["detail"]


def test_a1_says_not_measured_instead_of_passing_when_a_side_is_missing():
    rows = stats.a1(matrix("own", 5, 0.5, 0.5, b="other"), "marlowe-full", "cc-local")
    assert all(g["pass"] is None for g in rows), rows


def test_a1_iact_compares_scenarios_passed():
    rs = [rec(f"i{n}", "marlowe-full", n < 12, "iact") for n in range(15)] + [rec(f"i{n}", "cc-local", n < 10, "iact") for n in range(15)]
    row = next(g for g in stats.a1(rs, "marlowe-full", "cc-local") if g["id"] == "A1/iact")
    assert row["pass"] is True
    rs = [rec(f"i{n}", "marlowe-full", n < 8, "iact") for n in range(15)] + [rec(f"i{n}", "cc-local", n < 10, "iact") for n in range(15)]
    assert next(g for g in stats.a1(rs, "marlowe-full", "cc-local") if g["id"] == "A1/iact")["pass"] is False


def test_a3_needs_ten_points_and_an_interval_above_zero():
    big = matrix("own", 40, 0.8, 0.5, a="marlowe-full", b="marlowe-shell")
    assert stats.a3(big, "marlowe-full", "marlowe-shell")["pass"] is True
    small = matrix("own", 40, 0.55, 0.5, a="marlowe-full", b="marlowe-shell")
    assert stats.a3(small, "marlowe-full", "marlowe-shell")["pass"] is False
    assert stats.a3([], "marlowe-full", "marlowe-shell")["pass"] is None


def test_the_report_has_a_row_per_arm_and_tier_the_gates_and_flags_a_leaky_ablation():
    rs = matrix("own", 4, 0.75, 0.25, a="marlowe-full", b="marlowe-shell", seeds=2)
    rs.append(rec("own-0", "marlowe-shell", False, violations=["read"], steps=3, input_tokens=100, cost_usd=0.0, seconds=2.0))
    text = report.render(rs)
    assert "| marlowe-full | own |" in text and "| marlowe-shell | own |" in text
    assert "## Gates" in text and "A3" in text
    assert "marlowe-shell (own) called tools it should not have had: read" in text


def test_every_documented_arm_can_be_built_and_the_shell_arm_is_the_only_tool_ablation(tmp_path):
    cfg = arms.ArmConfig(out_dir=tmp_path, marlowe="marlowe.exe", model="marlowe-dusk:27b-super")
    for name in arms.ARMS:
        a = arms.make_adapter(name, cfg)
        assert a.name == name
    full = arms.make_adapter("marlowe-full", cfg)
    shell = arms.make_adapter("marlowe-shell", cfg)
    daemon = arms.make_adapter("marlowe-daemon", cfg)
    memory = arms.make_adapter("marlowe-memory", cfg)
    assert full.model == "ollama/marlowe-dusk:27b-super"
    assert "MARLOWE_TOOL_GROUPS" not in full.env() and shell.env()["MARLOWE_TOOL_GROUPS"] == "shell"
    assert "--standalone" in full.flags(tmp_path) and "--no-memory" in full.flags(tmp_path)
    assert "--standalone" not in daemon.flags(tmp_path)
    assert "--no-memory" not in memory.flags(tmp_path)
    assert full.home != shell.home, "each arm has its own private home"
    with pytest.raises(KeyError):
        arms.make_adapter("nonsense", cfg)


def test_ablations_without_a_switch_are_listed_as_not_wired():
    unwired = {k["name"] for k in arms.KNOBS if not k["wired"]}
    assert {"repo map on/off", "LSP on/off", "rolling vs append-only masking", "single agent vs build_v2"} <= unwired
    assert all(k["arm"] is None for k in arms.KNOBS if not k["wired"])


def test_the_shell_only_arm_flags_any_tool_outside_the_shell_as_a_violation(tmp_path):
    ws = tmp_path / "ws"
    ws.mkdir()
    a, rec_file = fake_marlowe(tmp_path, [{"tools": ["bash", "job_output", "read"], "writes": {}}], name="marlowe-shell", shell_only=True)
    res = a.run("go", ws, timeout=60, run_dir=tmp_path / "run")
    assert res.violations == ["read"]
    assert records(rec_file)[0]["env"]["MARLOWE_TOOL_GROUPS"] == "shell"
    ok_dir = tmp_path / "ok"
    ok_dir.mkdir()
    ok, _ = fake_marlowe(ok_dir, [{"tools": ["Bash", "PowerShell", "python", "JobOutput"]}], name="s2", shell_only=True)
    assert ok.run("go", ws, timeout=60, run_dir=tmp_path / "run2").violations == []


def test_model_refs_are_normalised_for_each_harness():
    assert arms.model_ref("marlowe-dusk:27b-super") == "ollama/marlowe-dusk:27b-super"
    assert arms.model_ref("openrouter/vendor/x") == "openrouter/vendor/x"
    assert arms.model_ref("vendor/x") == "ollama/vendor/x"
    assert arms.bare_model("ollama/marlowe-dusk:27b-super") == "marlowe-dusk:27b-super"
    assert arms.bare_model("marlowe-dusk:27b-super") == "marlowe-dusk:27b-super"


def test_the_cli_runs_a_suite_end_to_end_and_writes_records_report_and_summary(tmp_path, capsys):
    out = tmp_path / "out"
    summary = tmp_path / "summary.json"
    rc = cli.main(["run", "--suite", "micro", "--arm", "noop", "--limit", "3", "--out", str(out), "--summary-json", str(summary)])
    text = capsys.readouterr().out
    assert rc == 0
    assert len(load_records(out / "records.jsonl")) == 3
    assert "| noop | micro |" in text and (out / "report.md").is_file()
    assert json.loads(summary.read_text(encoding="utf-8")) == {"runs": 3, "passed": 0, "skipped": 0}
    # Re-running the same folder resumes: nothing new is added.
    assert cli.main(["run", "--suite", "micro", "--arm", "noop", "--limit", "3", "--out", str(out)]) == 0
    assert len(load_records(out / "records.jsonl")) == 3


def test_the_cli_lists_arms_tasks_and_toolchains(capsys):
    assert cli.main(["arms"]) == 0 and "marlowe-shell" in capsys.readouterr().out
    assert cli.main(["tasks", "--suite", "micro"]) == 0 and capsys.readouterr().out.count("micro-") == 10
    assert cli.main(["tasks", "--suite", "iact"]) == 0 and capsys.readouterr().out.count("iact-") == 15
    assert cli.main(["toolchains"]) == 0 and "python" in capsys.readouterr().out


def test_a_dry_run_shows_each_arms_command_and_whether_it_could_run(tmp_path, capsys):
    rc = cli.main(["run", "--suite", "micro", "--arm", "marlowe-shell,cc-local", "--dry-run", "--out", str(tmp_path / "d"), "--marlowe", "no-such-marlowe.exe", "--limit", "1"])
    text = capsys.readouterr().out
    assert rc == 0
    assert "marlowe-shell: NOT READY" in text and "marlowe binary not found" in text
    assert "--setting-sources" in text and "bypassPermissions" in text


def test_a_run_refuses_an_arm_that_cannot_run_instead_of_recording_failures(tmp_path, capsys):
    rc = cli.main(["run", "--suite", "micro", "--arm", "marlowe-full", "--out", str(tmp_path / "x"), "--marlowe", "no-such-marlowe.exe", "--limit", "1"])
    assert rc == 2 and "cannot run" in capsys.readouterr().err
    assert not (tmp_path / "x" / "records.jsonl").exists()


def test_the_polyglot_and_own_suites_say_what_they_need(tmp_path):
    with pytest.raises(SystemExit) as e:
        cli.main(["tasks", "--suite", "polyglot"])
    assert "--polyglot-dir" in str(e.value)
    with pytest.raises(SystemExit) as e:
        cli.main(["tasks", "--suite", "own", "--own-file", str(tmp_path / "none.json")])
    assert "own-mine" in str(e.value)


def test_the_merge_gate_flags_a_task_that_passed_at_baseline_and_fails_every_rerun():
    base = [rec("a", "marlowe-full", True), rec("b", "marlowe-full", True), rec("c", "marlowe-full", False)]
    rerun1 = [rec("a", "marlowe-full", True), rec("b", "marlowe-full", False), rec("c", "marlowe-full", False)]
    rerun2 = [rec("a", "marlowe-full", False), rec("b", "marlowe-full", False), rec("c", "marlowe-full", True)]
    g = stats.merge_gate(base, [rerun1, rerun2], "marlowe-full")
    assert g["pass"] is False and g["regressions"] == ["b"], "a failed one rerun and passed the other; b failed both"
    assert stats.merge_gate(base, [rerun1], "marlowe-full")["pass"] is None
    ok = stats.merge_gate(base, [rerun1, base], "marlowe-full")
    assert ok["pass"] is True and ok["regressions"] == []


def test_the_merge_gate_command_reads_two_reruns(tmp_path, capsys):
    def folder(name, recs):
        d = tmp_path / name
        d.mkdir()
        (d / "records.jsonl").write_text("".join(r.to_json() + "\n" for r in recs), encoding="utf-8")
        return str(d)

    b = folder("b", [rec("a", "marlowe-full", True)])
    r1, r2 = folder("r1", [rec("a", "marlowe-full", False)]), folder("r2", [rec("a", "marlowe-full", False)])
    assert cli.main(["merge-gate", "--baseline", b, "--rerun", r1, "--rerun", r2]) == 1
    assert "failed every rerun: a" in capsys.readouterr().out
    assert cli.main(["merge-gate", "--baseline", b, "--rerun", b, "--rerun", b]) == 0
