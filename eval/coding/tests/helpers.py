"""Shared pieces of the harness tests."""

from __future__ import annotations

import json
import shutil
import subprocess
import sys
from pathlib import Path

from mcoding.adapters import ClaudeCodeAdapter, MarloweAdapter

HERE = Path(__file__).resolve().parent
FAKE = str(HERE / "fake_agent.py")
CODING = HERE.parent
MICRO = CODING / "micro"


def plan_file(tmp: Path, plans, name: str = "plan.json") -> str:
    p = tmp / name
    p.write_text(json.dumps(plans), encoding="utf-8")
    return str(p)


def fake_marlowe(tmp: Path, plans, *, name="marlowe-full", shell_only=False, memory=False, daemon=False, extra_env=None) -> tuple:
    """A Marlowe adapter whose binary is the fake agent; returns (adapter, record file)."""
    record = tmp / f"record-{name}.jsonl"
    env = {"FAKE_PLAN": plan_file(tmp, plans, f"plan-{name}.json"), "FAKE_RECORD": str(record), **(extra_env or {})}
    a = MarloweAdapter(
        sys.executable,
        prefix=[FAKE],
        name=name,
        model="ollama/fake",
        home=tmp / "homes" / name,
        shell_only=shell_only,
        memory=memory,
        daemon=daemon,
        env_extra=env,
    )
    return a, record


def fake_cc(tmp: Path, plans, *, name="cc-local", extra_env=None) -> tuple:
    record = tmp / f"record-{name}.jsonl"
    claude = Path(sys.executable)
    a = ClaudeCodeAdapter(claude, prefix=[FAKE], config_dir=tmp / "cc-config", name=name, model="fake-model")
    # The fake reads its plan from these; ClaudeCodeAdapter builds its own isolated env, so add them to it.
    base = a.invocation

    def invocation(prompt, ws, **kw):
        inv = base(prompt, ws, **kw)
        inv.env.update({"FAKE_PLAN": plan_file(tmp, plans, f"plan-{name}.json"), "FAKE_RECORD": str(record), **(extra_env or {})})
        return inv

    a.invocation = invocation  # type: ignore[method-assign]
    base_session = a.session_invocation

    def session_invocation(ws, **kw):
        inv = base_session(ws, **kw)
        inv.env.update({"FAKE_PLAN": plan_file(tmp, plans, f"plan-{name}.json"), "FAKE_RECORD": str(record), **(extra_env or {})})
        return inv

    a.session_invocation = session_invocation  # type: ignore[method-assign]
    return a, record


def records(path: Path) -> list:
    if not path.is_file():
        return []
    return [json.loads(l) for l in path.read_text(encoding="utf-8").splitlines() if l.strip()]


def git(cwd, *args) -> str:
    r = subprocess.run(["git", *args], cwd=cwd, capture_output=True, text=True, encoding="utf-8", check=True)
    return r.stdout


def commit_all(repo: Path, msg: str) -> str:
    git(repo, "add", "-A")
    git(repo, "-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "--allow-empty", "-m", msg)
    return git(repo, "rev-parse", "HEAD").strip()


def init_repo(repo: Path) -> None:
    repo.mkdir(parents=True, exist_ok=True)
    git(repo, "init", "-q")
    git(repo, "config", "core.autocrlf", "false")


def copy_micro(task_id: str, dest: Path) -> Path:
    shutil.copytree(MICRO / task_id, dest / task_id)
    return dest / task_id
