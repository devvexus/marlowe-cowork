"""Review fixes: the machine guard everywhere, runs outside the checkout, infrastructure is not a failure."""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

import pytest

from mcoding import arms as armsmod
from mcoding import cli, own, toolchains
from mcoding.adapters import CallableAdapter
from mcoding.guard import CargoGuard, install_cargo_shim
from mcoding.model import Task, load_records
from mcoding.runner import RunOptions, run_matrix
from mcoding.util import is_guarded, run_guarded
from mcoding.workspace import Context, grade, is_compile_failure

from helpers import CODING, commit_all, init_repo
from test_own import candidate, python_history

HAS_POWERSHELL = shutil.which("powershell") is not None
needs_powershell = pytest.mark.skipif(not HAS_POWERSHELL, reason="the guard is a PowerShell script")


def fake_guard(tmp: Path, body: str) -> str:
    """A stand-in for scripts/cargo-idle.ps1 (same file name, so it counts as guarded)."""
    d = tmp / "guard"
    d.mkdir(parents=True, exist_ok=True)
    p = d / "cargo-idle.ps1"
    p.write_text(body, encoding="utf-8")
    return str(p)


# ---- the arm name and the real harness -----------------------------------------------------


def test_an_unknown_arm_is_a_clean_message_not_a_traceback(tmp_path, capsys):
    out = tmp_path / "o"
    rc = cli.main(["run", "--suite", "micro", "--arm", "full", "--out", str(out), "--dry-run"])
    err = capsys.readouterr().err
    assert rc == 2
    assert "unknown arm full" in err and "marlowe-full" in err and "Traceback" not in err
    assert not out.exists(), "nothing is created for a run that cannot start"


def test_the_real_entry_point_accepts_the_arm_the_rust_report_card_passes(tmp_path):
    # crates/marlowe/src/bench/e2e.rs builds `--arm marlowe-full`; the dry run is the harness's own check.
    r = subprocess.run(
        [sys.executable, "-I", str(CODING / "run.py"), "run", "--suite", "micro", "--arm", "marlowe-full", "--limit", "1", "--dry-run", "--out", str(tmp_path / "o")],
        capture_output=True,
        text=True,
    )
    assert r.returncode == 0, r.stderr
    assert "marlowe-full" in r.stdout


# ---- runs live outside every checkout --------------------------------------------------------


def test_a_run_folder_inside_the_checkout_is_refused(tmp_path, capsys):
    inside = cli.REPO / "eval" / "coding" / "results" / "should-not-exist"
    rc = cli.main(["run", "--suite", "micro", "--arm", "noop", "--limit", "1", "--out", str(inside)])
    assert rc == 2 and "inside this repository" in capsys.readouterr().err
    assert not inside.exists()


def test_any_git_checkout_above_the_run_folder_is_refused(tmp_path):
    repo = tmp_path / "some-other-checkout"
    init_repo(repo)
    assert "git checkout" in (cli.check_out_dir(repo / "results" / "run") or "")
    assert cli.check_out_dir(tmp_path / "plain" / "run") is None


def test_the_default_run_folder_is_outside_the_checkout(monkeypatch, tmp_path):
    monkeypatch.setenv("LOCALAPPDATA", str(tmp_path / "local"))
    root = cli.default_out_root()
    assert root == tmp_path / "local" / "marlowe" / "eval"
    assert cli.check_out_dir(root / "20261011-000000") is None
    here = cli.REPO.resolve()
    assert here not in root.resolve().parents


# ---- cargo behind the guard ------------------------------------------------------------------


def test_the_polyglot_rust_command_goes_through_the_guard():
    cmd = toolchains.COMMANDS["rust"]["tests"][0]
    assert cmd[:4] == ["powershell", "-NoProfile", "-File", "{cargo_idle}"]
    assert cmd[4:] == ["test", "--", "--include-ignored"]
    assert all(c[0] != "cargo" for lang in toolchains.COMMANDS.values() for c in lang["tests"] + lang["prepare"])
    assert Path(Context().cargo_idle).name == "cargo-idle.ps1", "the default context finds the real guard"


