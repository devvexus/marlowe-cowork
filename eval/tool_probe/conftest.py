"""Makes `grade` and `probe` importable when pytest runs from the repository root."""

import sys
from pathlib import Path

HERE = str(Path(__file__).parent)
if HERE not in sys.path:
    sys.path.insert(0, HERE)
