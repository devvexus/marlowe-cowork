"""The interface between the harness and a system under test.

Two levels, and the separation is deliberate:

  * `MemorySystem` speaks RAW DICTS, exactly as the wire will. It cannot return a typed
    model, because a typed model could not express a malformed response -- and the
    conformance fixtures exist precisely to emit malformed responses. If the ABC validated
    on the way out, the harness could never be shown to reject anything.

  * `Client` is the validating layer: it builds requests, polices its own outbound frames,
    parses responses, enforces section 4 semantics, checks correlation, and records the
    transcript.

The subprocess transport is deliberately absent. It is the one component that depends on
section 4.0, which is drafted in docs/design/proposed-4.0-transport.md and not yet pinned.
Everything else builds against this ABC in the meantime, and the reference stub satisfies it
in-process.
"""

from __future__ import annotations

import abc
from dataclasses import dataclass, field
from typing import Any

from ..contract import (
    AnswerRequest,
    AnswerResponse,
    IngestRequest,
    IngestResponse,
    Interface,
    ProtocolError,
    RetrievalRequest,
    RetrievalResponse,
    check_correlation,
    parse_answer_response,
    parse_ingest_response,
    parse_retrieval_response,
    validate_outbound_ingest,
)


class ImplementationFailure(Exception):
    """The system under test failed on a well-formed request.

    Class B in the proposed section 4.0.4 taxonomy: a RESULT, not a protocol error. The unit
    is scored as failed and the run continues.
    """


class MemorySystem(abc.ABC):
    """What M0b, and any third-party system, presents to the harness.

    Three operations. There is no fourth: if the harness ever needs one, the split is
    leaking and the fix belongs in CONTRACTS.md, not here.
    """

    name: str = "unnamed"

    @abc.abstractmethod
    def call(self, op: Interface, body: dict[str, Any]) -> dict[str, Any]:
        """Take a section 4 request body, return a section 4 response body.

        Raise `ImplementationFailure` to report a Class B failure.
        """

    def close(self) -> None:  # pragma: no cover - trivial default
        return None


@dataclass
class Exchange:
    """One request/response pair, as it would appear on the wire."""

    op: str
    request: dict[str, Any]
    response: dict[str, Any] | None
    error: dict[str, str] | None = None

    def as_frames(self) -> list[dict[str, Any]]:
        """The transcript is written in proposed section 4.0.3 frame shape.

        run.jsonl is therefore the wire log even when the run went through an in-process
        stub, which is what lets a third party diff a stub run against a real one.
        """
        frames: list[dict[str, Any]] = [{"op": self.op, "body": self.request}]
        if self.error is not None:
            frames.append({"op": self.op, "error": self.error})
        else:
            frames.append({"op": self.op, "body": self.response})
        return frames


@dataclass
class Client:
    """Validating wrapper around a `MemorySystem`."""

    system: MemorySystem
    transcript: list[Exchange] = field(default_factory=list)
    failures: list[dict[str, str]] = field(default_factory=list)

    # -- the three interfaces ------------------------------------------------------

    def ingest(self, req: IngestRequest) -> IngestResponse:
        body = req.model_dump(mode="json")
        # Section 4.6: the harness declares origin and never trust. Checked on our own
        # frame, before it leaves, because an eval that can set trust directly bypasses the
        # mechanism the laundering suite exists to test.
        validate_outbound_ingest(body)
        raw = self._call(Interface.INGEST, body)
        resp = parse_ingest_response(raw)
        check_correlation(Interface.INGEST, req, resp)
        return resp

    def retrieve(self, req: RetrievalRequest) -> RetrievalResponse:
        body = req.model_dump(mode="json")
        raw = self._call(Interface.RETRIEVE, body)
        resp = parse_retrieval_response(raw)
        check_correlation(Interface.RETRIEVE, req, resp)
        return resp

    def answer(self, req: AnswerRequest) -> AnswerResponse:
        body = req.model_dump(mode="json")
        raw = self._call(Interface.ANSWER, body)
        resp = parse_answer_response(raw)
        check_correlation(Interface.ANSWER, req, resp)
        return resp

    # -- plumbing ------------------------------------------------------------------

    def _call(self, op: Interface, body: dict[str, Any]) -> dict[str, Any]:
        exchange = Exchange(op=op.value, request=body, response=None)
        self.transcript.append(exchange)
        try:
            raw = self.system.call(op, body)
        except ImplementationFailure as exc:
            exchange.error = {"kind": "internal_error", "detail": str(exc)}
            self.failures.append({"op": op.value, "detail": str(exc)})
            raise
        except ProtocolError:
            raise
        exchange.response = raw
        return raw

    def close(self) -> None:
        self.system.close()
