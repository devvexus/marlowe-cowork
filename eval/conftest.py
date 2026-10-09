"""Make `src/` importable without an install.

`pip install -e .` is the supported path; this exists so that a third party who clones the
repo and types `pytest` gets a passing suite on the first try. "Reproducible by a third
party from the repo alone" is an acceptance criterion, and an install step they have to
discover from a traceback is a step they can get wrong.
"""

import sys
from pathlib import Path

SRC = Path(__file__).parent / "src"
if str(SRC) not in sys.path:
    sys.path.insert(0, str(SRC))
