"""Parsing and section 4 semantic enforcement.

Two layers, kept separate because they fail for different reasons:

  * pydantic handles SHAPE  -- required fields, types, closed enums, no extras
  * `semantic_checks` handles CONTRACT INVARIANTS -- the rules section 4 states in prose

Both raise ProtocolError. Neither warns, and neither returns a score.
"""

from __future__ import annotations

from typing import Any, TypeVar

from pydantic import BaseModel, ValidationError

from .answer import AnswerRequest, AnswerResponse
from .common import Fidelity
from .errors import Interface, ProtocolError, ProtocolErrorKind
from .ingest import IngestRequest, IngestResponse
from .retrieval import RetrievalRequest, RetrievalResponse
from .version import CONTRACT_VERSION

M = TypeVar("M", bound=BaseModel)


# --------------------------------------------------------------------------------------
# shape
# --------------------------------------------------------------------------------------


def _map_validation_error(exc: ValidationError, interface: Interface) -> ProtocolError:
    """Turn a pydantic failure into the most specific contract violation it represents.

    Specificity is the point. The acceptance criterion is "rejects a response missing
    `cost`", so the harness must be able to say `missing_cost` -- a generic validation
    failure would satisfy the letter and lose the diagnosis.
    """
    err = exc.errors()[0]
    loc = tuple(str(p) for p in err["loc"])
    etype = err["type"]
    where = ".".join(loc) or "<root>"

    # A missing cost inside an embedded retrieval result is a RETRIEVE violation even
    # though it arrived on the answer interface.
    iface = Interface.RETRIEVE if loc[:1] == ("retrieval",) else interface

    if etype == "missing":
        if loc[-1] == "cost":
            return ProtocolError(
                ProtocolErrorKind.MISSING_COST,
                iface,
                "response carries no cost block; section 4.2b makes a result without cost "
                "unrepresentable",
                location=where,
            )
        if loc[-1] == "clock":
            return ProtocolError(
                ProtocolErrorKind.MISSING_CLOCK,
                iface,
                "section 4.5 requires clock on all three interfaces",
                location=where,
            )
        return ProtocolError(
            ProtocolErrorKind.MISSING_FIELD, iface, f"required field absent: {where}",
            location=where,
        )

    if etype == "extra_forbidden":
        return ProtocolError(
            ProtocolErrorKind.UNKNOWN_FIELD,
            iface,
            f"undeclared field: {where}",
            location=where,
        )

    if etype == "enum":
        return ProtocolError(
            ProtocolErrorKind.BAD_ENUM, iface, err.get("msg", "value outside closed set"),
            location=where,
        )

    return ProtocolError(
        ProtocolErrorKind.BAD_TYPE, iface, err.get("msg", etype), location=where
    )


def _parse(model_cls: type[M], raw: Any, interface: Interface) -> M:
    if not isinstance(raw, dict):
        raise ProtocolError(
            ProtocolErrorKind.MALFORMED_JSON,
            interface,
            f"expected a JSON object, got {type(raw).__name__}",
        )
    try:
        return model_cls.model_validate(raw)
    except ValidationError as exc:
        raise _map_validation_error(exc, interface) from None


# --------------------------------------------------------------------------------------
# semantics
# --------------------------------------------------------------------------------------


def _check_version(version: str, interface: Interface) -> None:
    if version != CONTRACT_VERSION:
        raise ProtocolError(
            ProtocolErrorKind.CONTRACT_VERSION_MISMATCH,
            interface,
            f"expected {CONTRACT_VERSION!r}, got {version!r}",
            location="contract_version",
        )


def _check_retrieval(r: RetrievalResponse, interface: Interface = Interface.RETRIEVE) -> None:
    _check_version(r.contract_version, interface)

    # Section 4.3: tombstones reach explicit recall and the abstention check, never
    # injection. Scoring one as a candidate would penalise the headline metric for working.
    for item in r.injected:
        if item.fidelity is Fidelity.TOMBSTONE:
            raise ProtocolError(
                ProtocolErrorKind.TOMBSTONE_INJECTED,
                interface,
                f"memory {item.memory_id} injected at fidelity tombstone",
                location="injected",
            )

    # Section 4.2: abstention and injection are mutually exclusive, in both directions.
    # Pinned 2026-08-02. The mapping is total -- every response either injects something or
    # abstains, and never both or neither -- so that one event cannot be scored two ways.
    if r.abstained and r.injected:
        raise ProtocolError(
            ProtocolErrorKind.ABSTAINED_WITH_INJECTED,
            interface,
            f"abstained with {len(r.injected)} injected memories; an implementation with "
            "something to inject has not abstained",
            location="injected",
        )
    if not r.abstained and r.abstention_reason is not None:
        raise ProtocolError(
            ProtocolErrorKind.REASON_WITHOUT_ABSTENTION,
            interface,
            f"abstention_reason {r.abstention_reason.value!r} set while abstained is false",
            location="abstention_reason",
        )
    if not r.abstained and not r.injected:
        raise ProtocolError(
            ProtocolErrorKind.EMPTY_INJECTION_WITHOUT_ABSTENTION,
            interface,
            "injected nothing without abstaining; injecting nothing IS the abstention "
            "outcome (no_candidates), and reporting it as a non-abstention makes the same "
            "event scoreable two ways",
            location="injected",
        )


