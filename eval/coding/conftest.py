"""Makes ``mcoding`` importable for ``pytest eval/coding`` without an install."""

import sys
from pathlib import Path

HERE = str(Path(__file__).parent)
if HERE not in sys.path:
    sys.path.insert(0, HERE)

# The task folders hold fixtures (test files the agents must make pass), not tests of the harness.
collect_ignore_glob = ["micro/*", "own/*", "iact/*", "results/*"]
