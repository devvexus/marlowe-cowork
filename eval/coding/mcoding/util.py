"""Small process and file helpers: children never open a window, always die with their tree."""

from __future__ import annotations

import json
import os
import re
import shutil
import signal
import subprocess
import sys
import threading
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Mapping, Optional, Sequence

WINDOWS = sys.platform == "win32"
# CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP: no console flashes on the desktop, and a Ctrl-C to the
# harness does not reach the child behind its back.
_FLAGS = 0x08000000 | 0x00000200 if WINDOWS else 0


@dataclass
class CmdResult:
    code: int
    out: str
    err: str
    timed_out: bool
    secs: float

    @property
    def ok(self) -> bool:
        return self.code == 0 and not self.timed_out


def kill_tree(pid: int) -> None:
    """Kills a process and everything it started."""
    if WINDOWS:
        subprocess.run(
            ["taskkill", "/F", "/T", "/PID", str(pid)],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            creationflags=_FLAGS,
            check=False,
        )
    else:
        try:
            os.killpg(os.getpgid(pid), signal.SIGKILL)
        except (ProcessLookupError, PermissionError):
            pass


def popen(argv: Sequence[str], *, cwd=None, env: Optional[Mapping[str, str]] = None, **kw) -> subprocess.Popen:
    """``subprocess.Popen`` with the harness's defaults (UTF-8 text, no window, own process group)."""
    kw.setdefault("text", True)
    kw.setdefault("encoding", "utf-8")
    kw.setdefault("errors", "replace")
    if WINDOWS:
        kw.setdefault("creationflags", _FLAGS)
    else:
        kw.setdefault("start_new_session", True)
    return subprocess.Popen(list(argv), cwd=cwd, env=dict(env) if env is not None else None, **kw)


def run_cmd(
    argv: Sequence[str],
    *,
    cwd=None,
    env: Optional[Mapping[str, str]] = None,
    timeout: float = 600,
    stdin_text: Optional[str] = None,
) -> CmdResult:
    """Runs a command to completion or to the deadline, then kills its whole tree."""
    started = time.monotonic()
    try:
        p = popen(
            argv,
            cwd=cwd,
            env=env,
            stdin=subprocess.PIPE if stdin_text is not None else subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
    except OSError as e:
        return CmdResult(127, "", f"cannot start {argv[0]!r}: {e}", False, 0.0)
    with p:
        try:
            out, err = p.communicate(stdin_text, timeout=timeout)
            return CmdResult(p.returncode, out or "", err or "", False, time.monotonic() - started)
        except subprocess.TimeoutExpired:
            kill_tree(p.pid)
            try:
                out, err = p.communicate(timeout=15)
            except subprocess.TimeoutExpired:
                out, err = "", ""
            return CmdResult(p.returncode if p.returncode is not None else -9, out or "", err or "", True, time.monotonic() - started)


@dataclass
class GuardedResult(CmdResult):
    #: the guard never let the command start within ``wait_cap`` (machine busy): not the command's fault
    never_started: bool = False


def is_guarded(argv: Sequence[str]) -> bool:
    """True when the command goes through ``scripts/cargo-idle.ps1`` (it may wait for the machine)."""
    return any(os.path.basename(str(a)).lower() == "cargo-idle.ps1" for a in argv)


def run_guarded(
    argv: Sequence[str],
    *,
    cwd=None,
    env: Optional[Mapping[str, str]] = None,
    timeout: float = 600,
    wait_cap: float = 6 * 3600,
) -> GuardedResult:
    """``run_cmd`` for a command behind the cargo-idle guard: ``timeout`` counts from when it starts.

    The guard waits, silently or with a ``[cargo-idle]`` line, for the machine-wide lock and for
    free memory; that wait is normal and is not the command's running time. The clock starts at the
    first line of output that is not the guard's own; ``wait_cap`` bounds the wait itself.
    """
    t0 = time.monotonic()
    try:
        p = popen(argv, cwd=cwd, env=env, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    except OSError as e:
        return GuardedResult(127, "", f"cannot start {argv[0]!r}: {e}", False, 0.0)
    bufs = {"out": [], "err": []}
    started = threading.Event()

    def pump(stream, key):
        for line in stream:
            bufs[key].append(line)
            if line.strip() and not line.startswith("[cargo-idle]"):
                started.set()

    threads = [threading.Thread(target=pump, args=(p.stdout, "out"), daemon=True), threading.Thread(target=pump, args=(p.stderr, "err"), daemon=True)]
    for t in threads:
        t.start()
    clock = None
    timed_out = never = False
    with p:
        while p.poll() is None:
            now = time.monotonic()
            if clock is None and started.is_set():
                clock = now
            if clock is not None and now - clock > timeout:
                timed_out = True
            elif clock is None and now - t0 > wait_cap:
                never = True
            if timed_out or never:
                kill_tree(p.pid)
                break
            time.sleep(0.2)
        try:
            p.wait(timeout=15)
        except subprocess.TimeoutExpired:
            pass
        for t in threads:
            t.join(timeout=5)
    code = p.returncode if p.returncode is not None else -9
    return GuardedResult(code, "".join(bufs["out"]), "".join(bufs["err"]), timed_out, time.monotonic() - t0, never)


def rmtree(path) -> None:
    """``shutil.rmtree`` that also removes read-only files (git objects on Windows)."""
    p = Path(path)
    if not p.exists():
        return

    def onerror(func, target, _exc):
        try:
            os.chmod(target, 0o700)
            func(target)
        except OSError:
            pass

    shutil.rmtree(p, onerror=onerror)


def read_text(path) -> str:
    return Path(path).read_text(encoding="utf-8", errors="replace")


def write_text(path, text: str) -> None:
    p = Path(path)
    p.parent.mkdir(parents=True, exist_ok=True)
    # newline="" so a fixture's line endings reach the disk exactly as written.
    with open(p, "w", encoding="utf-8", newline="") as f:
        f.write(text)


def read_json(path):
    return json.loads(read_text(path))


def write_json(path, value) -> None:
    write_text(path, json.dumps(value, indent=2, sort_keys=True) + "\n")


def which(name: str) -> Optional[str]:
    return shutil.which(name)


_SECRET = re.compile(r"(KEY|TOKEN|SECRET|PASSWORD|CREDENTIAL|PASSPHRASE|COOKIE)", re.I)
_VENDOR = re.compile(r"^(ANTHROPIC|CLAUDE|CLAUDECODE|OPENAI|OPENROUTER|GEMINI|GOOGLE|AWS|AZURE|HF|HUGGING|GITHUB|GH|XAI|MISTRAL|GROQ|DEEPSEEK|TOGETHER|FIREWORKS)_", re.I)
# Not secrets, whatever the name looks like.
_KEEP = {"KEYBOARD_LAYOUT", "SSH_AUTH_SOCK_PATH"}


def scrubbed_env(base: Optional[Mapping[str, str]] = None) -> dict:
    """``base`` (default: the process environment) without API keys, tokens, or vendor settings.

    The harness runs two coding agents against a local model for $0: nothing in the parent's
    environment should be able to point either at a paid endpoint or a signed-in account.
    """
    src = os.environ if base is None else base
    out = {}
    for k, v in src.items():
        if k in _KEEP:
            out[k] = v
        elif _VENDOR.match(k) or _SECRET.search(k):
            continue
        else:
            out[k] = v
    return out


def short(text: str, n: int = 400) -> str:
    text = text.strip()
    return text if len(text) <= n else text[: n - 3] + "..."


def tail(text: str, n: int = 2000) -> str:
    return text if len(text) <= n else "..." + text[-n:]
