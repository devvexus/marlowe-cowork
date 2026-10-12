"""Entry point of the coding eval: ``python eval/coding/run.py <command>`` (or ``marlowe eval <command>``).

Run with ``python -I`` (``marlowe eval`` does): isolated mode ignores PYTHON* variables, so the code
that runs is this folder's and nothing else.
"""

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from mcoding.cli import main  # noqa: E402

if __name__ == "__main__":
    raise SystemExit(main())
