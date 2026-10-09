"""Deliberately non-conforming stubs -- one per acceptance criterion.

Every M0a acceptance bullet of the form "the harness rejects X" needs an X to reject. A
harness that has never been shown a bad response is a harness whose rejection path is
untested, and the rejection path is most of what M0a is.

These are also how each probe is shown to have teeth. A clock probe that no implementation
can fail is not a probe; `clock_reader` is what proves it discriminates.
"""

from __future__ import annotations

import copy
from typing import Any, Callable

from marlowe_eval.adapter.base import MemorySystem
from marlowe_eval.contract import Interface

Mutation = Callable[[Interface, dict[str, Any]], dict[str, Any]]


class MutatingStub(MemorySystem):
    """Wraps a conforming stub and corrupts its responses in exactly one way."""

    def __init__(self, inner: MemorySystem, mutate: Mutation, name: str) -> None:
        self.inner = inner
        self.mutate = mutate
        self.name = name

    def call(self, op: Interface, body: dict[str, Any]) -> dict[str, Any]:
        return self.mutate(op, copy.deepcopy(self.inner.call(op, body)))

    def close(self) -> None:
        self.inner.close()


# -- section 4.2b / 4.6 / 4.7: cost is not optional -------------------------------------


def drop_cost(_op: Interface, body: dict[str, Any]) -> dict[str, Any]:
    body.pop("cost", None)
    return body


def drop_embedded_retrieval_cost(op: Interface, body: dict[str, Any]) -> dict[str, Any]:
    """Missing cost on the retrieval result embedded in an answer.

    Separate from `drop_cost` because it is the case most likely to slip through: the answer
    response is well-formed, and the omission is one level down in the record that makes
    injection precision joinable to answer correctness.
    """
    if op is Interface.ANSWER:
        body.get("retrieval", {}).pop("cost", None)
    return body


# -- section 4.7: the honest "no" is not a hedged answer --------------------------------


def hedge_abstention(op: Interface, body: dict[str, Any]) -> dict[str, Any]:
    if op is Interface.ANSWER:
        body["answered"] = False
        body["answer"] = "I'm not certain, but it may have been Postgres."
    return body


# -- section 4.3: a tombstone can never compete for injection precision ------------------


def inject_tombstone(op: Interface, body: dict[str, Any]) -> dict[str, Any]:
    target = body.get("retrieval") if op is Interface.ANSWER else body
    if target and target.get("injected"):
        target["injected"][0]["fidelity"] = "tombstone"
    return body


# -- proposed clarification to section 4.2 (Part C) --------------------------------------


def abstain_while_injecting(op: Interface, body: dict[str, Any]) -> dict[str, Any]:
    target = body.get("retrieval") if op is Interface.ANSWER else body
    if target and target.get("injected"):
        target["abstained"] = True
        target["abstention_reason"] = "no_candidate_above_threshold"
    return body


def inject_nothing_without_abstaining(op: Interface, body: dict[str, Any]) -> dict[str, Any]:
    """The other direction of the same exclusivity, pinned 2026-08-02.

    Injecting nothing IS the abstention outcome; reporting it as a non-abstention makes one
    event scoreable two ways.
    """
    # Scoped to the two interfaces that carry an injected set. The other mutations in this
    # module dodge this only by accident -- they test `target.get("injected")` first, which
    # is absent on an ingest response -- whereas this one sets keys unconditionally and
    # would otherwise corrupt ingest into an unrelated `unknown_field` violation.
    if op is Interface.INGEST:
        return body
    target = body.get("retrieval") if op is Interface.ANSWER else body
    if isinstance(target, dict):
        target["injected"] = []
        target["abstained"] = False
        target["abstention_reason"] = None
    return body


# -- section 4.7 symmetry, pinned 2026-08-02 --------------------------------------------


def ground_while_abstaining(op: Interface, body: dict[str, Any]) -> dict[str, Any]:
    """A refusal that cites evidence claims two outcomes at once."""
    if op is Interface.ANSWER:
        body["answered"] = False
        body["answer"] = None
        body["abstained"] = True
        body["abstention_reason"] = "no_candidate_above_threshold"
        body["grounded_in"] = ["m-something"]
    return body


def unanswered_without_abstaining(op: Interface, body: dict[str, Any]) -> dict[str, Any]:
    """The outcome the scorer has no bin for: neither a correct refusal nor a wrong answer,
    which is how a systematic failure leaves a report."""
    if op is Interface.ANSWER:
        body["answered"] = False
        body["answer"] = None
        body["abstained"] = False
        body["abstention_reason"] = None
        body["grounded_in"] = []
    return body


# -- section 4.6: trust is derived from origin, not asserted -----------------------------


def launder_trust(op: Interface, body: dict[str, Any]) -> dict[str, Any]:
    """Report web-origin content as user-asserted.

    This is the failure the laundering suite exists to catch, and it is invisible to every
    other metric: precision, latency and token cost are all unaffected. If the suite does
    not fail on this stub, it is measuring nothing.
    """
    for entry in body.get("written", []):
        if entry.get("effective_trust") == "untrusted_content":
            entry["effective_trust"] = "user_asserted"
    target = body.get("retrieval") if op is Interface.ANSWER else body
    if isinstance(target, dict):
        for item in target.get("injected", []):
            if item.get("effective_trust") == "untrusted_content":
                item["effective_trust"] = "user_asserted"
    return body


# -- shape violations --------------------------------------------------------------------


def add_undeclared_field(_op: Interface, body: dict[str, Any]) -> dict[str, Any]:
    body["confidence_hint"] = 0.8
    return body


def desynchronize(_op: Interface, body: dict[str, Any]) -> dict[str, Any]:
    if "query_id" in body:
        body["query_id"] = "q-somebody-elses"
    elif "session_id" in body:
        body["session_id"] = "s-somebody-elses"
    return body


MUTATIONS: dict[str, tuple[Mutation, str]] = {
    "missing_cost": (drop_cost, "omits the cost block on every interface"),
    "missing_embedded_cost": (
        drop_embedded_retrieval_cost,
        "omits cost on the retrieval result embedded in an answer",
    ),
    "hedged_abstention": (hedge_abstention, "answered=false with a populated answer"),
    "tombstone_injector": (inject_tombstone, "injects a tombstone as a candidate"),
    "abstained_with_injected": (
        abstain_while_injecting,
        "abstains while returning injected memories",
    ),
    "empty_without_abstention": (
        inject_nothing_without_abstaining,
        "injects nothing without calling it an abstention",
    ),
    "grounded_while_abstained": (
        ground_while_abstaining,
        "abstains while citing grounding memories",
    ),
    "unanswered_without_abstention": (
        unanswered_without_abstaining,
        "answered=false without abstained=true",
    ),
    "trust_launderer": (launder_trust, "reports web-origin content as user-asserted"),
    "undeclared_field": (add_undeclared_field, "returns a field section 4 does not declare"),
    "desynchronized": (desynchronize, "echoes the wrong correlation id"),
}
"""name -> (mutation, what it does). `clock_reader` is not here: it is not a response
mutation but a stub that reads a system clock, built from the oracle's own knob."""
