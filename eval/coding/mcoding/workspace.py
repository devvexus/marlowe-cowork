"""Workspaces: building the folder an agent works in, and grading what it left.

``dir`` tasks copy a folder. ``git`` tasks (the own-repo tier) export one commit, put back the files
the later fix changed, and start a fresh one-commit history, so ``git log`` cannot show the fix.
Hidden tests are installed only after the agent has finished.
"""

from __future__ import annotations

import io
import os
import re
import shutil
import subprocess
import sys
import zipfile
from dataclasses import dataclass, field
from pathlib import Path
from typing import Optional

from .model import Task
from .util import popen, rmtree, run_cmd, scrubbed_env, tail


@dataclass
class Context:
    """Paths the task commands may mention: ``{python}``, ``{cargo_idle}``, ``{target_dir}``."""

    python: str = sys.executable
    cargo_idle: str = ""
    target_dir: str = ""
    extra_env: dict = field(default_factory=dict)

    def expand(self, argv) -> list:
        mapping = {"python": self.python, "cargo_idle": self.cargo_idle, "target_dir": self.target_dir}
        return [re.sub(r"\{(\w+)\}", lambda m: mapping.get(m.group(1), m.group(0)), a) for a in argv]

    def env(self) -> dict:
        env = scrubbed_env()
        if self.target_dir:
            env["CARGO_TARGET_DIR"] = self.target_dir
        env.setdefault("CARGO_BUILD_JOBS", "2")
        env.setdefault("RUST_TEST_THREADS", "2")
        env["PYTHONDONTWRITEBYTECODE"] = "1"
        env.update(self.extra_env)
        return env


def git_bytes(repo, *args) -> Optional[bytes]:
    """stdout of a git command as bytes, or None when it fails."""
    with popen(["git", "-C", str(repo), *args], text=False, encoding=None, errors=None, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL) as p:
        out, _ = p.communicate()
        return out if p.returncode == 0 else None


def git(cwd, *args, check: bool = True) -> str:
    r = run_cmd(["git", *args], cwd=cwd, timeout=300)
    if check and r.code != 0:
        raise RuntimeError(f"git {' '.join(args)} failed in {cwd}: {r.err.strip() or r.out.strip()}")
    return r.out


def init_baseline(ws: Path) -> None:
    """A fresh one-commit repository of whatever is in ``ws`` (so a diff against it is the agent's work)."""
    git(ws, "init", "-q")
    for k, v in (("user.email", "eval@localhost"), ("user.name", "eval"), ("core.autocrlf", "false"), ("commit.gpgsign", "false")):
        git(ws, "config", k, v)
    git(ws, "add", "-A")
    git(ws, "commit", "-q", "--allow-empty", "-m", "base")


def export_commit(repo, commit: str, dest: Path) -> None:
    """The tree of ``commit`` without history, via a zip so it needs no tar."""
    data = git_bytes(repo, "archive", "--format=zip", commit)
    if data is None:
        raise RuntimeError(f"git archive {commit} failed in {repo}")
    dest.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(io.BytesIO(data)) as z:
        z.extractall(dest)


KEEP = {".git", "target", "node_modules", "__pycache__", ".pytest_cache"}


def sync_tree(src: Path, dst: Path) -> None:
    """Makes ``dst`` hold exactly the files of ``src``, touching only files whose bytes differ.

    A reused workspace keeps the modification times of everything that did not change, so a build
    tool (cargo) rebuilds the crates the task actually differs in and not the whole tree.
    """
    dst.mkdir(parents=True, exist_ok=True)
    wanted = set()
    for root, dirs, files in os.walk(src):
        dirs[:] = [d for d in dirs if d not in KEEP]
        rel_root = Path(root).relative_to(src)
        for f in files:
            rel = rel_root / f
            wanted.add(rel)
            a, b = Path(root) / f, dst / rel
            if b.is_file() and b.stat().st_size == a.stat().st_size and b.read_bytes() == a.read_bytes():
                continue
            b.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(a, b)
    for root, dirs, files in os.walk(dst, topdown=False):
        rel_root = Path(root).relative_to(dst)
        if rel_root.parts and rel_root.parts[0] in KEEP:
            continue
        for f in files:
            if rel_root / f not in wanted and (rel_root / f).parts[0] not in KEEP:
                (Path(root) / f).unlink()
        if rel_root.parts and not any(Path(root).iterdir()):
            Path(root).rmdir()


