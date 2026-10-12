"""Which of the polyglot languages this machine can run today.

Nothing is installed for the eval (design 9.0: the installs for Go, uv and Docker were not approved).
A language whose toolchain is missing is reported as unavailable and its tasks are left out of the
score, never counted as failures.
"""

from __future__ import annotations

import re
import shutil
from dataclasses import dataclass
from typing import Callable, Optional

from .util import run_cmd


@dataclass
class Toolchain:
    language: str
    available: bool
    detail: str


def _python() -> Toolchain:
    import sys

    r = run_cmd([sys.executable, "-m", "pytest", "--version"], timeout=60)
    return Toolchain("python", r.ok, (r.out or r.err).strip().splitlines()[0] if (r.out or r.err).strip() else "pytest missing")


def _cargo() -> Toolchain:
    if not shutil.which("cargo"):
        return Toolchain("rust", False, "cargo not on PATH")
    r = run_cmd(["cargo", "--version"], timeout=60)
    return Toolchain("rust", r.ok, r.out.strip())


def _node() -> Toolchain:
    if not shutil.which("node") or not shutil.which("npm"):
        return Toolchain("javascript", False, "node or npm not on PATH")
    r = run_cmd(["node", "--version"], timeout=60)
    return Toolchain("javascript", r.ok, "node " + r.out.strip())


def _go() -> Toolchain:
    if not shutil.which("go"):
        return Toolchain("go", False, "go not installed (9.1-d was not approved)")
    r = run_cmd(["go", "version"], timeout=60)
    return Toolchain("go", r.ok, r.out.strip())


def _cpp() -> Toolchain:
    compiler = next((c for c in ("g++", "clang++", "cl") if shutil.which(c)), None)
    if not compiler or not shutil.which("cmake"):
        return Toolchain("cpp", False, "needs cmake and a C++ compiler on PATH" + ("" if compiler else " (none found)"))
    return Toolchain("cpp", True, f"cmake + {compiler}")


def _java() -> Toolchain:
    if not shutil.which("java") or not shutil.which("gradle"):
        return Toolchain("java", False, "needs java 17+ and gradle")
    r = run_cmd(["java", "-version"], timeout=60)
    m = re.search(r'version "(\d+)(?:\.(\d+))?', r.err + r.out)
    major = int(m.group(1)) if m else 0
    if major == 1:  # 1.8 reports itself as 1.8
        major = int(m.group(2) or 8)
    return Toolchain("java", major >= 17, f"java {major}" + ("" if major >= 17 else " (the polyglot Java tasks need 17+)"))


_DETECT: dict = {"python": _python, "rust": _cargo, "javascript": _node, "go": _go, "cpp": _cpp, "java": _java}

#: Every cargo run goes through the machine guard (one build at a time machine-wide, memory headroom,
#: the training hold file); `{cargo_idle}` is filled in from the Context.
CARGO_IDLE = ["powershell", "-NoProfile", "-File", "{cargo_idle}"]

#: Aider's test command per language (benchmark.py), and any one-time step before the agent starts.
COMMANDS = {
    "python": {"tests": [["{python}", "-m", "pytest", "-q", "-x"]], "prepare": []},
    "rust": {"tests": [CARGO_IDLE + ["test", "--", "--include-ignored"]], "prepare": []},
    "javascript": {"tests": [["npm", "test", "--silent"]], "prepare": [["npm", "install", "--no-audit", "--no-fund", "--loglevel=error"]]},
    "go": {"tests": [["go", "test", "./..."]], "prepare": []},
    "cpp": {"tests": [["cmake", "--build", "build"]], "prepare": []},
    "java": {"tests": [["gradle", "test", "-i"]], "prepare": []},
}


def detect(language: str) -> Toolchain:
    fn: Optional[Callable[[], Toolchain]] = _DETECT.get(language)
    return fn() if fn else Toolchain(language, False, "unknown language")


def detect_all() -> dict:
    return {lang: fn() for lang, fn in _DETECT.items()}
