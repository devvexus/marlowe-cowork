"""The sampler's blinding, checked on the bytes a human would actually receive.

ROADMAP.md: *"The judge sees query + injected memory, never the score or whether it was
injected."* Three leaks are possible and all three are tested here -- the score itself,
the ordering, and the injected flag. The last is why packets mix decoys.

These run against the reference stub today, which is the point: the way to find out
whether blinding holds is to serialize a real packet file and look at it, before anyone's
judgments depend on it.
"""

from __future__ import annotations

import json

import pytest

from marlowe_eval.adapter.base import Client
from marlowe_eval.datasets import synthetic
from marlowe_eval.labels import decoy_pool_from, draw, write_packets
from marlowe_eval.suites import benchmark
from marlowe_eval_stubs import build_target


@pytest.fixture(scope="module")
def plan():
    corpus = synthetic.build(cases=60, seed=11)
    client = Client(build_target("stub://oracle", corpus))
    result = benchmark.run_benchmark(client, corpus)
    attributions = [
        (session.session_id, mid, turn.text)
        for session in corpus.sessions
        for turn in session.turns
        for mid in result.attributor.memories_of(turn.turn_id)
    ]
    return draw(
        result.records,
        seed=7,
        decoy_pool=decoy_pool_from(attributions),
        questions={c.query_id: c.question for c in corpus.cases},
    )


def test_packet_type_has_no_score_decile_or_injected_field(plan):
    """Closed by the type, not by a flag: LabelPacket has nowhere to put them."""
    fields = set(plan.packets[0].as_dict())
    assert fields == {"packet_id", "query", "memory"}


def test_serialized_packets_leak_no_score(plan, tmp_path):
    """The real check: write the file a human judges from and read the bytes back.

    Scoped to the packets array, not the whole file. The instructions block deliberately
    contains the word "score" -- it tells the judge they are not being shown one -- and a
    naive substring scan over the file would flag that as a leak.
    """
    path = tmp_path / "packets.json"
    write_packets(list(plan.packets), path)
    payload = json.loads(path.read_text(encoding="utf-8"))

    packets_only = json.dumps(payload["packets"])
    for key in ("score", "calibrated_precision", "decile", "was_injected", "memory_id"):
        assert key not in packets_only, f"the blinded packet file leaks {key!r}"

    for packet in payload["packets"]:
        assert set(packet) == {"packet_id", "query", "memory"}

    # And the numeric scores themselves never appear, under any key name.
    for drawn in plan.draws:
        assert f"{drawn.score:.6f}" not in packets_only


def test_packet_order_does_not_encode_the_decile(plan):
    """Stratify then emit in draw order and the decile is readable off the row number.
    The shuffle is what closes that, so assert the order is not grouped."""
    deciles = [d.decile for d in plan.draws]
    assert deciles != sorted(deciles), "packet order is sorted by decile; blinding leaks"


def test_packet_ids_are_assigned_after_the_shuffle(plan):
    """An id minted before shuffling would carry the stratification order in its digits."""
    assert [d.packet_id for d in plan.draws] == [
        f"p-{i:05d}" for i in range(len(plan.draws))
    ]


def test_sample_mixes_decoys_so_presence_says_nothing(plan):
    """"Never ... whether it was injected" has no content if every packet was injected."""
    injected = sum(1 for d in plan.draws if d.was_injected)
    decoys = sum(1 for d in plan.draws if not d.was_injected)
    assert injected > 0 and decoys > 0
    assert 0.2 <= decoys / len(plan.draws) <= 0.5


def test_sampler_is_deterministic_under_the_seed(plan):
    corpus = synthetic.build(cases=60, seed=11)
    client = Client(build_target("stub://oracle", corpus))
    result = benchmark.run_benchmark(client, corpus)
    attributions = [
        (s.session_id, mid, t.text)
        for s in corpus.sessions
        for t in s.turns
        for mid in result.attributor.memories_of(t.turn_id)
    ]
    again = draw(
        result.records,
        seed=7,
        decoy_pool=decoy_pool_from(attributions),
        questions={c.query_id: c.question for c in corpus.cases},
    )
    assert [p.as_dict() for p in again.packets] == [p.as_dict() for p in plan.packets]
