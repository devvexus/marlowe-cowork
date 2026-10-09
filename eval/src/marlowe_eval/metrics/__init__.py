"""Metrics. Every rate that leaves this package carries its cost or its caveat.

Three structural rules live here rather than in prose:

  * `Scored` cannot be built without a `CostSummary` -- section 5.7's "report the pair".
  * `PoisoningResult` cannot be built without utility retention -- section 8.3's "a defense
    that blocks everything by breaking the agent is not a defense".
  * There is no field called `injection_precision`. There are three differently-named
    precisions, so the headline cannot be filled in with a proxy by accident.
"""

from .accuracy import (
    AnswerGrader,
    ContainmentGrader,
    abstention_accuracy,
    accuracy,
    grade_records,
)
from .cost import CostAccumulator, CostSummary, Scored, percentile
from .precision import (
    EvidencePrecision,
    HumanJudgedPrecision,
    decile_of,
    evidence_precision,
    summarize_trust,
)
from .records import (
    DISTRACTOR,
    GOLD,
    UNATTRIBUTABLE,
    Attributor,
    InjectedRecord,
    QueryRecord,
)
from .security import PoisoningResult, TrustAssertion
from .staleness import StalenessCurve, StalenessPoint, fit

__all__ = [
    "DISTRACTOR",
    "GOLD",
    "UNATTRIBUTABLE",
    "AnswerGrader",
    "Attributor",
    "ContainmentGrader",
    "CostAccumulator",
    "CostSummary",
    "EvidencePrecision",
    "HumanJudgedPrecision",
    "InjectedRecord",
    "PoisoningResult",
    "QueryRecord",
    "Scored",
    "StalenessCurve",
    "StalenessPoint",
    "TrustAssertion",
    "abstention_accuracy",
    "accuracy",
    "decile_of",
    "evidence_precision",
    "fit",
    "grade_records",
    "percentile",
    "summarize_trust",
]
