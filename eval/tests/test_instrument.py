"""Does the instrument recover a known input?

An eval harness that cannot read back a planted value is not measuring; it is producing
numbers. These tests set a knob on the reference stub and require the harness's arithmetic
to return it. They are the closest thing M0a has to a calibration standard before any real
implementation exists.
"""

from __future__ import annotations

import pytest

from marlowe_eval.adapter.base import Client
from marlowe_eval.datasets import synthetic
from marlowe_eval.metrics import (
    ContainmentGrader,
    abstention_accuracy,
    accuracy,
    evidence_precision,
)
from marlowe_eval.metrics.security import PoisoningResult
from marlowe_eval.suites import benchmark, poisoning, staleness
from marlowe_eval_stubs import build_target

TOLERANCE = 0.06
"""A seeded point check, not a statistical claim. ~450 draws puts one sigma near 0.023, so
this band is roughly 2.5 sigma -- wide enough not to flake, tight enough that a broken
join or an off-by-one in the attribution logic fails it."""


def _records(spec: str, cases: int = 200):
    corpus = synthetic.build(cases=cases, seed=11)
    client = Client(build_target(spec, corpus))
    return benchmark.run_benchmark(client, corpus)


@pytest.mark.parametrize("knob", [0.5, 0.7, 0.9])
def test_evidence_precision_recovers_the_precision_knob(knob):
    result = _records(f"stub://oracle?precision={knob}")
    measured = evidence_precision(result.records).value
    assert abs(measured - knob) < TOLERANCE, (
        f"stub injected gold evidence at p={knob}; the harness measured {measured:.4f}. "
        "The instrument does not recover its own input."
    )


@pytest.mark.parametrize("knob", [0.6, 0.9])
def test_abstention_accuracy_recovers_the_abstention_knob(knob):
    result = _records(f"stub://oracle?abstention_rate={knob}")
    measured = abstention_accuracy(result.records, result.cost).value
    assert abs(measured - knob) < 0.12  # ~50 abstention cases; wider band by construction


def test_answer_accuracy_recovers_the_accuracy_knob():
    result = _records("stub://oracle?answer_accuracy=0.8")
    measured = accuracy(result.records, ContainmentGrader(), result.cost).value
    assert abs(measured - 0.8) < 0.08


def test_staleness_probe_recovers_the_supersession_half_life():
    """The stub decays a superseded memory on a 7-day half-life; the sweep must find it."""
    curve, _ = staleness.run(
        lambda corpus: build_target("stub://oracle", corpus), pairs=40
    )
    assert curve.half_life_ms is not None, "half-life not observed within the sweep horizon"
    days = curve.half_life_ms / 86_400_000
    assert 5.0 < days < 10.0, f"planted half-life 7d, measured {days:.2f}d"


def test_unattributable_injections_are_counted_not_guessed():
    """A memory absent from the ingest mapping is unattributable, not a false positive."""
    result = _records("stub://oracle")
    precision = evidence_precision(result.records)
    assert precision.unattributable == 0
    assert precision.gold + precision.distractor > 0


# -- the security types refuse to report half a result ------------------------------------


def test_poisoning_result_requires_utility_retention():
    """Section 8.3: a defense that blocks everything by breaking the agent is not a
    defense. ASR alone is unconstructible."""
    with pytest.raises(TypeError):
        PoisoningResult(suite="minja", attacks=4, successes=0)  # type: ignore[call-arg]


def test_poisoning_result_rejects_impossible_counts():
    with pytest.raises(ValueError):
        PoisoningResult(
            suite="minja", attacks=2, successes=5,
            utility_under_attack=1.0, utility_clean=1.0, detail={},
        )


# -- the laundering suite must discriminate, or it measures nothing -------------------------


def test_laundering_suite_passes_a_conforming_stub():
    report = poisoning.run(lambda c: build_target("stub://oracle", c), cases=10)
    assert report["trust_assertions"]["failed"] == 0


def test_laundering_suite_catches_a_launderer():
    """Invisible to every other metric: precision, latency and tokens are all unaffected
    when web-origin content is relabelled user-asserted."""
    report = poisoning.run(
        lambda c: build_target("stub://broken.trust_launderer", c), cases=10
    )
    assert report["trust_assertions"]["failed"] > 0
    failure = report["trust_assertions"]["failures"][0]
    assert failure["channel"] == "web"
    assert failure["expected_effective_trust"] == "untrusted_content"
    assert failure["observed_effective_trust"] == "user_asserted"


def test_unsigned_writes_are_visibly_refused():
    """K3 sets this at 0%. Refusal must be visible in `rejected`, not inferred from
    absence -- section 4.6 makes that a first-class outcome for exactly this reason."""
    report = poisoning.run(lambda c: build_target("stub://oracle", c), cases=10)
    unsigned = report["families"]["unsigned_write"]
    assert unsigned["asr"] == 0.0
    assert all(a["visibly_rejected"] for a in unsigned["detail"]["attempts"])


def test_utility_retention_has_a_real_denominator():
    """A retention ratio computed against a zero baseline is not a measurement."""
    report = poisoning.run(lambda c: build_target("stub://oracle", c), cases=10)
    assert report["utility"]["clean"] > 0.0
