"""Report honesty: what must be present, and what must never be filled in by proxy."""

from __future__ import annotations

import ast
from pathlib import Path

import pytest

import marlowe_eval
from marlowe_eval.datasets import fixture_locomo, fixture_longmemeval, reporting, verify
from marlowe_eval.datasets.errors import CorpusFormatError
from marlowe_eval.runner import RunConfig, run
from marlowe_eval_stubs import build_target


def _run(**kwargs):
    return run(
        lambda corpus: build_target("stub://oracle", corpus),
        {"longmemeval-s": fixture_longmemeval()},
        RunConfig(target="stub://oracle", **kwargs),
    )


def test_headline_is_absent_and_says_so_without_a_label_set():
    """The failure mode this guards against is a proxy quietly standing in for K1."""
    report = _run().report
    assert report["labels"]["injection_precision_human"] is None
    assert report["labels"]["human_labels"] == "absent"
    joined = " ".join(report["notes"])
    assert "NO HUMAN LABEL SET" in joined
    assert "is NOT that metric" in joined


def test_there_is_no_field_called_injection_precision():
    """Three differently-named precisions exist so nobody can fill the headline in with
    whichever one they happened to have."""
    report = _run().report
    flat = repr(report)
    assert '"injection_precision"' not in flat
    assert "'injection_precision'" not in flat
    assert "evidence_precision" in flat


def test_every_declared_benchmark_appears_even_when_not_run():
    """Omission reads identically to 'never meant to be there'. It must be impossible."""
    report = _run().report
    keys = {row["key"] for row in report["benchmark_coverage"]}
    assert keys == {d.key for d in reporting.DECLARED}
    not_run = [r for r in report["benchmark_coverage"] if r["status"] == "not_run"]
    assert all(r["reason"] for r in not_run), "a skipped benchmark must carry its reason"


def test_evidence_precision_is_labelled_as_not_the_headline():
    report = _run().report
    note = report["benchmarks"]["longmemeval-s"]["evidence_precision"]["note"]
    assert "NOT the K1 headline" in note


def test_accuracy_carries_its_grader_and_comparability_caveat():
    report = _run().report
    detail = report["benchmarks"]["longmemeval-s"]["answer_accuracy"]["detail"]
    assert detail["grader"] == "containment-v1"
    assert detail["comparable_to_published"] is False
    assert "LLM judge" in detail["comparability_note"]


def test_every_accuracy_number_ships_with_its_cost():
    """Section 5.7: report the pair, or it does not ship."""
    report = _run().report
    for block in report["benchmarks"]["longmemeval-s"].values():
        if isinstance(block, dict) and "metric" in block and "value" in block:
            if block["metric"] in {"answer_accuracy", "abstention_accuracy"}:
                assert "cost" in block
                assert "retrieval_tokens" in block["cost"]
                assert "latency_ms" in block["cost"]


def test_budget_misses_are_scored_not_protocol_errors():
    """An implementation exceeding 300 ms is a result the eval exists to produce."""
    report = _run(latency_budget_ms=0, token_budget=0).report
    cost = report["benchmarks"]["longmemeval-s"]["answer_accuracy"]["cost"]
    assert cost["latency_ms"]["over_budget"] > 0
    assert cost["retrieval_tokens"]["over_budget"] > 0
    assert report["protocol"]["errors"] == []


# -- corpus adapters ----------------------------------------------------------------------


def test_fixtures_load_and_cover_their_categories():
    lme = fixture_longmemeval()
    assert len(lme.cases) == 8
    assert any(c.is_abstention for c in lme.cases)
    assert all(c.gold_turn_ids for c in lme.cases if not c.is_abstention)

    locomo = fixture_locomo()
    assert len(locomo.cases) == 5
    assert {c.category for c in locomo.cases} == set(
        __import__("marlowe_eval.datasets.locomo", fromlist=["CATEGORIES"]).CATEGORIES
    )


def test_verify_corpus_rejects_a_drifted_release(tmp_path):
    """The whole point of verify-corpus: a renamed field is a loud failure, not a bisect.

    Uses `longmemeval-m`, which shares the adapter and carries no pinned digest, so this
    exercises schema drift rather than the integrity guard tested separately below.
    """
    drifted = tmp_path / "drifted.json"
    drifted.write_text('[{"question_id": "x", "question": "y"}]', encoding="utf-8")
    with pytest.raises((verify.VerificationFailed, CorpusFormatError)):
        verify.verify("longmemeval-m", drifted)