def _check_answer(a: AnswerResponse) -> None:
    _check_version(a.contract_version, Interface.ANSWER)

    # Section 4.7, pinned verbatim: "answered: false with a populated answer is a protocol
    # error." A schema that lets the honest "no" and a hedged answer blur is a schema that
    # will let a confabulation score as an abstention.
    if not a.answered and a.answer is not None and a.answer.strip():
        raise ProtocolError(
            ProtocolErrorKind.HEDGED_ABSTENTION,
            Interface.ANSWER,
            f"answered is false but answer carries {len(a.answer.strip())} characters",
            location="answer",
        )

    if not a.abstained and a.abstention_reason is not None:
        raise ProtocolError(
            ProtocolErrorKind.REASON_WITHOUT_ABSTENTION,
            Interface.ANSWER,
            f"abstention_reason {a.abstention_reason.value!r} set while abstained is false",
            location="abstention_reason",
        )

    # Section 4.7, by symmetry with 4.2. Pinned 2026-08-02.
    if a.abstained and a.grounded_in:
        raise ProtocolError(
            ProtocolErrorKind.GROUNDED_WHILE_ABSTAINED,
            Interface.ANSWER,
            f"abstained while citing {len(a.grounded_in)} grounding memories; a refusal "
            "that claims to have answered from evidence is two outcomes at once",
            location="grounded_in",
        )
    if not a.answered and not a.abstained:
        raise ProtocolError(
            ProtocolErrorKind.UNANSWERED_WITHOUT_ABSTENTION,
            Interface.ANSWER,
            "answered is false without abstained being true; the scorer has no bin for "
            "that outcome, so it would count as neither a correct refusal nor an "
            "incorrect answer -- which is how a systematic failure leaves a report",
            location="answered",
        )

    _check_retrieval(a.retrieval, Interface.RETRIEVE)

    if a.retrieval.query_id != a.query_id:
        raise ProtocolError(
            ProtocolErrorKind.CORRELATION_MISMATCH,
            Interface.ANSWER,
            f"embedded retrieval is for {a.retrieval.query_id!r}, answer is for "
            f"{a.query_id!r}; the two must be joinable on one record",
            location="retrieval.query_id",
        )


# --------------------------------------------------------------------------------------
# public entry points
# --------------------------------------------------------------------------------------


def parse_retrieval_response(raw: Any) -> RetrievalResponse:
    r = _parse(RetrievalResponse, raw, Interface.RETRIEVE)
    _check_retrieval(r)
    return r


def parse_ingest_response(raw: Any) -> IngestResponse:
    r = _parse(IngestResponse, raw, Interface.INGEST)
    _check_version(r.contract_version, Interface.INGEST)
    return r


def parse_answer_response(raw: Any) -> AnswerResponse:
    a = _parse(AnswerResponse, raw, Interface.ANSWER)
    _check_answer(a)
    return a


def check_correlation(
    interface: Interface, sent: BaseModel, received: BaseModel
) -> None:
    """Positional correlation, additionally checked -- see proposed section 4.0.5.

    A desynchronized stream must fail loudly rather than silently misattribute a result to
    the wrong query.
    """
    key = "session_id" if interface is Interface.INGEST else "query_id"
    want = getattr(sent, key)
    got = getattr(received, key)
    if want != got:
        raise ProtocolError(
            ProtocolErrorKind.CORRELATION_MISMATCH,
            interface,
            f"sent {key}={want!r}, received {key}={got!r}",
            location=key,
        )


# --------------------------------------------------------------------------------------
# outbound: the harness policing itself
# --------------------------------------------------------------------------------------


def _find_key(node: Any, key: str, path: str = "") -> str | None:
    if isinstance(node, dict):
        for k, v in node.items():
            here = f"{path}.{k}" if path else k
            if k == key:
                return here
            found = _find_key(v, key, here)
            if found:
                return found
    elif isinstance(node, list):
        for i, v in enumerate(node):
            found = _find_key(v, key, f"{path}[{i}]")
            if found:
                return found
    return None


def validate_outbound_ingest(payload: dict[str, Any]) -> None:
    """Section 4.6: M0a declares origin and NEVER trust.

    The models make this structurally impossible, so this is a second lock on the same
    door. It is here because the failure it guards against is not a crash -- an eval that
    could set trust directly would quietly bypass the mechanism it exists to test, and
    every laundering result after that point would be meaningless while still looking fine.
    """
    hit = _find_key(payload, "trust_class")
    if hit is not None:
        raise ProtocolError(
            ProtocolErrorKind.HARNESS_DECLARED_TRUST,
            Interface.INGEST,
            "the harness attempted to declare trust_class on an ingest request",
            location=hit,
        )
    hit = _find_key(payload, "effective_trust")
    if hit is not None:
        raise ProtocolError(
            ProtocolErrorKind.HARNESS_DECLARED_TRUST,
            Interface.INGEST,
            "the harness attempted to declare effective_trust on an ingest request",
            location=hit,
        )


__all__ = [
    "AnswerRequest",
    "AnswerResponse",
    "IngestRequest",
    "IngestResponse",
    "RetrievalRequest",
    "RetrievalResponse",
    "check_correlation",
    "parse_answer_response",
    "parse_ingest_response",
    "parse_retrieval_response",
    "validate_outbound_ingest",
]