def test_the_agents_environment_puts_the_cargo_shim_first_with_the_limits(tmp_path):
    cfg = armsmod.ArmConfig(out_dir=tmp_path / "o", marlowe="m.exe", cargo_idle="C:/g/cargo-idle.ps1", target_dir=str(tmp_path / "t"))
    for arm in ("marlowe-full", "marlowe-shell", "cc-local"):
        a = armsmod.make_adapter(arm, cfg)
        inv = a.invocation("p", tmp_path, run_dir=tmp_path / "r", tag="t")
        path_key = next(k for k in inv.env if k.upper() == "PATH")
        assert inv.env[path_key].split(os.pathsep)[0] == str(tmp_path / "o" / "cargo-shim"), arm
        assert (inv.env["CARGO_BUILD_JOBS"], inv.env["RUST_TEST_THREADS"]) == ("1", "1"), arm
        assert inv.env["CARGO_TARGET_DIR"] == str(tmp_path / "t"), arm
    sess = armsmod.make_adapter("marlowe-full", cfg).session_invocation(tmp_path, run_dir=tmp_path / "r", tag="s")
    assert sess.env["CARGO_BUILD_JOBS"] == "1", "the interactive suite is guarded too"
    shim = tmp_path / "o" / "cargo-shim"
    assert {p.name for p in shim.iterdir()} == {"cargo", "cargo.cmd", "cargo_shim.py"}
    assert "cargo_shim.py" in (shim / "cargo.cmd").read_text(encoding="utf-8")


def test_without_a_guard_the_environment_is_untouched(tmp_path):
    a = armsmod.make_adapter("marlowe-full", armsmod.ArmConfig(out_dir=tmp_path / "o", marlowe="m.exe"))
    assert a.guard is None
    assert "CARGO_TARGET_DIR" not in a.invocation("p", tmp_path, run_dir=tmp_path, tag="t").env


@needs_powershell
def test_the_shim_forwards_to_the_guard_which_cannot_find_the_shim_again(tmp_path):
    guard = fake_guard(
        tmp_path,
        'Write-Output ("args=" + ($args -join "|"))\n'
        'Write-Output ("first=" + ($env:PATH -split ";")[0])\n'
        'exit 7\n',
    )
    g = install_cargo_shim(tmp_path / "shim", guard)
    env = dict(os.environ)
    env = g.apply(env)
    r = subprocess.run([sys.executable, str(tmp_path / "shim" / "cargo_shim.py"), "build", "-p", "x", "--", "--nocapture"], env=env, capture_output=True, text=True)
    assert r.returncode == 7, "the guard's exit code is cargo's"
    assert "args=build|-p|x|--|--nocapture" in r.stdout
    first = [l for l in r.stdout.splitlines() if l.startswith("first=")][0]
    assert str(tmp_path / "shim") not in first, "the real cargo is found, not the shim again"


# ---- the machine is not the agent ------------------------------------------------------------


def rust_task(tmp: Path, guard: str, timeout: int = 5) -> Task:
    ws_root = tmp / "root"
    ws_root.mkdir(exist_ok=True)
    return Task(id="t", tier="own", prompt="p", root=ws_root, tests=[["powershell", "-NoProfile", "-File", "{cargo_idle}", "test"]], test_timeout=timeout)


@needs_powershell
def test_the_test_clock_starts_when_the_guard_lets_the_command_run(tmp_path):
    guard = fake_guard(tmp_path, 'Write-Host "[cargo-idle] waiting: 3 GB of commit free, need 12 GB"\nStart-Sleep -Seconds 4\nWrite-Output "test result: ok. 1 passed; 0 failed"\nexit 0\n')
    ws = tmp_path / "ws"
    ws.mkdir()
    started = time.monotonic()
    g = grade(rust_task(tmp_path, guard, timeout=2), ws, Context(cargo_idle=guard))
    assert g.passed and not g.infra, g
    assert time.monotonic() - started >= 4, "the wait outlasted the 2 s test timeout and was not counted"


@needs_powershell
def test_a_run_that_is_slow_after_it_starts_still_times_out(tmp_path):
    guard = fake_guard(tmp_path, 'Write-Output "Compiling x"\nStart-Sleep -Seconds 30\nexit 0\n')
    ws = tmp_path / "ws"
    ws.mkdir()
    g = grade(rust_task(tmp_path, guard, timeout=2), ws, Context(cargo_idle=guard))
    assert not g.passed and not g.infra and "timed out" in g.reason


@needs_powershell
def test_the_hold_file_exit_is_infrastructure_not_a_failed_run(tmp_path):
    guard = fake_guard(tmp_path, 'Write-Host "[cargo-idle] HOLD: all builds and tests are paused"\nexit 75\n')
    ws = tmp_path / "ws"
    ws.mkdir()
    task = rust_task(tmp_path, guard)
    g = grade(task, ws, Context(cargo_idle=guard))
    assert g.infra and not g.passed and "not graded" in g.reason

    rec = run_matrix([_with_workspace(task, tmp_path)], {"idle": CallableAdapter(None, name="idle")},
                     RunOptions(out_dir=tmp_path / "o", progress=lambda m: None, ctx=Context(cargo_idle=guard)))
    assert len(rec) == 1 and rec[0].graded is False and rec[0].meta.get("infra") is True
    assert "not graded" in rec[0].skip_reason

    # The machine is free again: resume runs it again and the new record replaces the old one.
    ok = fake_guard(tmp_path / "free", 'Write-Output "test result: ok. 1 passed; 0 failed"\nexit 0\n')
    again = run_matrix([_with_workspace(task, tmp_path)], {"idle": CallableAdapter(None, name="idle")},
                       RunOptions(out_dir=tmp_path / "o", progress=lambda m: None, ctx=Context(cargo_idle=ok)))
    assert len(again) == 1 and again[0].graded is True and again[0].passed is True
    assert len(load_records(tmp_path / "o" / "records.jsonl")) == 1


