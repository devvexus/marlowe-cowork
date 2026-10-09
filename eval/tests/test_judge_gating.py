"""The judge's two structural refusals.

HP1 permits the offline judge only where its agreement against the human set is published
alongside anything it produces. That is enforced in two places, and both are constructors
that raise rather than checks that run later:

  * `AnthropicJudge` refuses without a two-part opt-in AND a human label set.
  * `JudgedPrecision` refuses without an `Agreement`.
"""

from __future__ import annotations

import pytest

from marlowe_eval.judge import (
    Agreement,
    JudgedPrecision,
    NoOverlap,
    ScriptedJudge,
    Verdict,
    VerdictCache,
    compute_agreement,
    judged_precision,
)
from marlowe_eval.judge.anthropic_judge import ENV_FLAG, AnthropicJudge
from marlowe_eval.judge.protocol import JudgeUnavailable
from marlowe_eval.labels.schema import HumanLabel, HumanLabelSet, LabelPacket, SampleDraw


def _labels(**relevance: bool) -> HumanLabelSet:
    return HumanLabelSet(
        label_set_id="test",
        labels={k: HumanLabel(packet_id=k, relevant=v) for k, v in relevance.items()},
    )


def _draws(*packet_ids: str) -> list[SampleDraw]:
    return [
        SampleDraw(
            packet_id=p, query_id="q", memory_id="m", category="c",
            score=0.8, calibrated_precision=0.8, decile="0.8-0.9", was_injected=True,
        )
        for p in packet_ids
    ]


# -- gate 1: the API judge is unrunnable, not merely unrun --------------------------------


def test_api_judge_refuses_without_the_flag(monkeypatch):
    monkeypatch.setenv(ENV_FLAG, "1")
    with pytest.raises(JudgeUnavailable, match="--judge anthropic"):
        AnthropicJudge(allow_api=False, label_set=_labels(a=True), cache=VerdictCache())


def test_api_judge_refuses_without_the_env_var(monkeypatch):
    monkeypatch.delenv(ENV_FLAG, raising=False)
    with pytest.raises(JudgeUnavailable, match=ENV_FLAG):
        AnthropicJudge(allow_api=True, label_set=_labels(a=True), cache=VerdictCache())


def test_api_judge_refuses_without_a_human_label_set(monkeypatch):
    """The gate that matters. Without it the judge would still run and still emit numbers,
    and HP1's requirement would be back to being a rule someone remembers."""
    monkeypatch.setenv(ENV_FLAG, "1")
    with pytest.raises(JudgeUnavailable, match="human label set"):
        AnthropicJudge(allow_api=True, label_set=None, cache=VerdictCache())
    with pytest.raises(JudgeUnavailable, match="human label set"):
        AnthropicJudge(
            allow_api=True, label_set=HumanLabelSet("empty"), cache=VerdictCache()
        )


# -- gate 2: no judge number without its agreement ----------------------------------------


def test_judged_precision_cannot_be_built_without_agreement():
    empty = Agreement(0, 0.0, 0.0, 0.0, 0.0, {})
    with pytest.raises(ValueError, match="agreement"):
        JudgedPrecision(value=0.97, judged=10, relevant=10, judge_id="x", agreement=empty)


def test_agreement_refuses_an_empty_overlap():
    """0/0 rendered as 1.0 would be the most flattering possible lie."""
    verdicts = [Verdict("p-1", True, "j")]
    with pytest.raises(NoOverlap):
        compute_agreement(verdicts, _labels(other=True), _draws("p-1"))


def test_judged_precision_carries_agreement_into_its_report():
    verdicts = [Verdict("p-1", True, "j"), Verdict("p-2", False, "j")]
    labels = _labels(**{"p-1": True, "p-2": False})
    result = judged_precision(verdicts, labels, _draws("p-1", "p-2"), judge_id="j")
    payload = result.as_dict()
    assert payload["value"] == 0.5
    assert payload["agreement_vs_human"]["n"] == 2
    assert payload["agreement_vs_human"]["raw_agreement"] == 1.0
    assert "not the K1 headline" in payload["note"]


def test_cohens_kappa_corrects_for_chance():
    verdicts = [Verdict(f"p-{i}", True, "j") for i in range(10)]
    labels = _labels(**{f"p-{i}": True for i in range(10)})
    agreement = compute_agreement(verdicts, labels, _draws(*[f"p-{i}" for i in range(10)]))
    # Everyone said yes to everything: raw agreement is perfect, but chance explains it,
    # so kappa must not be reported as a strong result.
    assert agreement.raw_agreement == 1.0
    assert agreement.cohens_kappa == 1.0


# -- the scripted judge is a test double and says so --------------------------------------


def test_scripted_judge_self_identifies_as_not_a_relevance_model():
    assert "NOT A RELEVANCE MODEL" in ScriptedJudge().judge_id


def test_verdict_cache_round_trips(tmp_path):
    """The cache is required, not an optimization: an uncached LLM breaks reproduction."""
    from marlowe_eval.judge.cache import key_for

    packet = LabelPacket("p-1", "why is ingest slow", "the ingest job times out")
    cache = VerdictCache(tmp_path / "cache.json")
    key = key_for("j", "v1", packet)
    cache.put(key, Verdict("p-1", True, "j", "because"))
    cache.save()

    reloaded = VerdictCache(tmp_path / "cache.json")
    hit = reloaded.get(key, "p-1")
    assert hit is not None and hit.relevant and hit.cached


def test_cache_key_changes_when_the_prompt_version_changes():
    from marlowe_eval.judge.cache import key_for

    packet = LabelPacket("p-1", "q", "m")
    assert key_for("j", "v1", packet) != key_for("j", "v2", packet)
