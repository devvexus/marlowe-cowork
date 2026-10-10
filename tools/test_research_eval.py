"""Offline tests for the research and extraction eval scorers (no model, no GPU, no network).

    python -m pytest tools/test_research_eval.py -q
"""

import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import extraction_eval as ee  # noqa: E402
import research_eval as re_  # noqa: E402

FIX = Path(__file__).resolve().parent / "research_eval_fixtures"


def load(name):
    return json.loads((FIX / f"{name}.json").read_text(encoding="utf-8"))


def test_markers_expand_ranges_lists_and_footnotes_but_not_links():
    md = "A claim [1, 3]. Another [2-4]. A footnote [^5]. A [link](http://x) and [6]."
    pairs, _ = re_.cited_pairs(md)
    assert [(p.sentence, p.n) for p in pairs] == [
        ("A claim.", 1), ("A claim.", 3),
        ("Another.", 2), ("Another.", 3), ("Another.", 4),
        ("A footnote.", 5),
        ("A [link](http://x) and.", 6),
    ]


def test_a_trailing_marker_after_the_full_stop_stays_with_its_sentence():
    pairs, _ = re_.cited_pairs("LoRA freezes the weights. [1] It trains low-rank updates [2].")
    assert [(p.sentence, p.n) for p in pairs] == [("LoRA freezes the weights.", 1), ("It trains low-rank updates.", 2)]


def test_references_section_code_and_headings_are_not_claims():
    md = "# Title [9]\n\nBody claim here [1].\n\n```\nx = [2]\n```\n\n## References\n\n1. Something [3]."
    pairs, _ = re_.cited_pairs(md)
    assert [(p.sentence, p.n) for p in pairs] == [("Body claim here.", 1)]


def test_duplicate_pairs_merge_and_abbreviations_do_not_split():
    md = "Hu et al. report parity [1]. Hu et al. report parity [1]. See e.g. the table [2]."
    pairs, _ = re_.cited_pairs(md)
    assert [(p.sentence, p.n) for p in pairs] == [("Hu et al. report parity.", 1), ("See e.g. the table.", 2)]


def fake_judge(bad_phrase):
    def judge(system, prompt, schema):
        if system is re_.KEYPOINT_SYSTEM:
            return {"verdict": "covered", "contradicted": False, "reason": "fake"}
        statement = prompt.rsplit("STATEMENT:", 1)[1]
        return {"verdict": "unsupported" if bad_phrase in statement else "supported", "reason": "fake"}
    return judge


def test_fixtures_score_as_planted_with_a_fake_judge():
    good, bad = load("good"), load("bad")
    g = re_.score_bundle(good, fake_judge("40% faster")).summary()
    b = re_.score_bundle(bad, fake_judge("40% faster")).summary()
    assert (g["citations"], g["supported"], g["citation_accuracy"]) == (11, 11, 1.0)
    assert (b["citations"], b["supported"], b["unsupported"]) == (11, 10, 1)
    assert abs(b["citation_accuracy"] - 10 / 11) < 1e-9
    assert g["effective_citations"] == 11 and g["supported_references"] == 3
    assert g["uncited_sentences"] == 1  # the "Inferred, not measured" sentence


def test_a_marker_with_no_reference_is_unsupported_without_asking_the_judge():
    bundle = {"question_id": "x", "report_markdown": "Claim [7].", "references": []}

    def judge(*_):
        raise AssertionError("judge called")
    s = re_.score_bundle(bundle, judge).summary()
    assert (s["unsupported"], s["dangling"], s["citation_accuracy"]) == (1, 1, 0.0)


def test_evidence_prefers_passages_then_overlapping_document_chunks():
    ref = {"passages": [{"text": "P one.", "locator": "abstract"}], "document_text": "zzz " * 400 + "rank decomposition matrices"}
    ev = re_.evidence_for(ref, "injects rank decomposition matrices")
    assert ev.startswith("[passage (abstract)] P one.") and "rank decomposition" in ev


def test_aggregate_meets_target_only_at_095():
    rows = [{"judged": 20, "supported": 19, "partial": 0, "citation_accuracy": 0.95, "effective_citations": 19}]
    assert re_.aggregate(rows)["meets_target"]
    rows[0].update(supported=18, citation_accuracy=0.9)
    assert not re_.aggregate(rows)["meets_target"]


def test_question_set_has_ten_questions_with_key_points():
    qs = re_.load_questions()
    assert len(qs) == 10
    assert {q["domain"] for q in qs.values()} == {"ml_research", "engineering", "general"}
    assert all(len(q["key_points"]) >= 4 for q in qs.values())


# ── extraction scorer ────────────────────────────────────────────────────────────────────────

def test_word_prf_matches_wcxb_semantics():
    assert ee.word_prf("", "") == (1.0, 1.0, 1.0)
    assert ee.word_prf("a", "") == (0.0, 0.0, 0.0)
    p, r, f = ee.word_prf("the cat sat", "the cat sat on the mat")
    assert (p, round(r, 4)) == (1.0, 0.5) and round(f, 4) == 0.6667


def test_link_recall_counts_only_links_whose_anchor_sits_in_the_ground_truth():
    links = [["https://a", "Low rank adaptation"], ["https://b", "Home"], ["https://c", "Subscribe to newsletter"]]
    gt = "We use low-rank adaptation here."
    assert ee.link_recall(links, gt, "we use low rank adaptation") == (1, 1)
    assert ee.link_recall(links, gt, "nothing") == (0, 1)


def test_url_link_recall_needs_the_url_inline_in_markdown():
    links = [["https://a/x (1)", "Low rank adaptation"], ["https://b", "Home"]]
    gt = "We use low-rank adaptation here."
    assert ee.url_link_recall(links, gt, "We use [Low rank adaptation](https://a/x%20%281%29).") == (1, 1)
    assert ee.url_link_recall(links, gt, "We use low rank adaptation https://a/x (1)") == (0, 1)


def test_pct_is_nearest_rank():
    v = list(range(1, 101))
    assert ee.pct(v, 0.95) == 95 and ee.pct(v, 0.5) == 50


def test_subtle_fixture_plants_three_distinct_errors():
    b = load("subtle")
    pairs, _ = re_.cited_pairs(b["report_markdown"])
    got = {(p.sentence, p.n) for p in pairs}
    planted = {(re_.strip_markers(x["sentence"]), x["n"]) for x in b["planted_unsupported"]}
    assert len(planted) == 3 and planted <= got
