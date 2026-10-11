"""A small client for ``marlowe -p --input-format stream-json --output-format stream-json``.

The protocol is Claude Code's SDK stream-json (design 2.7 H2/H5/H6): one JSON object per line each
way. This module is standard library only so the eval harness and ad hoc scripts can drive a
Marlowe turn without installing anything::

    from marlowe_client import MarloweClient

    def gate(tool, tool_input):
        return {"behavior": "allow"} if tool != "Bash" else {"behavior": "deny", "message": "no shell"}

    with MarloweClient(cwd="C:/repo", can_use_tool=gate, permission_mode="default") as m:
        turn = m.ask("summarise the failing test")
        print(turn.text, turn.cost_usd, turn.exit_code)
        turn = m.ask("now fix it")      # same session, same process

``query`` is the one-shot form (``--output-format json``).
"""

from __future__ import annotations

import json
import os
import queue
import subprocess
import threading
import time
from dataclasses import dataclass, field
from typing import Any, Callable, Iterator, Optional, Sequence

#: ``can_use_tool`` answers: ``{"behavior": "allow"}`` (optionally ``updatedInput``) or
#: ``{"behavior": "deny", "message": "..."}``.
Decision = dict
CanUseTool = Callable[[str, dict], Decision]

EXIT_OK, EXIT_ERROR, EXIT_APPROVAL, EXIT_BUDGET, EXIT_CANCELLED = 0, 1, 2, 3, 130


class MarloweError(RuntimeError):
    """The process ended, or sent something that is not the protocol."""


@dataclass
class Turn:
    """One finished turn: the ``result`` line plus everything that came before it."""

    result: dict
    events: list = field(default_factory=list)

    @property
    def text(self) -> str:
        return self.result.get("result", "")

    @property
    def is_error(self) -> bool:
        return bool(self.result.get("is_error"))

    @property
    def subtype(self) -> str:
        return self.result.get("subtype", "")

    @property
    def exit_code(self) -> int:
        """What ``marlowe -p`` exits with if this were the last turn (0/1/2/3/130)."""
        return int(self.result.get("exit_code", EXIT_OK))

    @property
    def session_id(self) -> str:
        return self.result.get("session_id", "")

    @property
    def cost_usd(self) -> float:
        return float(self.result.get("total_cost_usd", 0.0))

    @property
    def denials(self) -> list:
        return list(self.result.get("permission_denials", []))

    @property
    def usage(self) -> dict:
        return dict(self.result.get("usage", {}))

    def tool_uses(self) -> list:
        """``(name, input)`` of every tool call the model made this turn, in order."""
        out = []
        for e in self.events:
            if e.get("type") == "assistant":
                for block in e.get("message", {}).get("content", []):
                    if block.get("type") == "tool_use":
                        out.append((block["name"], block.get("input", {})))
        return out


