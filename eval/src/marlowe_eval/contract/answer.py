"""CONTRACTS.md section 4.7 -- the answer interface.

LongMemEval and LoCoMo score answers, not retrieval. Abstention is scored explicitly.
"""

from __future__ import annotations

from pydantic import Field

from .common import AbstentionReason, Clock, ContractModel
from .retrieval import RetrievalResponse
from .version import CONTRACT_VERSION


class AnswerRequest(ContractModel):
    contract_version: str = CONTRACT_VERSION
    clock: Clock
    query_id: str
    session_id: str
    question: str


class AnswerLatency(ContractModel):
    total: int
    retrieval: int | None = None
    generation: int | None = None


class AnswerCost(ContractModel):
    """Section 4.7 separates retrieval from generation tokens on purpose: section 5.7's
    <=7,000 budget is a RETRIEVAL budget, and folding generation into it would hide a miss."""

    prompt_tokens: int
    completion_tokens: int
    retrieval_tokens: int
    latency_ms: AnswerLatency


class AnswerResponse(ContractModel):
    """Section 4.7.

    `retrieval` is embedded, not referenced. Injection precision and answer correctness must
    be joinable on a single record or the headline metric cannot be attributed to a
    retrieval decision -- which is the entire reason both are measured.
    """

    contract_version: str = CONTRACT_VERSION
    query_id: str
    answered: bool
    answer: str | None = None
    abstained: bool
    abstention_reason: AbstentionReason | None = None
    grounded_in: list[str] = Field(default_factory=list)
    retrieval: RetrievalResponse
    cost: AnswerCost
