"""The machine guard for the agent's own cargo runs.

Grading goes through ``scripts/cargo-idle.ps1`` (one build at a time machine-wide, memory
headroom, the training hold file). The agent runs cargo itself in a shell with no permission
prompts, so it needs the same guard: a ``cargo`` shim directory goes first on its ``PATH`` and
forwards every call to the script; the script finds the real cargo because the shim strips its own
directory from ``PATH`` first. Builds also get one job, one test thread and a shared target dir.
"""

from __future__ import annotations

import json
import os
import sys
from dataclasses import dataclass
from pathlib import Path

SHIM_PY = '''"""cargo shim written by the coding eval: forwards to the machine guard (see mcoding/guard.py)."""
import os
import subprocess
import sys

GUARD = {guard}
here = os.path.normcase(os.path.abspath(os.path.dirname(__file__)))
keep = [p for p in os.environ.get("PATH", "").split(os.pathsep) if os.path.normcase(os.path.abspath(p or ".")) != here]
env = dict(os.environ)
env["PATH"] = os.pathsep.join(keep)
sys.exit(subprocess.run(["powershell", "-NoProfile", "-File", GUARD, *sys.argv[1:]], env=env).returncode)
'''


@dataclass
class CargoGuard:
    """What an agent's environment needs so its cargo runs are guarded."""

    shim_dir: Path
    target_dir: str = ""

    def apply(self, env: dict) -> dict:
        """``env`` with the shim first on PATH and the cargo limits set (changes and returns it)."""
        key = next((k for k in env if k.upper() == "PATH"), "PATH")
        old = env.get(key, "")
        env[key] = str(self.shim_dir) + (os.pathsep + old if old else "")
        env["CARGO_BUILD_JOBS"] = "1"
        env["RUST_TEST_THREADS"] = "1"
        if self.target_dir:
            env["CARGO_TARGET_DIR"] = self.target_dir
        return env


def install_cargo_shim(shim_dir: Path, cargo_idle: str, python: str = "") -> CargoGuard:
    """Writes ``cargo.cmd`` (cmd, PowerShell), ``cargo`` (Git Bash) and ``cargo_shim.py`` into ``shim_dir``."""
    python = python or sys.executable
    shim_dir.mkdir(parents=True, exist_ok=True)
    (shim_dir / "cargo_shim.py").write_text(SHIM_PY.format(guard=json.dumps(str(cargo_idle))), encoding="utf-8", newline="\n")
    py_win, py_posix = str(python), Path(python).as_posix()
    script = shim_dir / "cargo_shim.py"
    (shim_dir / "cargo.cmd").write_text(f'@echo off\r\n"{py_win}" "{script}" %*\r\n', encoding="utf-8", newline="")
    (shim_dir / "cargo").write_text(f'#!/bin/sh\nexec "{py_posix}" "{script.as_posix()}" "$@"\n', encoding="utf-8", newline="\n")
    return CargoGuard(shim_dir=shim_dir)