def materialize(task: Task, dest: Path, ctx: Optional[Context] = None) -> Path:
    """Builds the agent's starting workspace in ``dest`` and returns it.

    With ``setup.reuse`` an existing ``dest`` is brought up to date in place instead of rebuilt
    (see :func:`sync_tree`); either way the history is a fresh single commit.
    """
    ctx = ctx or Context()
    reuse = bool(task.setup.get("reuse")) and dest.exists()
    if dest.exists() and not reuse:
        rmtree(dest)
    if reuse and (dest / ".git").exists():
        rmtree(dest / ".git")
    kind = task.setup.get("kind", "dir")
    if kind == "dir":
        src = task.root / task.setup.get("path", "repo")
        if not src.is_dir():
            raise FileNotFoundError(f"{task.id}: setup folder {src} does not exist")
        extra = task.setup.get("exclude", [])
        shutil.copytree(src, dest, ignore=shutil.ignore_patterns(".git", "__pycache__", "node_modules", "target", ".pytest_cache", "*.pyc", *extra))
    elif kind == "git":
        repo, commit, parent = task.setup["repo"], task.setup["commit"], task.setup.get("parent", task.setup["commit"])
        stage = dest.parent / (dest.name + ".export")
        if stage.exists():
            rmtree(stage)
        export_commit(repo, commit, stage)
        for rel in task.setup.get("revert", []):
            old = git_bytes(repo, "show", f"{parent}:{rel}")
            target = stage / rel
            if old is None:
                # The fix added this file: it does not exist before it.
                if target.exists():
                    target.unlink()
            else:
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_bytes(old)
        if reuse:
            sync_tree(stage, dest)
            rmtree(stage)
        else:
            dest.parent.mkdir(parents=True, exist_ok=True)
            stage.rename(dest)
    else:
        raise ValueError(f"{task.id}: unknown setup kind {kind!r}")
    init_baseline(dest)
    for cmd in task.prepare:
        r = run_cmd(ctx.expand(cmd), cwd=dest, env=ctx.env(), timeout=900)
        if not r.ok:
            raise RuntimeError(f"{task.id}: prepare step {cmd} failed: {tail(r.err or r.out, 600)}")
    return dest


def install_hidden(task: Task, ws: Path) -> None:
    for dest_rel, src_rel in task.hidden.items():
        data = (task.root / src_rel).read_bytes()
        target = ws / dest_rel
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(data)
    if task.hidden_git:
        repo, commit = task.hidden_git["repo"], task.hidden_git["commit"]
        for rel in task.hidden_git.get("paths", []):
            data = git_bytes(repo, "show", f"{commit}:{rel}")
            target = ws / rel
            if data is None:
                # The fix deleted this test file.
                if target.exists():
                    target.unlink()
                continue
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(data)


_PASSED = [
    re.compile(r"test result: \w+\. (\d+) passed"),  # cargo
    re.compile(r"(\d+) passed"),  # pytest, jest ("Tests: 3 passed")
    re.compile(r"^ok\s+\S+", re.M),  # go
]


def count_passed(output: str) -> Optional[int]:
    """How many tests the output says passed (None when it says nothing recognisable)."""
    cargo = [int(n) for n in _PASSED[0].findall(output)]
    if cargo:
        return sum(cargo)
    m = _PASSED[1].findall(output)
    if m:
        return max(int(n) for n in m)
    go = _PASSED[2].findall(output)
    if go:
        return len(go)
    return None


@dataclass
class Grade:
    passed: bool
    output_tail: str = ""
    reason: str = ""
    tests_passed: Optional[int] = None


def grade(task: Task, ws: Path, ctx: Optional[Context] = None) -> Grade:
    """Installs the hidden files and runs the task's commands; passes when all exit 0 and tests ran."""
    ctx = ctx or Context()
    try:
        install_hidden(task, ws)
    except Exception as e:  # a grading failure is a result, not a crash of the whole matrix
        return Grade(False, reason=f"could not install the hidden tests: {e}")
    if not task.tests:
        return Grade(False, reason="the task has no test command")
    log = []
    total = 0
    unknown = False
    for cmd in task.tests:
        r = run_cmd(ctx.expand(cmd), cwd=ws, env=ctx.env(), timeout=task.test_timeout)
        log.append(f"$ {' '.join(cmd)}\n{r.out}\n{r.err}")
        if r.timed_out:
            return Grade(False, tail("\n".join(log)), f"tests timed out after {task.test_timeout}s")
        if r.code != 0:
            return Grade(False, tail("\n".join(log)), f"exit {r.code}")
        n = count_passed(r.out + "\n" + r.err)
        if n is None:
            unknown = True
        else:
            total += n
    text = "\n".join(log)
    if not unknown and total == 0:
        return Grade(False, tail(text), "no test ran", 0)
    return Grade(True, tail(text), "", None if unknown else total)


def diff_of_work(ws: Path) -> str:
    """The agent's changes against the starting commit, untracked files included."""
    r = run_cmd(["git", "add", "-A"], cwd=ws, timeout=120)
    if not r.ok:
        return ""
    return run_cmd(["git", "diff", "--cached", "--stat"], cwd=ws, timeout=120).out

