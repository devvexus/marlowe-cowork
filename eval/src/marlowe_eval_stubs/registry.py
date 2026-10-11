"""Resolve a `--target` spec to a MemorySystem.

Two schemes:

  stub://   the reference oracle and the conformance fixtures, in process
  exec://   a real implementation spawned over the NDJSON transport pinned in
            CONTRACTS.md section 4.0 (pinned 2026-08-02); see
            docs/design/pinned-4.0-transport-record.md

`exec://` takes a command line, not a URL, because a URL cannot carry one without mangling
Windows paths. It **must** contain the literal token `{profile_root}`, which is replaced with
a freshly created empty directory on every spawn:

    --target "exec://./target/release/marlowe --eval-adapter --profile-root {profile_root}"

The placeholder is required rather than optional, and that is load-bearing. The harness
spawns a target once per corpus and four more times in the clock probe alone, and each spawn
must start from empty state -- the probe compares a run at epoch against the same run shifted
ten years, so state surviving between them would silently corrupt the comparison. A missing
placeholder is therefore an error at target-construction time, not a default that quietly
shares one directory.
"""

from __future__ import annotations

import shlex
import tempfile
from pathlib import Path
from urllib.parse import parse_qs, urlparse

from marlowe_eval.adapter.base import MemorySystem
from marlowe_eval.adapter.subprocess_ndjson import SubprocessTarget
from marlowe_eval.datasets.model import Corpus

from .broken import MUTATIONS, MutatingStub
from .oracle import OracleStub

EXEC_PREFIX = "exec://"
PROFILE_ROOT_TOKEN = "{profile_root}"

_FLOAT_KNOBS = {"precision", "abstention_rate", "answer_accuracy", "gate_threshold"}
_INT_KNOBS = {"seed", "maturation_ms", "k"}


def _knobs(query: str) -> dict[str, float | int]:
    out: dict[str, float | int] = {}
    for key, values in parse_qs(query).items():
        value = values[-1]
        if key in _FLOAT_KNOBS:
            out[key] = float(value)
        elif key in _INT_KNOBS:
            out[key] = int(value)
        else:
            raise ValueError(f"unknown stub knob: {key!r}")
    return out


def _build_exec_target(spec: str) -> MemorySystem:
    """Spawn a real implementation over the section 4.0 transport.

    `corpus` is deliberately not passed on. A real implementation receives its data through
    section 4.6 ingest and by no other route; handing it the corpus would be a second,
    unmeasured channel into the system under test, which is exactly what the M0a/M0b split
    exists to prevent.
    """
    command = spec[len(EXEC_PREFIX) :].strip()
    if not command:
        raise ValueError("exec:// target has no command")

    # posix=False keeps Windows backslashes intact; the quote stripping afterwards is what
    # posix=True would have done for the quoting, without eating the separators.
    argv = [tok.strip('"') for tok in shlex.split(command, posix=False)]

    if PROFILE_ROOT_TOKEN not in argv:
        raise ValueError(
            f"exec:// command must contain the literal token {PROFILE_ROOT_TOKEN} so every "
            "spawn gets a fresh, empty profile root. Without it, spawns share state and the "
            "clock probe silently compares contaminated runs. Example:\n"
            '  exec://./target/release/marlowe --eval-adapter --profile-root {profile_root}'
        )

    root = Path(tempfile.mkdtemp(prefix="marlowe-eval-"))
    argv = [str(root) if tok == PROFILE_ROOT_TOKEN else tok for tok in argv]
    return SubprocessTarget(argv=argv, name=spec, cleanup_dirs=(root,))


def build_target(spec: str, corpus: Corpus, **overrides: float | int) -> MemorySystem:
    if spec.startswith(EXEC_PREFIX):
        if overrides:
            raise ValueError(
                f"stub knobs {sorted(overrides)} are meaningless against a real "
                "implementation; exec:// takes none"
            )
        return _build_exec_target(spec)

    parsed = urlparse(spec)
    if parsed.scheme != "stub":
        raise ValueError(
            f"unsupported target scheme {parsed.scheme!r}. Known schemes: stub://, exec://."
        )

    kind = parsed.netloc or parsed.path.lstrip("/")
    knobs = {**_knobs(parsed.query), **overrides}

    if kind == "oracle":
        return OracleStub(corpus, **knobs)  # type: ignore[arg-type]

    if kind == "broken.clock_reader":
        # Not a response mutation: a stub that reads the system clock, which is exactly what
        # section 4.5 forbids and what the clock probe must catch.
        return OracleStub(corpus, read_system_clock=True, **knobs)  # type: ignore[arg-type]

    if kind.startswith("broken."):
        name = kind.split(".", 1)[1]
        if name not in MUTATIONS:
            raise ValueError(f"unknown conformance fixture: {name!r}")
        mutation, _ = MUTATIONS[name]
        return MutatingStub(OracleStub(corpus, **knobs), mutation, kind)  # type: ignore[arg-type]

    raise ValueError(f"unknown stub target: {kind!r}")


def describe_targets() -> dict[str, str]:
    out = {
        "stub://oracle": "reference implementation; reads the answer key, does not search",
        "stub://broken.clock_reader": "reads a system clock; must fail the clock probe",
        "exec://<cmd> --profile-root {profile_root}": (
            "spawn a real implementation over the section 4.0 NDJSON transport"
        ),
    }
    for name, (_, what) in MUTATIONS.items():
        out[f"stub://broken.{name}"] = what
    return out
