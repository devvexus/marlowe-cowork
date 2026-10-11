"""Section 4.5 and the reproduction criterion.

Two claims under test:

  * The harness reads no system clock. It cannot credibly verify that an implementation
    obeys section 4.5 while quietly breaking the same rule itself.
  * A run at a fixed seed and clock reproduces bit-identically over the deterministic core,
    with timing excluded by a declared allowlist and no tolerance windows.
"""

from __future__ import annotations

import ast
from pathlib import Path

import marlowe_eval
from marlowe_eval import canonical
from marlowe_eval.datasets import fixture_longmemeval
from marlowe_eval.runner import RunConfig, run
from marlowe_eval.suites import clock
from marlowe_eval_stubs import build_target

PACKAGE = Path(marlowe_eval.__file__).parent

BANNED_CALLS = {
    ("time", "time"), ("time", "monotonic"), ("time", "perf_counter"),
    ("time", "time_ns"), ("time", "monotonic_ns"),
    ("datetime", "now"), ("datetime", "today"), ("datetime", "utcnow"),
    ("date", "today"),
}


def test_no_source_file_carries_a_byte_order_mark():
    """§4.0.2 pins UTF-8 without a BOM on the wire; the source tree follows the same rule.

    Earned rather than theoretical: a Windows tool wrote BOMs into nine files during the
    §4.0 pin, and every AST-scanning test in this suite failed with a `SyntaxError` about
    an invalid non-printable character. They caught it, but the message pointed at Python's
    parser rather than at the encoding. This one says what actually happened.
    """
    offenders = [
        path.name
        for path in Path(marlowe_eval.__file__).parents[1].rglob("*.py")
        if path.read_bytes().startswith(b"\xef\xbb\xbf")
    ]
    assert not offenders, f"UTF-8 BOM in: {offenders}"


def test_harness_reads_no_system_clock():
    offenders = []
    for path in PACKAGE.rglob("*.py"):
        tree = ast.parse(path.read_text(encoding="utf-8"))
        for node in ast.walk(tree):
            if isinstance(node, ast.Call) and isinstance(node.func, ast.Attribute):
                owner = node.func.value
                if isinstance(owner, ast.Name) and (owner.id, node.func.attr) in BANNED_CALLS:
                    offenders.append(f"{path.name}:{node.lineno} {owner.id}.{node.func.attr}()")
    assert not offenders, (
        "the harness itself must derive every timestamp from the synthetic clock: "
        f"{offenders}"
    )


def _make(spec):
    return lambda corpus: build_target(spec, corpus)


def _run(spec="stub://oracle"):
    return run(
        _make(spec),
        {"longmemeval-s": fixture_longmemeval()},
        RunConfig(target=spec, seed=7, clock_ms=1_780_000_000_000),
    )


def test_two_runs_at_the_same_seed_and_clock_are_bit_identical():
    assert _run().core_hash == _run().core_hash


def test_timing_is_excluded_by_a_declared_allowlist_only():
    assert canonical.TIMING_ALLOWLIST == frozenset({"latency_ms"})
    stripped = canonical.strip_timing(
        {"cost": {"retrieval_tokens": 812, "latency_ms": {"total": 118}}}
    )
    assert stripped == {"cost": {"retrieval_tokens": 812}}


def test_a_field_outside_the_allowlist_changes_the_hash():
    """No tolerance windows: any off-allowlist difference is a failure, not a wobble."""
    base = {"value": 0.9500000, "cost": {"latency_ms": {"total": 1}}}
    drifted = {"value": 0.9500001, "cost": {"latency_ms": {"total": 999}}}
    assert canonical.core_hash(base) != canonical.core_hash(drifted)


# -- the clock probe must discriminate, or it is measuring nothing -----------------------


def test_clock_probe_passes_a_conforming_implementation():
    assert clock.run(_make("stub://oracle")).verdict == clock.PASS


def test_clock_probe_fails_a_system_clock_reader():
    """If this ever passes, the probe has stopped having teeth and every decay-dependent
    number in the report is unverified."""
    assert clock.run(_make("stub://broken.clock_reader")).verdict != clock.PASS