def _with_workspace(task: Task, tmp: Path) -> Task:
    src = tmp / "root" / "repo"
    src.mkdir(parents=True, exist_ok=True)
    (src / "f.txt").write_text("x", encoding="utf-8")
    task.setup = {"kind": "dir", "path": "repo"}
    return task


def test_a_guard_that_never_starts_is_reported_after_the_wait_cap(tmp_path):
    if not HAS_POWERSHELL:
        pytest.skip("PowerShell")
    guard = fake_guard(tmp_path, "Start-Sleep -Seconds 30\n")
    r = run_guarded(["powershell", "-NoProfile", "-File", guard], timeout=5, wait_cap=2)
    assert r.never_started and not r.timed_out


def test_only_commands_through_cargo_idle_are_guarded():
    assert is_guarded(["powershell", "-NoProfile", "-File", "C:/x/scripts/cargo-idle.ps1", "test"])
    assert not is_guarded(["python", "-m", "pytest"])


# ---- unfair tasks ----------------------------------------------------------------------------


def test_compile_failures_are_told_from_assertion_failures():
    assert is_compile_failure("error[E0425]: cannot find function `helper` in this scope")
    assert is_compile_failure("error: could not compile `foo` (lib test) due to 1 previous error")
    assert not is_compile_failure("thread 'a' panicked at src/lib.rs:3\nassertion `left == right` failed\ntest result: FAILED. 0 passed; 1 failed")


def compile_failing_history(tmp: Path) -> tuple:
    """The fix adds a function and the hidden test calls it: on the parent the test does not compile."""
    repo = tmp / "repo"
    init_repo(repo)
    (repo / "calc.py").write_text("def add(a, b):\n    return a - b\n", encoding="utf-8")
    (repo / "test_calc.py").write_text("from calc import add\n\n\ndef test_zero():\n    assert add(0, 0) == 0\n", encoding="utf-8")
    base = commit_all(repo, "feat: calc")
    (repo / "calc.py").write_text("def add(a, b):\n    return a + b\n\n\ndef helper_sum(xs):\n    return sum(xs)\n", encoding="utf-8")
    (repo / "test_calc.py").write_text("from calc import add, helper_sum\n\n\ndef test_adds():\n    assert add(2, 3) == helper_sum([2, 3])\n", encoding="utf-8")
    fix = commit_all(repo, "fix: add subtracted")
    return repo, base, fix


@needs_powershell
def test_validation_marks_a_task_unfair_when_its_tests_do_not_compile_before_the_fix(tmp_path):
    # Rust-shaped output from a stand-in for cargo: compile error until the fix's source is in place.
    repo, base, fix = compile_failing_history(tmp_path)
    guard = fake_guard(
        tmp_path,
        '$src = Get-Content -Raw "calc.py"\n'
        'if ($src -match "helper_sum") { Write-Output "test result: ok. 1 passed; 0 failed"; exit 0 }\n'
        '[Console]::Error.WriteLine("error[E0432]: unresolved import `calc::helper_sum`")\n'
        '[Console]::Error.WriteLine("error: could not compile `calc` (test) due to 1 previous error")\n'
        'exit 101\n',
    )
    c = candidate(repo, base, fix)
    c["tests"] = [["powershell", "-NoProfile", "-File", "{cargo_idle}"]]
    c["statement"] = "add() is wrong."
    t = own.to_task(c, tmp_path)
    r = own.validate(t, tmp_path / "work", Context(cargo_idle=guard))
    assert (r["broken_fails"], r["broken_compile_error"], r["unfair"], r["ok"]) == (True, True, True, False)

    # Recorded, such a task is left out of approved runs and of any run that would score it.
    c["status"] = "approved"
    c["validated"] = {"unfair": True}
    assert own.to_task(c, tmp_path).meta["skip"].startswith("unfair")
    path = tmp_path / "cands.json"
    own.save_candidates([c], path)
    assert own.load_tasks(path, approved_only=True) == []


def test_a_task_that_fails_by_assertion_is_fair(tmp_path):
    repo, base, fix = python_history(tmp_path)
    r = own.validate(own.to_task(candidate(repo, base, fix), tmp_path), tmp_path / "work")
    assert (r["broken_compile_error"], r["unfair"], r["ok"]) == (False, False, True)