class MarloweClient:
    """Owns one ``marlowe -p`` process in stream-json mode and runs turns through it."""

    def __init__(
        self,
        *,
        marlowe: str = "marlowe",
        cwd: Optional[str] = None,
        args: Sequence[str] = (),
        env: Optional[dict] = None,
        can_use_tool: Optional[CanUseTool] = None,
        permission_mode: Optional[str] = None,
        model: Optional[str] = None,
        standalone: bool = False,
        include_partial_messages: bool = False,
        on_event: Optional[Callable[[dict], None]] = None,
        idle_timeout: float = 1800.0,
    ) -> None:
        self._argv = [marlowe, "-p", "--input-format", "stream-json", "--output-format", "stream-json"]
        if can_use_tool is not None:
            self._argv += ["--permission-prompt-tool", "stdio"]
        if permission_mode:
            self._argv += ["--permission-mode", permission_mode]
        if model:
            self._argv += ["--model", model]
        if standalone:
            self._argv.append("--standalone")
        if include_partial_messages:
            self._argv.append("--include-partial-messages")
        if cwd:
            self._argv += ["--workspace", cwd]
        self._argv += list(args)
        self._env = {**os.environ, **(env or {})}
        self._cwd = cwd
        self._can_use_tool = can_use_tool
        self._on_event = on_event
        self._idle = idle_timeout
        self._proc: Optional[subprocess.Popen] = None
        self._lines: "queue.Queue[Optional[dict]]" = queue.Queue()
        self._write_lock = threading.Lock()
        self._next_id = 0
        self._responses: dict = {}
        self.init: dict = {}
        self.stderr = ""

    # ------------------------------------------------------------ lifecycle

    def start(self) -> dict:
        """Starts the process and returns its ``system``/``init`` line."""
        self._proc = subprocess.Popen(
            self._argv,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            cwd=self._cwd,
            env=self._env,
            text=True,
            encoding="utf-8",
            bufsize=1,
        )
        threading.Thread(target=self._read_stdout, daemon=True).start()
        threading.Thread(target=self._read_stderr, daemon=True).start()
        # The init line comes with the first turn (the session opens then); initialize asks early.
        reply = self.control({"subtype": "initialize"})
        self.init = reply or {}
        return self.init

    def close(self) -> int:
        """Closes stdin (the run ends after its current turn) and returns the exit code."""
        p = self._proc
        if p is None:
            return EXIT_OK
        try:
            if p.stdin and not p.stdin.closed:
                p.stdin.close()
        except OSError:
            pass
        try:
            code = p.wait(timeout=30)
        except subprocess.TimeoutExpired:
            p.kill()
            code = p.wait()
        for pipe in (p.stdout, p.stderr):
            try:
                if pipe:
                    pipe.close()
            except OSError:
                pass
        return code

    def __enter__(self) -> "MarloweClient":
        self.start()
        return self

    def __exit__(self, *exc: Any) -> None:
        self.close()

    # ------------------------------------------------------------ plumbing

    def _read_stdout(self) -> None:
        assert self._proc and self._proc.stdout
        for raw in self._proc.stdout:
            raw = raw.strip()
            if not raw:
                continue
            try:
                self._lines.put(json.loads(raw))
            except json.JSONDecodeError:
                self._lines.put({"type": "_garbage", "line": raw})
        self._lines.put(None)

    def _read_stderr(self) -> None:
        assert self._proc and self._proc.stderr
        for raw in self._proc.stderr:
            self.stderr += raw

    def _write(self, obj: dict) -> None:
        assert self._proc and self._proc.stdin
        with self._write_lock:
            try:
                self._proc.stdin.write(json.dumps(obj) + "\n")
                self._proc.stdin.flush()
            except (BrokenPipeError, OSError) as e:
                raise MarloweError(f"marlowe is not reading stdin: {e}; stderr: {self.stderr.strip()}") from e

    def _events(self, until: Callable[[dict], bool]) -> Iterator[dict]:
        """Yields lines, answering control requests, until ``until`` accepts one."""
        deadline = time.monotonic() + self._idle
        while True:
            try:
                ev = self._lines.get(timeout=1.0)
            except queue.Empty:
                if time.monotonic() > deadline:
                    raise MarloweError(f"no output for {self._idle:.0f}s")
                continue
            deadline = time.monotonic() + self._idle
            if ev is None:
                raise MarloweError(f"marlowe ended before the turn finished; stderr: {self.stderr.strip()}")
            if self._on_event:
                self._on_event(ev)
            kind = ev.get("type")
            if kind == "control_request":
                self._answer(ev)
                continue
            if kind == "control_response":
                r = ev.get("response", {})
                self._responses[r.get("request_id")] = r
            yield ev
            if until(ev):
                return

    def _answer(self, ev: dict) -> None:
        req = ev.get("request", {})
        rid = ev.get("request_id", "")
        if req.get("subtype") != "can_use_tool":
            self._write({"type": "control_response", "response": {"subtype": "error", "request_id": rid, "error": f"unsupported {req.get('subtype')!r}"}})
            return
        try:
            decision = self._can_use_tool(req.get("tool_name", ""), req.get("input", {})) if self._can_use_tool else {"behavior": "deny", "message": "no handler"}
            if decision is None:
                decision = {"behavior": "deny", "message": "no decision"}
            self._write({"type": "control_response", "response": {"subtype": "success", "request_id": rid, "response": decision}})
        except MarloweError:
            raise
        except Exception as e:  # a handler's bug is a denial, never a hang
            self._write({"type": "control_response", "response": {"subtype": "error", "request_id": rid, "error": f"handler failed: {e}"}})

    # ------------------------------------------------------------ the API

    def control(self, request: dict, timeout: float = 30.0) -> Optional[dict]:
        """Sends a ``control_request`` and returns its ``response`` payload (None on an error reply)."""
        self._next_id += 1
        rid = f"c{self._next_id}"
        self._write({"type": "control_request", "request_id": rid, "request": request})
        end = time.monotonic() + timeout
        while rid not in self._responses:
            if time.monotonic() > end:
                raise MarloweError(f"no answer to {request.get('subtype')}")
            # Only control responses matter here; a turn's events are read by ask().
            for _ in self._events(lambda e: e.get("type") == "control_response" and e.get("response", {}).get("request_id") == rid):
                pass
        r = self._responses.pop(rid)
        return r.get("response") if r.get("subtype") == "success" else None

    def ask(self, text: str) -> Turn:
        """Sends a user message and returns when its turn ends."""
        self._write({"type": "user", "message": {"role": "user", "content": text}})
        events: list = []
        for ev in self._events(lambda e: e.get("type") == "result"):
            events.append(ev)
        return Turn(result=events[-1], events=events[:-1])

    def interrupt(self) -> None:
        """Cancels the running turn (it ends with exit code 130)."""
        self._next_id += 1
        self._write({"type": "control_request", "request_id": f"c{self._next_id}", "request": {"subtype": "interrupt"}})

    def set_permission_mode(self, mode: str) -> None:
        self.control({"subtype": "set_permission_mode", "mode": mode})


def query(prompt: str, *, marlowe: str = "marlowe", cwd: Optional[str] = None, args: Sequence[str] = (), env: Optional[dict] = None, timeout: float = 1800.0) -> Turn:
    """One turn, one process: ``marlowe -p --output-format json``."""
    argv = [marlowe, "-p", "--output-format", "json"]
    if cwd:
        argv += ["--workspace", cwd]
    argv += list(args) + [prompt]
    done = subprocess.run(argv, capture_output=True, text=True, encoding="utf-8", cwd=cwd, env={**os.environ, **(env or {})}, timeout=timeout)
    try:
        result = json.loads(done.stdout)
    except json.JSONDecodeError as e:
        raise MarloweError(f"marlowe exited {done.returncode} without a result: {done.stderr.strip()}") from e
    result.setdefault("exit_code", done.returncode)
    return Turn(result=result)
