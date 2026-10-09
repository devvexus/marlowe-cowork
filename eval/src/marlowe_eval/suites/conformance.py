"""Contract conformance -- does the implementation obey section 4 at all?

Each interface is probed with its own fresh client, and a failure on one does not stop the
others. That is not tidiness: the acceptance criterion is *"rejects a response missing cost
-- on all three interfaces"*, and a runner that aborted on the first violation could only
ever demonstrate one of the three.

A conforming implementation produces zero findings. A conformance fixture produces exactly
the finding it was built to produce, which is how each check is shown to discriminate rather
than merely to pass.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any, Callable

from ..adapter.base import Client, ImplementationFailure, MemorySystem
from ..contract import (
    AnswerRequest,
    IngestRequest,
    Interface,
    ProtocolError,
    RetrievalRequest,
)
from ..datasets.model import Corpus
from . import clock

MakeSystem = Callable[[Corpus], MemorySystem]


@dataclass(frozen=True)
class Finding:
    interface: str
    kind: str
    detail: str
    location: str

    def as_dict(self) -> dict[str, str]:
        return {
            "interface": self.interface,
            "kind": self.kind,
            "detail": self.detail,
            "location": self.location,
        }


@dataclass
class ConformanceReport:
    target: str
    findings: list[Finding]
    clock_verdict: str
    interfaces_probed: list[str]

    @property
    def conforms(self) -> bool:
        return not self.findings and self.clock_verdict == clock.PASS

    def as_dict(self) -> dict[str, Any]:
        return {
            "target": self.target,
            "conforms": self.conforms,
            "interfaces_probed": self.interfaces_probed,
            "clock_probe": self.clock_verdict,
            "findings": [f.as_dict() for f in self.findings],
        }


def _probe(interface: Interface, fn: Callable[[], Any]) -> Finding | None:
    try:
        fn()
    except ProtocolError as exc:
        return Finding(
            interface=exc.interface.value,
            kind=exc.kind.value,
            detail=exc.detail,
            location=exc.location or "",
        )
    except ImplementationFailure as exc:
        # Class B in the proposed section 4.0.4 taxonomy: a result, not a contract
        # violation. Recorded so a target that simply fails is distinguishable from one
        # that misbehaves.
        return Finding(
            interface=interface.value,
            kind="implementation_failure",
            detail=str(exc),
            location="",
        )
    return None


def run(make: MakeSystem, corpus: Corpus, *, target: str = "") -> ConformanceReport:
    findings: list[Finding] = []
    session = corpus.sessions[0]
    case = corpus.cases[0]

    def ingest_once(client: Client) -> None:
        client.ingest(
            IngestRequest(
                clock={"now_ms": session.ingest_at_ms},
                session_id=session.session_id,
                turns=list(session.turns),
            )
        )

    # -- ingest --------------------------------------------------------------------
    ingest_client = Client(make(corpus))
    finding = _probe(Interface.INGEST, lambda: ingest_once(ingest_client))
    if finding:
        findings.append(finding)

    # -- retrieve ------------------------------------------------------------------
    # A fresh client, and the setup ingest is allowed to fail: a response missing `cost`
    # on retrieve is a retrieve finding whether or not ingest was well formed.
    retrieve_client = Client(make(corpus))
    try:
        ingest_once(retrieve_client)
    except (ProtocolError, ImplementationFailure):
        pass
    finding = _probe(
        Interface.RETRIEVE,
        lambda: retrieve_client.retrieve(
            RetrievalRequest(
                query_id=case.query_id,
                session_id=case.session_id,
                turn_index=0,
                query_text=case.question,
                clock={"now_ms": case.ask_at_ms},
                budget={"max_tokens": 7000, "max_latency_ms": 300},
            )
        ),
    )
    if finding:
        findings.append(finding)

    # -- answer --------------------------------------------------------------------
    answer_client = Client(make(corpus))
    try:
        ingest_once(answer_client)
    except (ProtocolError, ImplementationFailure):
        pass
    finding = _probe(
        Interface.ANSWER,
        lambda: answer_client.answer(
            AnswerRequest(
                clock={"now_ms": case.ask_at_ms},
                query_id=case.query_id,
                session_id=case.session_id,
                question=case.question,
            )
        ),
    )
    if finding:
        findings.append(finding)

    for client in (ingest_client, retrieve_client, answer_client):
        client.close()

    # The clock probe drives the same interfaces, so a target that violates section 4
    # breaks it too. That is not a crash and not a clock verdict -- it is the shape
    # findings above, already recorded. Report the probe as unrunnable and move on.
    try:
        clock_verdict = clock.run(make).verdict
    except ProtocolError as exc:
        clock_verdict = f"not_run ({exc.kind.value})"

    return ConformanceReport(
        target=target,
        findings=findings,
        clock_verdict=clock_verdict,
        interfaces_probed=["ingest", "retrieve", "answer"],
    )