def test_verify_corpus_rejects_a_file_that_is_not_the_pinned_release(tmp_path):
    """LongMemEval-S has a pinned digest since 2026-08-02, so the integrity guard fires
    before anything else. Handing it the fixture -- a different file -- must be refused."""
    from marlowe_eval.datasets import FIXTURE_DIR
    from marlowe_eval.datasets.fetch import IntegrityError

    with pytest.raises(IntegrityError, match="digest mismatch"):
        verify.verify("longmemeval-s", FIXTURE_DIR / "longmemeval_s.sample.json")


def test_verify_corpus_flags_a_count_mismatch(tmp_path):
    """The fixtures are 8 questions; a real release is 500. Counts must be enforced.

    Checked through `longmemeval-m`, which shares the adapter and is still unpinned, so the
    count check is reached rather than short-circuited by the digest guard above.
    """
    from marlowe_eval.datasets import FIXTURE_DIR

    with pytest.raises(verify.VerificationFailed) as exc:
        verify.verify("longmemeval-m", FIXTURE_DIR / "longmemeval_s.sample.json")
    assert any("expected 500" in f for f in exc.value.findings)


def test_fetch_refuses_a_corpus_with_no_pinned_digest(tmp_path):
    """Fails on mismatch, never warns -- and an unpinned digest is unverifiable, not fine.
    LoCoMo is still unpinned, so it is the live example."""
    from marlowe_eval.datasets import FIXTURE_DIR
    from marlowe_eval.datasets.fetch import MANIFEST, IntegrityError, verify_file

    assert MANIFEST["locomo"].sha256 is None
    with pytest.raises(IntegrityError, match="no pinned sha256"):
        verify_file(FIXTURE_DIR / "locomo.sample.json", MANIFEST["locomo"])


def test_fetch_rejects_a_digest_mismatch(tmp_path):
    """The pinned case: a file that is not the release is refused, loudly."""
    from marlowe_eval.datasets import FIXTURE_DIR
    from marlowe_eval.datasets.fetch import MANIFEST, IntegrityError, verify_file

    spec = MANIFEST["longmemeval-s"]
    assert spec.sha256 is not None, "LongMemEval-S digest was pinned on 2026-08-02"
    with pytest.raises(IntegrityError, match="digest mismatch"):
        verify_file(FIXTURE_DIR / "longmemeval_s.sample.json", spec)


def test_temporal_coherence_is_reported_not_failed():
    """The real release dates 76/500 questions before their own history. That is a corpus
    property, so it is a statistic -- failing on it would reject the real data."""
    from marlowe_eval.datasets.verify import _temporal_coherence

    corpus = fixture_longmemeval()
    block = _temporal_coherence(corpus)
    assert set(block) >= {
        "questions_dated_before_their_own_history",
        "of_which_gold_evidence_is_after_the_question",
        "max_overshoot_days",
    }


def test_answer_accuracy_reports_the_temporally_clean_subset():
    """Reported beside the headline, never instead of it: a gap between the two is a signal
    about clock handling rather than recall."""
    report = _run().report
    detail = report["benchmarks"]["longmemeval-s"]["answer_accuracy"]["detail"]
    subset = detail["temporally_clean_subset"]
    assert set(subset) >= {"accuracy", "n", "excluded"}
    assert "diagnostic, not the headline" in subset["note"]


# -- the contract package's isolation --------------------------------------------------


def test_contract_package_depends_on_nothing_else_in_the_harness():
    """Section 4 is the only contract M0a may depend on; if `contract/` starts importing
    the rest of the harness, that boundary has stopped being real."""
    root = Path(marlowe_eval.__file__).parent / "contract"
    offenders = []
    for path in root.rglob("*.py"):
        tree = ast.parse(path.read_text(encoding="utf-8"))
        for node in ast.walk(tree):
            if isinstance(node, ast.ImportFrom) and node.level >= 2:
                offenders.append(f"{path.name}: from {'.' * node.level}{node.module or ''}")
    assert not offenders, f"contract/ reached outside section 4: {offenders}"
