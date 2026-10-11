"""CONTRACTS.md section 4.6 -- the ingest interface.

Loads a benchmark history; also how the poisoning suite plants its attacks.

The rule that shapes this whole module: **M0a declares `origin`. It never declares
`trust_class`.** Letting the eval set trust directly would let it bypass the mechanism it
exists to test. Structurally, `Origin` and `Turn` have no trust field at all and forbid
extras, so a harness bug that tried to send one cannot construct a request.
"""

from __future__ import annotations

from pydantic import Field

from .common import Channel, Clock, ContractModel, Speaker, TrustClass
from .version import CONTRACT_VERSION


class Origin(ContractModel):
    """Where the bytes came from. The implementation derives trust from this."""

    channel: Channel
    actor: str
    ref: str | None = None


class Turn(ContractModel):
    turn_id: str
    speaker: Speaker
    text: str
    occurred_at_ms: int
    origin: Origin


class IngestRequest(ContractModel):
    contract_version: str = CONTRACT_VERSION
    clock: Clock
    session_id: str
    turns: list[Turn]


class Written(ContractModel):
    """`effective_trust` here is what the harness DERIVED. The laundering suite compares
    against this value and never against anything the eval supplied."""

    turn_id: str
    memory_ids: list[str]
    effective_trust: TrustClass


class RejectedWrite(ContractModel):
    """A first-class outcome. A suite that plants a malformed or unauthorized write must be
    able to see it refused rather than infer refusal from absence.

    `reason` is a free string: section 4.6 shows "unknown_parent" but pins no vocabulary,
    and the CONTRACTS section 3.5 `Rejected` enum is outside the section 4 boundary M0a is
    permitted to depend on. The suites assert that a rejection occurred, not how it was
    spelled.
    """

    turn_id: str
    reason: str


class IngestLatency(ContractModel):
    total: int


class IngestCost(ContractModel):
    ingest_tokens: int
    latency_ms: IngestLatency


class IngestResponse(ContractModel):
    contract_version: str = CONTRACT_VERSION
    session_id: str
    written: list[Written] = Field(default_factory=list)
    rejected: list[RejectedWrite] = Field(default_factory=list)
    cost: IngestCost
