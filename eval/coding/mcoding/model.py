"""The harness's data: tasks, what an agent run produced, and one row per (task, arm, seed).

A task is a folder with ``task.json``::

    {"id": "micro-01-clamp", "tier": "micro", "language": "python",
     "prompt": "...", "setup": {"kind": "dir", "path": "repo"},
     "hidden": {"tests/test_hidden.py": "hidden/test_hidden.py"},
     "tests": [["python", "-m", "pytest", "-q"]], "timeout": 900}

``setup`` is ``dir`` (copy a folder) or ``git`` (check a commit out of a repository, put the files
a later fix changed back as they were, and drop the history, so the agent cannot read the fix).
``hidden`` files are copied in after the agent finishes and before the tests run. A task passes when
every command in ``tests`` exits 0 and at least one test ran.
"""

from __future__ import annotations

import json
from dataclasses import dataclass, field
from pathlib import Path
from typing import Optional

from .util import read_json


@dataclass
class Task:
    id: str
    tier: str
    prompt: str
    root: Path
    language: str = "python"
    setup: dict = field(default_factory=lambda: {"kind": "dir", "path": "repo"})
    #: destination path in the workspace -> source file under the task root
    hidden: dict = field(default_factory=dict)
    #: for ``git`` setups: files taken from the fix commit at grading time
    hidden_git: dict = field(default_factory=dict)
    tests: list = field(default_factory=list)
    #: commands run once before the agent starts (``npm install``)
    prepare: list = field(default_factory=list)
    timeout: int = 1200
    test_timeout: int = 900
    #: attempts the protocol allows (polyglot: 2, the second sees the first's test output)
    attempts: int = 1
    #: files the prompt names (polyglot)
    files: list = field(default_factory=list)
    meta: dict = field(default_factory=dict)

    @staticmethod
    def load(folder) -> "Task":
        root = Path(folder)
        d = read_json(root / "task.json")
        known = {f for f in Task.__dataclass_fields__}  # type: ignore[attr-defined]
        extra = {k: v for k, v in d.items() if k not in known}
        d = {k: v for k, v in d.items() if k in known}
        t = Task(root=root, **d)
        t.meta = {**t.meta, **extra}
        return t

    def to_json(self) -> dict:
        d = {k: getattr(self, k) for k in self.__dataclass_fields__ if k != "root"}  # type: ignore[attr-defined]
        return d


@dataclass
class AgentResult:
    """What one agent invocation produced (numbers are None when the harness does not report them)."""

    exit_code: int = 0
    timed_out: bool = False
    seconds: float = 0.0
    steps: Optional[int] = None
    input_tokens: Optional[int] = None
    output_tokens: Optional[int] = None
    cached_tokens: Optional[int] = None
    cost_usd: Optional[float] = None
    api_ms: Optional[int] = None
    tool_calls: int = 0
    tool_errors: int = 0
    #: tool-call repairs the harness made (Marlowe reports them; Claude Code has none)
    repairs: Optional[int] = None
    #: model prefill / decode milliseconds, when a harness can say (neither does today)
    prefill_ms: Optional[int] = None
    decode_ms: Optional[int] = None
    final_text: str = ""
    is_error: bool = False
    subtype: str = ""
    #: tool names the arm should not have been able to use
    violations: list = field(default_factory=list)
    error: str = ""
    transcript: str = ""
    stderr_tail: str = ""

    def merge(self, other: "AgentResult") -> "AgentResult":
        """Totals for a task that took two attempts."""

        def add(a, b):
            return b if a is None else a if b is None else a + b

        self.exit_code = other.exit_code
        self.timed_out = self.timed_out or other.timed_out
        self.seconds += other.seconds
        for f in ("steps", "input_tokens", "output_tokens", "cached_tokens", "cost_usd", "api_ms", "repairs"):
            setattr(self, f, add(getattr(self, f), getattr(other, f)))
        self.tool_calls += other.tool_calls
        self.tool_errors += other.tool_errors
        self.final_text = other.final_text or self.final_text
        self.is_error = other.is_error
        self.subtype = other.subtype or self.subtype
        self.violations = sorted(set(self.violations) | set(other.violations))
        self.error = other.error or self.error
        self.stderr_tail = other.stderr_tail or self.stderr_tail
        return self


@dataclass
class RunRecord:
    """One row of ``records.jsonl``."""

    run: str
    task: str
    tier: str
    arm: str
    seed: int
    passed: bool
    attempts_used: int = 1
    graded: bool = True
    skip_reason: str = ""
    test_output_tail: str = ""
    agent: dict = field(default_factory=dict)
    meta: dict = field(default_factory=dict)

    def to_json(self) -> str:
        return json.dumps(self.__dict__, sort_keys=True)

    @staticmethod
    def from_json(line: str) -> "RunRecord":
        return RunRecord(**json.loads(line))


def load_records(path) -> list:
    """The records file, one record per (run, task, arm, seed): a later line (a rerun) replaces an earlier one."""
    out: dict = {}
    p = Path(path)
    if not p.is_file():
        return []
    for line in p.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if line:
            r = RunRecord.from_json(line)
            out[(r.run, r.task, r.arm, r.seed)] = r
    return list(out.values())


def load_tasks(folder) -> list:
    """Every task folder directly under ``folder`` (those with a ``task.json``), sorted by id."""
    root = Path(folder)
    tasks = [Task.load(p.parent) for p in sorted(root.glob("*/task.json"))]
    return sorted(tasks, key=lambda t: t.id)

