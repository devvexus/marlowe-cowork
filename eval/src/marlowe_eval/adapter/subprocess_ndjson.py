"""CONTRACTS.md section 4.0 -- the subprocess transport.

The harness spawns a process, writes request frames on its stdin, reads response frames from
its stdout, and closes stdin to end the run. This is the one component of M0a that was
deliberately left unwritten until section 4.0 was pinned (2026-08-02); `base.py`,
`registry.py` and `canonical.py` each name it as deferred, and `canonical.py` already
specifies the behaviour of the deadline flag below.

It adds no field that carries meaning and it is not a fourth interface. Section 4.0.8:
*"the transport contributes nothing to a run's identity."* For a given section 4 body the
frame bytes are a pure function of that body -- no ids, no timestamps, no retries, no
concurrency.

Four things here are easy to get wrong and are therefore explicit:

  * **Binary pipes, never text mode.** Python's text mode performs universal-newline
    translation, which silently rewrites a `\\r\\n` terminator to `\\n`. Section 4.0.2 makes
    `\\r\\n` a protocol error, so text mode would delete the very defect the rule exists to
    catch -- the same "the test goes green because the failing path stopped existing" shape
    this project has logged repeatedly.
  * **Bounded reads.** `readline()` on a hostile or broken target is unbounded. Section
    4.0.2 caps a frame at 64 MiB, so the cap is enforced while reading, not after.
  * **No retry, ever** (section 4.0.7). A retry makes the outcome depend on timing.
  * **The deadline is the harness's only wall-clock decision**, and it exists solely to bound
    a hung process. It is deliberately far above any budget an implementation is scored
    against: an implementation missing its 300 ms P95 is *a result the eval exists to
    produce*, never a reason a run cannot report.
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import threading
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from ..contract import Interface, ProtocolError, ProtocolErrorKind
from ..contract.errors import (
    CLASS_A_BY_WIRE_KIND,
    CLASS_B_WIRE_KINDS,
    TRANSPORT_KINDS,
)
from .base import ImplementationFailure, MemorySystem

# Section 4.0.2. An overlong line is `malformed_frame`.
MAX_FRAME_BYTES = 64 * 1024 * 1024

# Section 4.0.7: "no lower than 100x the request's max_latency_ms, floored at 30 s."
DEADLINE_MULTIPLE = 100
DEADLINE_FLOOR_S = 30.0

_READ_CHUNK = 65536


class TransportError(ProtocolError):
    """A section 4.0 violation, or a failure that leaves no section 4 response.

    Subclasses `ProtocolError` on purpose. `runner.py` catches `ProtocolError` per suite,
    records it, and carries on to the next one, which is what section 4.0.7 asks for when it
    says a crashed run still emits a report -- *"because a crash is a result."*

    The `kind` is a real `ProtocolErrorKind` member spelled exactly as section 4.0.4 spells
    it, and the constructor **refuses any kind outside `TRANSPORT_KINDS`**. An earlier draft
    carried the true name in a side attribute while the typed field held an approximation;
    that is the unobservable-mismatch pattern in miniature -- two fields disagreeing, with
    only the untyped one correct -- so the taxonomy is checked here instead.
    """

    def __init__(
        self,
        kind: ProtocolErrorKind,
        interface: Interface,
        detail: str,
        *,
        location: str | None = None,
    ) -> None:
        if kind not in TRANSPORT_KINDS:
            raise AssertionError(
                f"{kind!r} is not a transport kind. A section 4 payload violation must not "
                "be raised as a transport failure: the two vocabularies are disjoint so a "
                "report can say which layer failed."
            )
        super().__init__(kind, interface, detail, location=location)


def minimal_env() -> dict[str, str]:
    """Section 4.0.9: a declared minimal environment.

    *"The harness spawns the target's argv unmodified, with a declared minimal environment,
    so a run does not inherit ambient state a third party cannot reproduce."*

    Declared, not filtered by heuristic -- the list is the contract. Nothing is invented
    either: no variable announces that the target is running under eval, because an
    implementation that can detect the eval can behave differently under it, and then the
    eval measures the detection rather than the system.
    """
    keep = (
        # POSIX
        "PATH", "HOME", "LANG", "LC_ALL", "TZ",
        # Windows: these are not optional. Without SYSTEMROOT a process loses crypto,
        # sockets and the loader's default search path.
        "SYSTEMROOT", "WINDIR", "COMSPEC", "PATHEXT",
        "TEMP", "TMP", "NUMBER_OF_PROCESSORS", "PROCESSOR_ARCHITECTURE",
    )
    return {k: os.environ[k] for k in keep if k in os.environ}


@dataclass
class SubprocessTarget(MemorySystem):
    """A section 4 implementation reached over NDJSON on stdio.

    Strictly serial (section 4.0.5): one request is written, exactly one response is read
    before the next is written. There is never more than one outstanding request, so there
    are no correlation ids -- correlation is positional, and additionally checked by
    `Client` through `check_correlation`.
    """

    argv: list[str]
    name: str = "exec"
    cwd: str | None = None
    cleanup_dirs: tuple[Path, ...] = ()
    exit_code: int | None = field(default=None, init=False)

    _proc: subprocess.Popen[bytes] | None = field(default=None, init=False, repr=False)
    _buf: bytearray = field(default_factory=bytearray, init=False, repr=False)
    _closed: bool = field(default=False, init=False, repr=False)
    _crashed: bool = field(default=False, init=False, repr=False)

    def __post_init__(self) -> None:
        if not self.argv:
            raise ValueError("exec target needs a command")
        try:
            self._proc = subprocess.Popen(
                self.argv,
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                # Section 4.0.1: stderr is diagnostic only and is NEVER parsed. Inheriting
                # rather than capturing means a third party sees the target's own
                # diagnostics on their terminal, where they are useful.
                stderr=None,
                cwd=self.cwd,
                env=minimal_env(),
                bufsize=0,  # raw pipes; see the module docstring on text mode
            )
        except OSError as exc:
            raise TransportError(
                ProtocolErrorKind.SPAWN_FAILED,
                Interface.INGEST,
                f"could not spawn {self.argv!r}: {exc}",
            ) from None

    # -- the MemorySystem contract ---------------------------------------------------

    def call(self, op: Interface, body: dict[str, Any]) -> dict[str, Any]:
        self._write_frame(op, body)
        frame = self._read_frame(op, deadline_s=_deadline_for(body))
        return self._unwrap(op, frame)

    def close(self) -> None:
        """Section 4.0.6: the harness signals end of run by closing stdin."""
        if self._closed:
            return
        self._closed = True
        proc = self._proc
        if proc is not None:
            try:
                if proc.stdin is not None and not proc.stdin.closed:
                    proc.stdin.close()
                # The implementation flushes and exits 0. Bound the wait so a target that
                # ignores EOF cannot hang the harness.
                try:
                    code = proc.wait(timeout=DEADLINE_FLOOR_S)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    code = proc.wait(timeout=5)
                # Deliberately not raised. close() runs inside `finally` blocks in
                # runner.py, and raising here would replace whatever real failure was
                # already in flight with this one. It is printed loudly instead, and
                # `exit_code` is left on the object. Carrying it into report.json needs a
                # runner.py change -- reported, not silently widened into.
                if code not in (0, None) and not self._crashed:
                    print(
                        f"WARNING: exec target {self.name!r} exited {code} at shutdown; "
                        "section 4.0.6 says a conforming implementation flushes and exits 0",
                        file=sys.stderr,
                    )
                self.exit_code = code
            except OSError:
                pass
        for path in self.cleanup_dirs:
            _rmtree_quiet(path)

    # -- framing ---------------------------------------------------------------------

    def _write_frame(self, op: Interface, body: dict[str, Any]) -> None:
        proc = self._require_live(op)
        # Section 4.0.3: `body` is the section 4 JSON verbatim, byte for byte. No
        # compression, no batching, no envelope metadata. separators= keeps the frame free
        # of incidental whitespace; a literal newline inside it would be a protocol error,
        # and RFC 8259 escaping means a correctly serialized value cannot contain one.
        line = json.dumps(
            {"op": op.value, "body": body},
            ensure_ascii=False,
            separators=(",", ":"),
        ).encode("utf-8")
        assert b"\n" not in line, "harness emitted a multi-line frame"
        try:
            assert proc.stdin is not None
            proc.stdin.write(line + b"\n")
            proc.stdin.flush()
        except (BrokenPipeError, OSError):
            self._crashed = True
            raise TransportError(
                ProtocolErrorKind.IMPLEMENTATION_CRASHED,
                op,
                "the target closed stdin or exited before the request was written; "
                "section 4.0.7 forbids a retry, because a retry makes the outcome depend "
                "on timing",
            ) from None

    def _read_frame(self, op: Interface, *, deadline_s: float) -> dict[str, Any]:
        raw = self._read_line(op, deadline_s=deadline_s)

        # Section 4.0.2: a frame is terminated by a single \n. A \r\n terminator is a
        # protocol error. This is only visible because the pipe is in binary mode.
        if raw.endswith(b"\r"):
            raise TransportError(
                ProtocolErrorKind.MALFORMED_FRAME,
                op,
                "frame terminated with CRLF; section 4.0.2 requires a single LF and tells "
                "implementations on Windows to set stdout to binary mode",
            )

        try:
            text = raw.decode("utf-8")
        except UnicodeDecodeError as exc:
            raise TransportError(
                ProtocolErrorKind.MALFORMED_FRAME,
                op,
                f"frame is not valid UTF-8: {exc}",
            ) from None

        if text.startswith("﻿"):
            raise TransportError(
                ProtocolErrorKind.MALFORMED_FRAME,
                op,
                "frame carries a byte-order mark; section 4.0.2 pins UTF-8 with no BOM",
            )

        try:
            frame = json.loads(text)
        except json.JSONDecodeError as exc:
            # Pretty-printed output lands here: reading to the first LF yields an
            # incomplete value. Section 4.0.2 calls that a protocol error rather than a
            # tolerated variation, and this is where it is caught.
            raise TransportError(
                ProtocolErrorKind.MALFORMED_FRAME,
                op,
                f"frame is not a JSON value: {exc}. Pretty-printed output is a protocol "
                "error, not a tolerated variation",
            ) from None

        if not isinstance(frame, dict):
            raise TransportError(
                ProtocolErrorKind.MALFORMED_FRAME,
                op,
                f"frame is a {type(frame).__name__}, not a JSON object",
            )
        return frame

    def _read_line(self, op: Interface, *, deadline_s: float) -> bytes:
        """One frame's bytes, without the terminator. Bounded in size and in time."""
        while True:
            idx = self._buf.find(b"\n")
            if idx >= 0:
                line = bytes(self._buf[:idx])
                del self._buf[: idx + 1]
                return line
            if len(self._buf) > MAX_FRAME_BYTES:
                raise TransportError(
                    ProtocolErrorKind.MALFORMED_FRAME,
                    op,
                    f"frame exceeds the {MAX_FRAME_BYTES} byte cap in section 4.0.2 with "
                    "no terminator seen",
                )
            chunk = self._read_chunk(op, deadline_s=deadline_s)
            if not chunk:
                self._crashed = True
                raise TransportError(
                    ProtocolErrorKind.IMPLEMENTATION_CRASHED,
                    op,
                    "EOF or process exit mid-request. Section 4.0.7: no retry -- the run is "
                    "marked crashed and remaining units are not attempted",
                )
            self._buf += chunk

    def _read_chunk(self, op: Interface, *, deadline_s: float) -> bytes:
        """A blocking read with a ceiling.

        The read runs on a worker thread because a timed read from a pipe is not portable:
        `selectors` does not accept pipe handles on Windows, which is the development
        platform per ADR-002 (revised).
        """
        proc = self._require_live(op)
        assert proc.stdout is not None
        fd = proc.stdout.fileno()

        box: dict[str, Any] = {}

        def _read() -> None:
            try:
                box["data"] = os.read(fd, _READ_CHUNK)
            except OSError as exc:  # pipe closed under us
                box["error"] = exc

        worker = threading.Thread(target=_read, daemon=True)
        worker.start()
        worker.join(timeout=deadline_s)

        if worker.is_alive():
            # Section 4.0.7: a harness-imposed deadline is a wall-clock decision by the
            # harness, and the run is marked `timing_tainted` -- no headline report, hash
            # not comparable. Killing the target is what unblocks the worker thread.
            proc.kill()
            self._crashed = True
            raise TransportError(
                ProtocolErrorKind.DEADLINE_EXCEEDED,
                op,
                f"no response within {deadline_s:.0f}s. This is the harness bounding a hung "
                "process, NOT a budget miss: an implementation exceeding "
                "budget.max_latency_ms returns a valid response and is scored normally. "
                "The run is timing_tainted -- its hash is not comparable",
            )
        if "error" in box:
            self._crashed = True
            raise TransportError(
                ProtocolErrorKind.IMPLEMENTATION_CRASHED,
                op,
                f"read failed mid-request: {box['error']}",
            )
        return box.get("data", b"")

    # -- frame rules (section 4.0.3) and the error taxonomy (section 4.0.4) -----------

    def _unwrap(self, op: Interface, frame: dict[str, Any]) -> dict[str, Any]:
        keys = set(frame)
        allowed = {"op", "body", "error"}
        extra = keys - allowed
        if extra:
            raise TransportError(
                ProtocolErrorKind.MALFORMED_FRAME,
                op,
                f"undeclared frame-level key(s): {sorted(extra)}. Section 4.0.3 permits "
                "only op, and exactly one of body or error",
                location=sorted(extra)[0],
            )
        if "op" not in frame:
            raise TransportError(
                ProtocolErrorKind.MALFORMED_FRAME,
                op,
                "frame has no `op`",
                location="op",
            )
        # Section 4.0.3: `op` must echo the request's. A desynchronized stream has to fail
        # loudly rather than misattribute a result to the wrong unit.
        if frame["op"] != op.value:
            raise TransportError(
                ProtocolErrorKind.UNKNOWN_OP,
                op,
                f"sent op={op.value!r}, received op={frame['op']!r}",
                location="op",
            )

        has_body = "body" in frame
        has_error = "error" in frame
        if has_body == has_error:
            raise TransportError(
                ProtocolErrorKind.MALFORMED_FRAME,
                op,
                "section 4.0.3 requires exactly one of `body` and `error`; this frame has "
                + ("both" if has_body else "neither"),
            )

        if has_body:
            body = frame["body"]
            if not isinstance(body, dict):
                raise TransportError(
                    ProtocolErrorKind.MALFORMED_FRAME,
                    op,
                    f"`body` is a {type(body).__name__}, not a JSON object",
                    location="body",
                )
            return body

        return self._raise_for_error(op, frame["error"])

    def _raise_for_error(self, op: Interface, error: Any) -> dict[str, Any]:
        """Section 4.0.4. `error` is the ABSENCE of a section 4 response, not a variant.

        Note what does NOT come through here: section 4.6's `rejected` is a *successful*
        response -- the implementation understood the request and refused a write -- and it
        travels in `body`. The poisoning suite asserts on exactly that, so conflating the
        two would make a visible refusal indistinguishable from a failure.
        """
        if not isinstance(error, dict) or "kind" not in error:
            raise TransportError(
                ProtocolErrorKind.MALFORMED_FRAME,
                op,
                f"`error` must be an object carrying `kind`, got {error!r}",
                location="error",
            )
        kind = str(error["kind"])
        detail = str(error.get("detail", ""))

        if kind in CLASS_B_WIRE_KINDS:
            # Class B: the system under test failed on a well-formed request. A RESULT --
            # the unit is scored as failed and the run continues.
            raise ImplementationFailure(detail or kind)

        mapped = CLASS_A_BY_WIRE_KIND.get(kind)
        if mapped is not None:
            # Class A: the harness or the version pairing is at fault. A defect, not a
            # measurement. Abort.
            raise TransportError(mapped, op, detail or "no detail supplied")

        raise TransportError(
            ProtocolErrorKind.MALFORMED_FRAME,
            op,
            f"error.kind {kind!r} is outside the closed set in section 4.0.4 "
            f"(class A: {sorted(CLASS_A_BY_WIRE_KIND)}; class B: {sorted(CLASS_B_WIRE_KINDS)})",
            location="error.kind",
        )

    def _require_live(self, op: Interface) -> subprocess.Popen[bytes]:
        proc = self._proc
        if proc is None or self._closed:
            raise TransportError(
                ProtocolErrorKind.IMPLEMENTATION_CRASHED,
                op,
                "the target process is not running",
            )
        return proc


def _deadline_for(body: dict[str, Any]) -> float:
    """Section 4.0.7: no lower than 100x the request's max_latency_ms, floored at 30 s.

    Only the retrieval request carries a budget. Ingest and answer take the floor, which is
    the conservative direction: a deadline that is too generous costs wall-clock on a hung
    run, while one that is too tight would turn a slow implementation into a protocol error
    and destroy the very result the eval exists to produce.
    """
    budget = body.get("budget")
    if isinstance(budget, dict):
        ms = budget.get("max_latency_ms")
        if isinstance(ms, (int, float)) and ms > 0:
            return max(DEADLINE_FLOOR_S, (ms / 1000.0) * DEADLINE_MULTIPLE)
    return DEADLINE_FLOOR_S


def _rmtree_quiet(path: Path) -> None:
    import shutil

    try:
        shutil.rmtree(path, ignore_errors=True)
    except OSError:  # pragma: no cover - best effort
        pass


__all__ = [
    "DEADLINE_FLOOR_S",
    "DEADLINE_MULTIPLE",
    "MAX_FRAME_BYTES",
    "SubprocessTarget",
    "TransportError",
    "minimal_env",
]
