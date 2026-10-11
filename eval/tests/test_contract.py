"""Section 4 enforcement: every rule the harness claims to enforce, exercised.

Each test names the acceptance criterion it covers. A test here failing means the harness
would accept a response CONTRACTS.md section 4 forbids -- which is worse than a wrong
number, because it is a wrong number that looks fine.
"""

from __future__ import annotations

import pytest

from marlowe_eval.adapter.base import Client
from marlowe_eval.contract import (
    AnswerRequest,
    IngestRequest,
    Interface,
    ProtocolError,
    ProtocolErrorKind,
    RetrievalRequest,
    validate_outbound_ingest,
)
from marlowe_eval.datasets import fixture_longmemeval
from marlowe_eval_stubs import build_target


@pytest.fixture(scope="module")
def corpus():
    return fixture_longmemeval()


def _client(spec, corpus):
    return Client(build_target(spec, corpus))


def _ingest(client, corpus):
    session = corpus.sessions[0]
    return client.ingest(
        IngestRequest(
            clock={"now_ms": session.ingest_at_ms},
            session_id=session.session_id,
            turns=list(session.turns),
        )
    )


def _retrieve(client, corpus):
    case = corpus.cases[0]
    return client.retrieve(
        RetrievalRequest(
            query_id=case.query_id,
            session_id=case.session_id,
            turn_index=0,
            query_text=case.question,
            clock={"now_ms": case.ask_at_ms},
            budget={"max_tokens": 7000, "max_latency_ms": 300},
        )
    )


def _answer(client, corpus):
    case = corpus.cases[0]
    return client.answer(
        AnswerRequest(
            clock={"now_ms": case.ask_at_ms},
            query_id=case.query_id,
            session_id=case.session_id,
            question=case.question,
        )
    )


# -- ACCEPTANCE: "Rejects a response missing `cost` -- on all three interfaces." ---------


@pytest.mark.parametrize("drive", [_ingest, _retrieve, _answer])
def test_missing_cost_rejected_on_every_interface(corpus, drive):
    client = _client("stub://broken.missing_cost", corpus)
    with pytest.raises(ProtocolError) as exc:
        drive(client, corpus)
    assert exc.value.kind is ProtocolErrorKind.MISSING_COST


def test_missing_cost_inside_embedded_retrieval_is_a_retrieve_violation(corpus):
    """The likeliest omission to slip through: well-formed answer, cost missing one level
    down in the record that joins injection precision to answer correctness."""
    client = _client("stub://broken.missing_embedded_cost", corpus)
    _ingest(client, corpus)
    with pytest.raises(ProtocolError) as exc:
        _answer(client, corpus)
    assert exc.value.kind is ProtocolErrorKind.MISSING_COST
    assert exc.value.interface is Interface.RETRIEVE


# -- ACCEPTANCE: "Rejects `answered: false` with a populated `answer`." ------------------


def test_hedged_abstention_rejected(corpus):
    client = _client("stub://broken.hedged_abstention", corpus)
    _ingest(client, corpus)
    with pytest.raises(ProtocolError) as exc:
        _answer(client, corpus)
    assert exc.value.kind is ProtocolErrorKind.HEDGED_ABSTENTION


# -- Section 4.3: a tombstone can never compete for injection precision ------------------


def test_tombstone_never_injected(corpus):
    client = _client("stub://broken.tombstone_injector", corpus)
    _ingest(client, corpus)
    with pytest.raises(ProtocolError) as exc:
        _retrieve(client, corpus)
    assert exc.value.kind is ProtocolErrorKind.TOMBSTONE_INJECTED


# -- Section 4.2: abstention and injection are mutually exclusive, both ways -------------
# Pinned 2026-08-02. The mapping is total: every response either injects or abstains, and
# never both or neither, so one event cannot be scored two ways.


def test_abstained_with_injected_rejected(corpus):
    client = _client("stub://broken.abstained_with_injected", corpus)
    _ingest(client, corpus)
    with pytest.raises(ProtocolError) as exc:
        _retrieve(client, corpus)
    assert exc.value.kind is ProtocolErrorKind.ABSTAINED_WITH_INJECTED


def test_empty_injection_without_abstention_rejected(corpus):
    client = _client("stub://broken.empty_without_abstention", corpus)
    _ingest(client, corpus)
    with pytest.raises(ProtocolError) as exc:
        _retrieve(client, corpus)
    assert exc.value.kind is ProtocolErrorKind.EMPTY_INJECTION_WITHOUT_ABSTENTION


# -- Section 4.7: the same exclusivity on the answer interface ---------------------------


def test_grounded_while_abstained_rejected(corpus):
    client = _client("stub://broken.grounded_while_abstained", corpus)
    _ingest(client, corpus)
    with pytest.raises(ProtocolError) as exc:
        _answer(client, corpus)
    assert exc.value.kind is ProtocolErrorKind.GROUNDED_WHILE_ABSTAINED


def test_unanswered_without_abstention_rejected(corpus):
    client = _client("stub://broken.unanswered_without_abstention", corpus)
    _ingest(client, corpus)
    with pytest.raises(ProtocolError) as exc:
        _answer(client, corpus)
    assert exc.value.kind is ProtocolErrorKind.UNANSWERED_WITHOUT_ABSTENTION


# -- Section 4.6: channel and speaker are closed sets ------------------------------------


def test_unknown_channel_is_an_error_not_a_default(corpus):
    """A silent fallback would make the laundering suite pass while measuring nothing."""
    from marlowe_eval.contract import Origin

    with pytest.raises(Exception):
        Origin(channel="carrier-pigeon", actor="user:primary")


def test_unknown_speaker_is_an_error_not_a_default(corpus):
    from marlowe_eval.contract import Origin, Turn

    with pytest.raises(Exception):
        Turn(
            turn_id="t", speaker="narrator", text="x", occurred_at_ms=0,
            origin=Origin(channel="web", actor="tool:web"),
        )


# -- shape and correlation ---------------------------------------------------------------


def test_undeclared_field_rejected(corpus):
    client = _client("stub://broken.undeclared_field", corpus)
    with pytest.raises(ProtocolError) as exc:
        _ingest(client, corpus)
    assert exc.value.kind is ProtocolErrorKind.UNKNOWN_FIELD


def test_correlation_mismatch_rejected(corpus):
    """A desynchronized stream must fail loudly rather than misattribute a result."""
    client = _client("stub://broken.desynchronized", corpus)
    with pytest.raises(ProtocolError) as exc:
        _ingest(client, corpus)
    assert exc.value.kind is ProtocolErrorKind.CORRELATION_MISMATCH


# -- Section 4.6: the harness declares origin and NEVER trust ----------------------------


def test_harness_may_not_declare_trust():
    poisoned = {
        "contract_version": "1.0",
        "clock": {"now_ms": 0},
        "session_id": "s",
        "turns": [{"turn_id": "t", "origin": {"channel": "web", "trust_class": "user_asserted"}}],
    }
    with pytest.raises(ProtocolError) as exc:
        validate_outbound_ingest(poisoned)
    assert exc.value.kind is ProtocolErrorKind.HARNESS_DECLARED_TRUST


def test_conforming_stub_raises_nothing(corpus):
    client = _client("stub://oracle", corpus)
    _ingest(client, corpus)
    _retrieve(client, corpus)
    _answer(client, corpus)
