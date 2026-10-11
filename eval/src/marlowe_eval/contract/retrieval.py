"""CONTRACTS.md sections 4.1-4.4 -- the retrieval interface."""

from __future__ import annotations

from pydantic import Field

from .common import AbstentionReason, Clock, ContractModel, Fidelity, PayloadKind, TrustClass
from .version import CONTRACT_VERSION


class RetrievalBudget(ContractModel):
    max_tokens: int
    max_latency_ms: int


class RetrievalRequest(ContractModel):
    """Section 4.1.

    `clock` has the same shape here as on ingest and answer. An earlier draft put a bare
    `now_ms` at the top level of this request only; that was pinned away on 2026-08-02
    because section 4.5 already required a clock on all three interfaces and the
    inconsistency was the first thing a third-party implementer would hit.
    """

    contract_version: str = CONTRACT_VERSION
    clock: Clock
    query_id: str
    session_id: str
    turn_index: int
    query_text: str
    budget: RetrievalBudget


class InjectedMemory(ContractModel):
    """Section 4.2b.

    `content` is non-optional by contract: a tombstone can never appear here (section 4.3),
    so there is no case where the text is legitimately absent.
    """

    memory_id: str
    content: str
    score: float
    calibrated_precision: float
    fidelity: Fidelity
    effective_trust: TrustClass
    payload_kind: PayloadKind


class GateStamp(ContractModel):
    """Section 4.4. `adaptive` is false for M0; only M10 may set it true."""

    version: str
    threshold: float
    adaptive: bool


class RetrievalLatency(ContractModel):
    """Section 4.2 shows total/embed/cues/fuse/gate.

    Only `total` is required. The stage breakdown is optional because an implementation
    without an embed stage should report its absence rather than report a zero that reads
    as a measurement.
    """

    total: int
    embed: int | None = None
    cues: int | None = None
    fuse: int | None = None
    gate: int | None = None


class RetrievalCost(ContractModel):
    retrieval_tokens: int
    latency_ms: RetrievalLatency


class RetrievalResponse(ContractModel):
    """Section 4.2.

    `cost` is required. Section 4.2b: "NOT Option. A result without cost is
    unrepresentable." That is the whole mechanism by which section 5.7's "report the pair"
    stops being a convention someone has to remember.
    """

    contract_version: str = CONTRACT_VERSION
    query_id: str
    abstained: bool
    abstention_reason: AbstentionReason | None = None
    injected: list[InjectedMemory] = Field(default_factory=list)
    considered: int
    gate: GateStamp
    cost: RetrievalCost
