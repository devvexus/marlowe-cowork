"""The own-repo tier: mining a history, putting a bug back, hiding its test, and proving the task sound."""

from __future__ import annotations

import json
import subprocess
from pathlib import Path

import pytest

from mcoding import own
from mcoding.adapters import CallableAdapter
from mcoding.runner import RunOptions, run_matrix
from mcoding.workspace import Context, grade, materialize

from helpers import CODING, commit_all, git, init_repo

PY_TEST = [["{python}", "-m", "pytest", "-q", "-p", "no:cacheprovider"]]


def python_history(tmp: Path) -> tuple:
    """A repo where commit 2 fixes a bug in calc.py and adds the test that proves it."""
    repo = tmp / "repo"
    init_repo(repo)
    (repo / "calc.py").write_text("def add(a, b):\n    return a - b\n", encoding="utf-8")
    (repo / "other.py").write_text("X = 1\n", encoding="utf-8")
    (repo / "test_calc.py").write_text("from calc import add\n\n\ndef test_zero():\n    assert add(0, 0) == 0\n", encoding="utf-8")
    base = commit_all(repo, "feat: calc")
    (repo / "calc.py").write_text("def add(a, b):\n    return a + b\n", encoding="utf-8")
    (repo / "test_calc.py").write_text("from calc import add\n\n\ndef test_zero():\n    assert add(0, 0) == 0\n\n\ndef test_adds():\n    assert add(2, 3) == 5\n", encoding="utf-8")
    fix = commit_all(repo, "fix: add subtracted")
    return repo, base, fix


def candidate(repo: Path, base: str, fix: str) -> dict:
    return {
        "id": "own-001-calc",
        "category": "bugfix",
        "repo": str(repo),
        "fix_commit": fix,
        "parent": base,
        "subject": "fix: add subtracted",
        "crate": "calc",
        "src_files": ["calc.py"],
        "test_files": ["test_calc.py"],
        "tests": PY_TEST,
        "statement": "add() returns the wrong answer. Fix the bug.",
        "status": "draft",
    }


def test_a_task_starts_from_the_parent_with_no_history_and_the_fix_tests_hidden(tmp_path):
    repo, base, fix = python_history(tmp_path)
    t = own.to_task(candidate(repo, base, fix), tmp_path)
    ws = materialize(t, tmp_path / "ws")
    assert "a - b" in (ws / "calc.py").read_text(encoding="utf-8"), "the bug is back"
    assert "test_adds" not in (ws / "test_calc.py").read_text(encoding="utf-8"), "the new test is not visible"
    log = subprocess.run(["git", "log", "--oneline"], cwd=ws, capture_output=True, text=True).stdout.strip().splitlines()
    assert len(log) == 1, "the fix cannot be read from history"
    assert not grade(t, ws, Context()).passed
    assert "test_adds" in (ws / "test_calc.py").read_text(encoding="utf-8"), "graded against the fix's tests"


def test_validation_proves_the_bug_fails_and_the_commits_own_fix_passes(tmp_path):
    repo, base, fix = python_history(tmp_path)
    t = own.to_task(candidate(repo, base, fix), tmp_path)
    r = own.validate(t, tmp_path / "work")
    assert (r["broken_fails"], r["fixed_passes"], r["ok"]) == (True, True, True)


def test_a_candidate_whose_tests_pass_without_the_fix_is_not_sound(tmp_path):
    repo, base, fix = python_history(tmp_path)
    c = candidate(repo, base, fix)
    # Hide only the old test: it passes on the broken code too.
    t = own.to_task(c, tmp_path)
    t.hidden_git = {"repo": str(repo), "commit": base, "paths": ["test_calc.py"]}
    r = own.validate(t, tmp_path / "work")
    assert r["broken_fails"] is False and r["ok"] is False


def test_an_agent_that_fixes_the_bug_passes_and_one_that_does_not_fails(tmp_path):
    repo, base, fix = python_history(tmp_path)
    t = own.to_task(candidate(repo, base, fix), tmp_path)

    def fixer(ws: Path, prompt: str):
        (ws / "calc.py").write_text("def add(a, b):\n    return a + b\n", encoding="utf-8")

    recs = run_matrix([t], {"fixer": CallableAdapter(fixer, name="fixer"), "idle": CallableAdapter(None, name="idle")}, RunOptions(out_dir=tmp_path / "o", progress=lambda m: None, ctx=Context()))
    by = {r.arm: r for r in recs}
    assert by["fixer"].passed and not by["idle"].passed
    assert (tmp_path / "o" / "work" / "fixer" / "shared").is_dir(), "own-tier workspaces keep one path so cargo can reuse its build"


def test_a_reused_workspace_only_rewrites_the_files_that_differ(tmp_path):
    repo, base, fix = python_history(tmp_path)
    t = own.to_task(candidate(repo, base, fix), tmp_path)
    ws = tmp_path / "ws"
    materialize(t, ws)
    keep = ws / "other.py"
    before = keep.stat().st_mtime_ns
    (ws / "scratch.txt").write_text("an agent's leftover", encoding="utf-8")
    (ws / "calc.py").write_text("changed by the agent", encoding="utf-8")
    materialize(t, ws)
    assert keep.stat().st_mtime_ns == before, "unchanged files keep their modification time"
    assert not (ws / "scratch.txt").exists(), "leftovers are removed"
    assert "a - b" in (ws / "calc.py").read_text(encoding="utf-8")


