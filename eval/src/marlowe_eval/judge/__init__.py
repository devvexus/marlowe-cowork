"""The offline judge (HP1 tier 2), its cache, and agreement against the human set.

Two rules are enforced by types rather than by discipline:

  * `AnthropicJudge` cannot be constructed without an explicit two-part opt-in AND a
    loaded human label set.
  * `JudgedPrecision` cannot be constructed without an `Agreement`.

Neither is a policy check that runs at report time. Both are constructors that refuse.
"""

from .agreement import (
    Agreement,
    JudgedPrecision,
    NoOverlap,
    compute_agreement,
    judged_precision,
)
from .cache import VerdictCache, key_for
from .protocol import Judge, JudgeUnavailable, Verdict
from .scripted import ScriptedJudge

__all__ = [
    "Agreement",
    "Judge",
    "JudgeUnavailable",
    "JudgedPrecision",
    "NoOverlap",
    "ScriptedJudge",
    "Verdict",
    "VerdictCache",
    "compute_agreement",
    "judged_precision",
    "key_for",
]


def build_anthropic_judge(**kwargs: object):  # pragma: no cover - thin lazy wrapper
    """Import the API-backed judge only when someone actually asks for it.

    Keeps `import marlowe_eval.judge` working without the optional `anthropic` dependency,
    and keeps the gated path from being reachable by accident.
    """
    from .anthropic_judge import AnthropicJudge

    return AnthropicJudge(**kwargs)  # type: ignore[arg-type]
