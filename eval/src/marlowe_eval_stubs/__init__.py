"""Reference and conformance stubs for the M0a harness.

A separate top-level package from `marlowe_eval` on purpose. The measurement must not
contain the thing measured, and a sibling package makes that visible in the import graph
rather than in a comment.

Target specs, as accepted by the CLI's `--target`:

    stub://oracle                  the reference implementation
    stub://oracle?precision=0.7    with knobs
    stub://broken.missing_cost     a conformance fixture
    stub://broken.clock_reader     reads a system clock; must fail the clock probe
"""

from .broken import MUTATIONS, MutatingStub
from .oracle import OracleStub
from .registry import build_target, describe_targets

__all__ = [
    "MUTATIONS",
    "MutatingStub",
    "OracleStub",
    "build_target",
    "describe_targets",
]
