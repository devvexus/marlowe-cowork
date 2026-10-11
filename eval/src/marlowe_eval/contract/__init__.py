"""CONTRACTS.md section 4, and nothing else.

This package is the harness's ONLY dependency on the Marlowe design. M0a is built against
the section 4 boundary without knowledge of the retriever; depending on anything else means
the M0a/M0b split is not real.

  section 4.1-4.4  retrieval   -> injection precision, tokens, latency
  section 4.5      clock       -> staleness half-life, reproducibility
  section 4.6      ingest      -> benchmark histories, the poisoning suite
  section 4.7      answer      -> accuracy and abstention

A test enforces that nothing in this package imports from elsewhere in the harness.
"""

from .answer import (
    AnswerCost,
    AnswerLatency,
    AnswerRequest,
    AnswerResponse,
)
from .common import (
    AbstentionReason,
    Channel,
    Clock,
    ContractModel,
    Fidelity,
    PayloadKind,
    Speaker,
    TrustClass,
)
from .errors import Interface, ProtocolError, ProtocolErrorKind
from .ingest import (
    IngestCost,
    IngestLatency,
    IngestRequest,
    IngestResponse,
    Origin,
    RejectedWrite,
    Turn,
    Written,
)
from .retrieval import (
    GateStamp,
    InjectedMemory,
    RetrievalBudget,
    RetrievalCost,
    RetrievalLatency,
    RetrievalRequest,
    RetrievalResponse,
)
from .validate import (
    check_correlation,
    parse_answer_response,
    parse_ingest_response,
    parse_retrieval_response,
    validate_outbound_ingest,
)
from .version import CONTRACT_VERSION

__all__ = [
    "CONTRACT_VERSION",
    "AbstentionReason",
    "AnswerCost",
    "AnswerLatency",
    "AnswerRequest",
    "AnswerResponse",
    "Channel",
    "Clock",
    "ContractModel",
    "Fidelity",
    "GateStamp",
    "IngestCost",
    "IngestLatency",
    "IngestRequest",
    "IngestResponse",
    "InjectedMemory",
    "Interface",
    "Origin",
    "PayloadKind",
    "ProtocolError",
    "ProtocolErrorKind",
    "RejectedWrite",
    "RetrievalBudget",
    "RetrievalCost",
    "RetrievalLatency",
    "RetrievalRequest",
    "RetrievalResponse",
    "Speaker",
    "TrustClass",
    "Turn",
    "Written",
    "check_correlation",
    "parse_answer_response",
    "parse_ingest_response",
    "parse_retrieval_response",
    "validate_outbound_ingest",
]