def rust_like_history(tmp: Path) -> tuple:
    repo = tmp / "rust"
    init_repo(repo)
    c = repo / "crates" / "foo"
    (c / "src").mkdir(parents=True)
    (c / "tests").mkdir()
    (c / "Cargo.toml").write_text("[package]\nname = 'foo'\n", encoding="utf-8")
    (c / "src" / "lib.rs").write_text("pub fn f() -> i32 { 1 }\n", encoding="utf-8")
    (c / "src" / "tests.rs").write_text("#[test]\nfn old() {}\n", encoding="utf-8")
    (c / "tests" / "regress.rs").write_text("#[test]\nfn old_it() {}\n", encoding="utf-8")
    base = commit_all(repo, "feat: foo")
    (c / "src" / "lib.rs").write_text("pub fn f() -> i32 { 2 }\n// more\n// lines\n", encoding="utf-8")
    (c / "src" / "tests.rs").write_text("#[test]\nfn old() {}\n\n#[test]\nfn f_returns_two() {}\n", encoding="utf-8")
    (c / "tests" / "regress.rs").write_text("#[test]\nfn old_it() {}\n\n#[test]\nfn it_keeps_order() {}\n", encoding="utf-8")
    fix = commit_all(repo, "M4.6 Z: review fixes (f returned one, order lost)")
    return repo, base, fix


def test_mining_finds_fix_commits_with_separate_tests_and_builds_the_cargo_commands(tmp_path):
    repo, base, fix = rust_like_history(tmp_path)
    cands = own.mine(repo, limit=10, since="2000-01-01")
    assert len(cands) == 1
    c = cands[0]
    assert (c["crate"], c["fix_commit"], c["parent"]) == ("foo", fix, base)
    assert c["src_files"] == ["crates/foo/src/lib.rs"]
    assert sorted(c["test_files"]) == ["crates/foo/src/tests.rs", "crates/foo/tests/regress.rs"]
    assert sorted(c["added_tests"]) == ["f_returns_two", "it_keeps_order"]
    cmds = [" ".join(x) for x in c["tests"]]
    assert any("test -p foo --lib tests" in x for x in cmds)
    assert any("test -p foo --test regress" in x for x in cmds)
    assert all(x.startswith("powershell -NoProfile -File {cargo_idle}") for x in cmds), "cargo always goes through the guard script"
    assert c["status"] == "draft" and c["needs_review"] is True
    assert "f returns two" in c["statement"], "the added tests' names are in the statement"
    assert c["statement"].startswith("There is a bug in the `foo` crate"), c["statement"]


def test_mining_skips_features_and_fixes_without_a_separate_test(tmp_path):
    repo = tmp_path / "r"
    init_repo(repo)
    c = repo / "crates" / "foo" / "src"
    c.mkdir(parents=True)
    (c / "lib.rs").write_text("pub fn f() {}\n", encoding="utf-8")
    commit_all(repo, "start")
    (c / "lib.rs").write_text("pub fn f() {}\npub fn g() {}\npub fn h() {}\n", encoding="utf-8")
    commit_all(repo, "fix: a bug but with inline tests only\n")
    assert own.mine(repo, since="2000-01-01") == []


def test_the_module_path_of_a_test_file():
    assert own.module_path("src/headless/tests.rs") == "headless::tests"
    assert own.module_path("src/tests.rs") == "tests"
    assert own.module_path("src/a/mod.rs") == "a"


def test_the_drafted_candidate_set_is_forty_unreviewed_bug_fixes_with_real_files():
    path = CODING / "own" / "candidates.json"
    d = json.loads(path.read_text(encoding="utf-8"))
    cands = d["candidates"]
    assert len(cands) == 40
    assert "not approved" in d["about"].lower() or "has not approved" in d["about"].lower()
    assert len({c["id"] for c in cands}) == 40
    assert all(c["status"] == "draft" and c["needs_review"] is True and c["category"] == "bugfix" for c in cands)
    assert all(c["date"] >= "2026-06-01" for c in cands)
    for c in cands:
        assert c["src_files"] and c["test_files"] and c["tests"] and c["statement"]
        assert all(cmd[:4] == ["powershell", "-NoProfile", "-File", "{cargo_idle}"] for cmd in c["tests"])
    # Where the history is available, every named file really is in the commits it names.
    repo = Path(cands[0]["repo"])
    if not (repo / ".git").exists():
        pytest.skip("the source repository is not on this machine")
    have = subprocess.run(["git", "-C", str(repo), "cat-file", "-e", cands[0]["fix_commit"]], capture_output=True).returncode == 0
    if not have:
        pytest.skip("the history the candidates were mined from is not in this clone")
    for c in cands:
        for rel in c["src_files"]:
            fixed = subprocess.run(["git", "-C", str(repo), "cat-file", "-e", f"{c['fix_commit']}:{rel}"], capture_output=True).returncode
            before = subprocess.run(["git", "-C", str(repo), "cat-file", "-e", f"{c['parent']}:{rel}"], capture_output=True).returncode
            assert fixed == 0 or before == 0, f"{c['id']}: {rel} is in neither commit"
        t = own.to_task(c, path.parent)
        assert t.setup["commit"] == c["parent"] and t.hidden_git["commit"] == c["fix_commit"]
