"""Score the fitted gate against LongMemEval-S, held-out and all-500.

**`eval/` is not modified by this script or by anything in `tools/`.** The harness exposes
fixtures through `--corpus` and reaches a real corpus only through `verify-corpus`; that
omission is the scoreboard's choice, and an M0b session adding `--corpus-path` would be the
implementation reshaping the measurement's interface for its own convenience. So this driver
imports `marlowe_eval` as a library and calls the same `run()` and `write_artifacts()` the CLI
calls. If `--corpus-path` is right long-term it is an M0a change, argued separately.

Reporting rule, and it is the reason this file exists rather than two CLI invocations:

  * the **held-out** figure is the headline
  * the **all-500** figure ships with its contamination attached to the value itself, the same
    treatment `corpus_variant` gets

A bare contaminated number next to a bare clean one gets quoted wrong, so there is no place in
the output where the all-500 value appears without the label.

Two driver-side diagnostics are computed from `run.jsonl` and are **labelled as driver-side**,
because they are not harness metrics and must not be mistaken for them:

  coverage      fraction of answerable cases with at least one GOLD memory injected. Precision
                without it is gameable by injecting almost nothing.
  curve         precision and coverage as a function of a swept calibrated-precision cut. This
                is where pre-registered Number 2 is read from. Sweeping a cut for REPORTING is
                not tuning; the shipped threshold is frozen at 0.95 and does not move.

MarloweCowork port (M2): paths point at this repo (`target/release/marlowe.exe`, `models/` or
$MARLOWE_MODELS, `data/` for the corpus and runs). M4.5 restores `--rerank-provider`,
`--rerank-plan` and `--gpu-runtime` for a `--features cuda` binary (pass it with `--binary`);
the shipped path is still CPU, depth 10, one graph. Each pass now also writes
`recall.json` (driver-side R@k, see `tools/recall_at_k.py`); the reporting rules are unchanged.
See tools/README.md for the exact commands.

    python tools/score_longmemeval.py --heldout-only --reranking models/ms-marco-MiniLM-L-2-v2-ft-session-j
"""

from __future__ import annotations

import argparse
import json
import os
import sys
from collections import defaultdict
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(REPO / "eval" / "src"))

from marlowe_eval.datasets import longmemeval  # noqa: E402
from marlowe_eval.datasets.model import Corpus  # noqa: E402
from marlowe_eval.metrics.records import Attributor  # noqa: E402
from marlowe_eval.runner import RunConfig, run, write_artifacts  # noqa: E402
from marlowe_eval_stubs import build_target  # noqa: E402

sys.path.insert(0, str(REPO / "tools"))
from recall_at_k import recall_at_k  # noqa: E402

SPLIT_PATH = REPO / "tools" / "split.json"
ARTIFACT_PATH = REPO / "crates" / "marlowe-memory" / "artifacts" / "gate-frozen-v5.json"
BINARY = REPO / "target" / "release" / "marlowe.exe"
# Same resolution order the product uses for its first candidate: $MARLOWE_MODELS, else <repo>/models.
# The harness spawns the binary with a minimal environment (section 4.0.9), so model paths travel
# on the command line, never through MARLOWE_* variables.
MODELS_DIR = Path(os.environ.get("MARLOWE_MODELS") or REPO / "models")
MODEL_DIR = MODELS_DIR / "jina-embeddings-v2-small-en"
SHIPPED_RERANKER = MODELS_DIR / "ms-marco-MiniLM-L-2-v2-ft-session-j"
CACHE_DIR = REPO / ".embedding-cache"
PREREG_PATH = REPO / "tools" / "preregistration-session-f.json"
DEFAULT_OUT = REPO / "data" / "runs" / "longmemeval"

# Overridable so the COLD retrieval latency can be measured on a fresh cache. A warm cache
# removes the query's forward pass from the timed span, so the warm P95 understates what a user
# pays on a fresh profile -- Session C measured 36 ms cold against 24 ms warm. The fix is to
# report from a cache-cold run and say so, never to exclude the embedding from the timed span.
_cache_dir = CACHE_DIR
# The cross-encoder directory the binary is pointed at, or the literal "off". Set from --reranking,
# which is REQUIRED. `None` formats into the target string as "None" and the binary refuses to
# start -- loud, and impossible to mistake for a measurement.
_reranking = None
# Passed straight through; `None` means "do not pass the flag", so the binary's own measured
# default applies and there is exactly one place the shipped value is written down.
_rerank_threads = None
_rerank_batch = None
_injection_coverage = None
_embedder_workers = None
_rerank_provider = None
_rerank_plan = None
_gpu_runtime = None

CONTAMINATION = (
    "the frozen gate's weights and isotonic calibration were fit on the {fit_cases} cases of "
    "the pre-registered fit split, which are a subset of these {n}. This value is "
    "optimistically biased and is NOT the headline. The headline is the held-out figure."
)

# **Pre-registered in runs/session-b/PREREGISTRATION.json**, before any of this ran, and carried
# unchanged through C, D and E. Named as Session B's rather than the current session's on purpose:
# what makes Number 2 comparable across four sessions is that its read rule has not moved, and a
# comment claiming each session re-registered them would obscure exactly that.
NUMBER_2_COVERAGE_TARGET = 0.25
BUDGET_TOKENS = 7000
BUDGET_P95_MS = 300
ABSTENTION_INJECTION_CEILING = 0.20
DEGENERATE_COVERAGE_FLOOR = 0.05

# How close a Number 2b read point must sit to its target coverage to count as MATCHED. Session D
# read at 0.607 against a 0.504 target and its delta was not like-for-like; the tolerance errs
# toward declaring UNMATCHED because a suppressed delta costs a comparison while a spurious one
# gets quoted.
MATCHED_COVERAGE_TOL = 0.02


def subset(corpus: Corpus, query_ids: set[str], suffix: str) -> Corpus:
    cases = tuple(c for c in corpus.cases if c.query_id in query_ids)
    keep = {c.session_id for c in cases}
    return Corpus(
        name=f"{corpus.name}{suffix}",
        sessions=tuple(s for s in corpus.sessions if s.session_id in keep),
        cases=cases,
        categories=corpus.categories,
    )


def iter_ndjson(path: Path):
    """Yield one parsed object per NDJSON line.

    **File iteration, never `str.splitlines()`.** `splitlines()` also splits on U+2028, U+2029
    and U+0085, which JSON does not require to be escaped inside a string — and LongMemEval
    transcripts contain them. Splitting there tears a frame in half and the parse fails partway
    through a 470 KB line, which is how this was found. Python's file iterator splits on `\\n`
    alone, which is what section 4.0.2 defines a frame boundary to be.
    """
    with path.open("r", encoding="utf-8", newline="\n") as fh:
        for line in fh:
            line = line.strip()
            if line:
                yield json.loads(line)


def read_records(transcript: Path, corpus: Corpus) -> list[dict]:
    """Rebuild per-query records from the harness's own `run.jsonl`.

    `run.jsonl` is the section 4.0.3 wire log, not a record dump, and `RunResult` does not
    expose the records the report was built from. So they are reconstructed here from the same
    two things the harness itself joins on:

      * the section 4.6 ingest responses, for `written[].turn_id -> memory_ids`
      * the section 4.7 answer responses, for the embedded retrieval result

    The join runs through the harness's **own** `Attributor`, imported rather than
    reimplemented. A hand-rolled attribution here would be a second, unvalidated definition of
    "gold" sitting beside the one the report uses, and the two would drift.
    """
    attributor = Attributor()
    responses: list[dict] = []

    for frame in iter_ndjson(transcript):
        body = frame.get("body")
        if not isinstance(body, dict):
            continue
        if frame.get("op") == "ingest" and "written" in body:
            for written in body["written"]:
                attributor.record(written["turn_id"], list(written["memory_ids"]))
        elif frame.get("op") == "answer" and "retrieval" in body:
            responses.append(body)

    gold_map = corpus.gold_map()
    case_of = {c.query_id: c for c in corpus.cases}

    records = []
    for body in responses:
        query_id = body["query_id"]
        case = case_of.get(query_id)
        if case is None:
            continue
        gold_turns = gold_map.get(query_id, frozenset())
        retrieval = body["retrieval"]
        injected = []
        for item in retrieval["injected"]:
            attribution, turn_id = attributor.attribute(item["memory_id"], gold_turns)
            injected.append(
                {
                    "memory_id": item["memory_id"],
                    "score": item["score"],
                    "calibrated_precision": item["calibrated_precision"],
                    "attribution": attribution,
                    "turn_id": turn_id,
                }
            )
        records.append(
            {
                "query_id": query_id,
                "category": case.category,
                "is_abstention": case.is_abstention,
                "injected": injected,
                "retrieval_tokens": retrieval["cost"]["retrieval_tokens"],
                "latency_ms": retrieval["cost"]["latency_ms"]["total"],
                "abstention_reason": retrieval.get("abstention_reason"),
                "considered": retrieval["considered"],
                "gate_version": retrieval["gate"]["version"],
            }
        )
    return records


def diagnostics(records: list[dict], scored: dict[str, list[dict]]) -> dict:
    """Driver-side only. Not harness metrics; labelled as such wherever they appear.

    `records` are what actually went on the wire (the injected set at the frozen threshold).
    `scored` is every candidate the gate judged, carrying the implementation's own
    `calibrated_precision` — that is what the swept curve is built from, because when the gate
    abstains everywhere the wire carries nothing to sweep.
    """
    answerable = [r for r in records if not r.get("is_abstention")]
    abstention = [r for r in records if r.get("is_abstention")]
    answerable_ids = [r["query_id"] for r in answerable]

    def at_cut(cut: float) -> dict:
        gold = distractor = 0
        covered = 0
        cases_with_any = 0
        kept_total = 0
        for query_id in answerable_ids:
            kept = [c for c in scored.get(query_id, ()) if c["calibrated_precision"] >= cut]
            if kept:
                cases_with_any += 1
            kept_total += len(kept)
            hit = False
            for item in kept:
                if item["attribution"] == "gold":
                    gold += 1
                    hit = True
                elif item["attribution"] == "distractor":
                    distractor += 1
            if hit:
                covered += 1
        attributed = gold + distractor
        n = len(answerable_ids)
        return {
            "cut": round(cut, 6),
            "precision": round(gold / attributed, 6) if attributed else None,
            "coverage": round(covered / n, 6) if n else 0.0,
            "cases_with_any_injection": cases_with_any,
            "mean_candidates_passing_per_case": round(kept_total / n, 3) if n else 0.0,
            "gold": gold,
            "distractor": distractor,
        }

    # Breakpoints where the gate's own calibration has them, plus a coarse grid.
    observed = sorted({round(c["calibrated_precision"], 6) for rows in scored.values() for c in rows})
    grid = sorted({round(x / 20, 4) for x in range(21)} | set(observed))
    curve = [at_cut(c) for c in grid]

    injections_on_abstention = sum(len(r["injected"]) for r in abstention)
    wire_gold = sum(
        1 for r in answerable for i in r["injected"] if i["attribution"] == "gold"
    )
    wire_attributed = sum(
        1 for r in answerable for i in r["injected"] if i["attribution"] in ("gold", "distractor")
    )
    wire_covered = sum(
        1 for r in answerable if any(i["attribution"] == "gold" for i in r["injected"])
    )

    return {
        "_note": (
            "DRIVER-SIDE DIAGNOSTICS, not harness metrics. `coverage` is reported beside every "
            "precision because precision alone is gameable by injecting almost nothing."
        ),
        "_curve_caveat": (
            "The swept curve applies the GATE only. The section 5.7 token budget is not "
            "re-applied at each cut, so at permissive cuts the curve's coverage is an UPPER "
            "BOUND on what would actually be injected. `mean_candidates_passing_per_case` is "
            "reported at every point so a reader can see where the budget would start to bite "
            "(roughly 15-40 LongMemEval turns fit in 7,000 estimated tokens)."
        ),
        "_curve_source": (
            "the implementation's own score and calibrated_precision for every scored "
            "candidate, read from --dump-gate-features. Not recomputed in Python: a second "
            "implementation of the gate's arithmetic with nothing comparing the two is the "
            "mismatch pattern this project keeps paying for."
        ),
        "answerable_cases": len(answerable),
        "abstention_cases": len(abstention),
        "injections_on_abstention_cases": injections_on_abstention,
        "scored_candidates": sum(len(v) for v in scored.values()),
        "on_the_wire_at_frozen_threshold": {
            "precision": round(wire_gold / wire_attributed, 6) if wire_attributed else None,
            "coverage": round(wire_covered / len(answerable), 6) if answerable else 0.0,
            "gold": wire_gold,
            "attributed": wire_attributed,
            "cases_with_any_injection": sum(1 for r in answerable if r["injected"]),
        },
        "curve": curve,
    }


def _band_for(value: float, bands: list[dict]) -> str:
    """Pick the pre-registered band containing `value`.

    Conditions are parsed from the pre-registration rather than restated here. **That is the
    point of this function existing at all**: the first version of this driver hardcoded Session
    B's thresholds, and when Session C's bands changed it kept reporting the old verdict beside
    the new number. Nothing failed -- the value was right and the label was wrong, which is
    exactly the shape of mismatch this project keeps paying for.
    """
    numbers = []
    for band in bands:
        condition = band["condition"]
        tokens = condition.replace("precision", " ").replace("<=", " ").replace("<", " ")
        tokens = tokens.replace(">=", " ").split()
        bounds = [float(t) for t in tokens]
        if condition.startswith("precision >="):
            lo, hi = bounds[0], float("inf")
        elif condition.startswith("precision <"):
            lo, hi = float("-inf"), bounds[0]
        else:
            lo, hi = bounds[0], bounds[1]
        numbers.append((lo, hi, band["verdict"]))
    for lo, hi, verdict in numbers:
        if lo <= value < hi:
            return verdict
    raise SystemExit(f"value {value} matches no pre-registered band; refusing to invent one")


def _read_at_coverage(curve: list[dict], target: float) -> dict:
    """Precision at the most selective cut still reaching `target` coverage.

    The shared mechanic behind Number 2 and Number 2b. Extracted so the two cannot drift apart:
    they differ only in the coverage they are read at, and a second hand-written copy of this
    lookup is how two numbers described as "the same rule at a different coverage" stop being
    that.
    """
    reachable = max((p["coverage"] for p in curve), default=0.0)
    if reachable < target:
        return {
            "value": None,
            "reason": (
                f"coverage never reaches {target} anywhere on the curve (maximum "
                f"{reachable:.4f}), so the pre-registered read point does not exist. Reported "
                "as unmeasurable rather than substituted with a nearby point."
            ),
            "max_coverage": round(reachable, 6),
        }
    eligible = [p for p in curve if p["coverage"] >= target]
    point = max(eligible, key=lambda p: p["cut"])
    if point["precision"] is None:
        return {"value": None, "reason": "no attributable injections at that cut"}
    return {
        "value": point["precision"],
        "read_at_cut": point["cut"],
        "coverage_there": point["coverage"],
    }


def number_two(curve: list[dict], bands: list[dict] | None) -> dict:
    """Pre-registered Number 2: precision at the cut where coverage first reaches 0.25.

    **Read rule unchanged since Session B**, so B, C and D are directly comparable. Session D
    registers no bands for it -- the rule fixes a coverage FLOOR and cannot express a
    simultaneous rise in precision and coverage, which is what Number 2b exists to address.
    """
    out = _read_at_coverage(curve, NUMBER_2_COVERAGE_TARGET)
    out["read_rule"] = (
        f"most selective cut whose coverage is at least {NUMBER_2_COVERAGE_TARGET} "
        "(a coverage FLOOR; unchanged since Session B)"
    )
    if out["value"] is not None and bands:
        out["verdict"] = _band_for(out["value"], bands)
        out["bands_applied"] = [b["condition"] + " -> " + b["verdict"] for b in bands]
    elif out["value"] is not None:
        out["verdict"] = None
        out["no_bands"] = (
            "Session D registers no bands for Number 2. Its coverage-floor rule cannot express "
            "that precision and coverage both rose -- the limitation Session C recorded -- so "
            "the comparison that carries a verdict this session is Number 2b, at matched "
            "coverage. Number 2 is reported for cross-session comparability only."
        )
    out["band_source"] = "tools/preregistration-session-f.json, written before the fit"
    return out


GENERALIZATION_FAILURE_MARGIN = 0.05


def calibration_generalization(per_cue_top: dict[str, float], n2: dict, n3: dict) -> dict:
    """The standing check: fit-split prediction vs held-out measurement.

    **Computed, not typed into a write-up.** Sessions B and C recorded this pair by hand in
    prose; a check that only exists when someone remembers to do the arithmetic is a check that
    stops happening. The failure it catches is invisible in every other number the harness
    produces -- precision, coverage, ASR and latency all look identical whether the curve
    generalizes or not.

    Two levels under v3. The overall pair continues the B/C series so the three sessions stay
    comparable; the per-cue pairs are new, because with one curve per cue a single cue's
    calibration can memorize while the other masks it in the max.
    """
    rule = (
        f"held-out BELOW the fit-split prediction by more than {GENERALIZATION_FAILURE_MARGIN} "
        "absolute is a signal about the CALIBRATION, not about the cue: investigate the fit "
        "before adding anything else."
    )

    def pair(name: str, predicted: float, measured: float | None, measured_from: str) -> dict:
        out = {
            "predicted_on_fit_split": predicted,
            "measured_on_heldout": measured,
            "measured_from": measured_from,
        }
        if measured is None:
            out["fired"] = None
            out["reading"] = "held-out value unmeasurable; the pair cannot be formed"
            return out
        delta = measured - predicted
        out["delta"] = round(delta, 6)
        out["fired"] = delta < -GENERALIZATION_FAILURE_MARGIN
        out["reading"] = (
            "MEMORIZED CALIBRATION SUSPECTED -- investigate the fit before anything else"
            if out["fired"]
            else (
                "generalizing, conservative" if delta >= 0 else "generalizing, slightly optimistic"
            )
        )
        return out

    per_cue = {}
    for cue, predicted in per_cue_top.items():
        block = n3.get(f"{cue}_alone", {})
        per_cue[cue] = pair(
            cue, predicted, block.get("precision"), f"number_3 {cue}_alone precision"
        )

    return {
        "_what": "fit-split predicted precision vs held-out measured precision",
        "_why": rule,
        "failure_margin": GENERALIZATION_FAILURE_MARGIN,
        "overall": {
            **pair(
                "max over cues",
                max(per_cue_top.values()),
                n2.get("value"),
                "number_2_cue_capability.value",
            ),
            "session_b_reference": "0.309 predicted -> 0.334 measured",
            "session_c_reference": "0.3176 predicted -> 0.371 measured",
        },
        "per_cue": per_cue,
        "any_fired": any(p.get("fired") for p in [*per_cue.values()]),
    }


def number_two_b(curve: list[dict], target: float, reference: float | None) -> dict:
    """Pre-registered Number 2b: the same rule, read at MATCHED coverage.

    Session C moved from 0.334 precision at 0.453 coverage to 0.371 at 0.504. Both rose, and
    Number 2's coverage-FLOOR rule returned "dense adds nothing measurable" for it. STATE.md
    required that a matched-coverage statistic be pre-registered before a future fit and never
    substituted after one; `tools/preregistration-session-f.json` is that registration.

    Reported as a COMPANION, never a replacement. Number 2's verdict stands on Number 2's rule.
    """
    out = _read_at_coverage(curve, target)
    out["read_rule"] = (
        f"most selective cut whose coverage is at least {target} -- Session C's REALISED "
        "coverage, not its floor"
    )
    out["matched_to"] = target
    out["session_c_reference_at_its_own_read_point"] = reference

    # **Whether the read is actually MATCHED is now computed, not left to prose.**
    #
    # `_read_at_coverage` returns the most selective cut whose coverage is AT LEAST the target.
    # When the curve is coarse the nearest such cut can sit far above it -- Session D's landed at
    # 0.607 against a 0.504 target -- and then the two numbers are read at different coverages and
    # their difference is not a quality signal at all. Session D caught that by hand and said so in
    # prose. A check that depends on someone remembering to do it is a check that stops happening,
    # so it is computed here and **the delta is SUPPRESSED when unmatched** rather than emitted
    # with a caveat beside it. A number that must not be used should not be in the file.
    #
    # The tolerance errs toward declaring UNMATCHED, which is the safe direction: a suppressed
    # delta costs a comparison, a spurious one gets quoted.
    coverage_there = out.get("coverage_there")
    matched = coverage_there is not None and abs(coverage_there - target) <= MATCHED_COVERAGE_TOL
    out["matched"] = matched
    out["matched_tolerance"] = MATCHED_COVERAGE_TOL
    if out["value"] is not None and reference is not None:
        if matched:
            out["delta_vs_session_c"] = round(out["value"] - reference, 6)
        else:
            out["delta_vs_session_c"] = None
            out["why_no_delta"] = (
                f"NOT MATCHED. The read point is coverage {coverage_there}, against a target of "
                f"{target} (tolerance {MATCHED_COVERAGE_TOL}). The two values are read at "
                "different coverages, so their difference measures selectivity as much as "
                "precision and is not a quality signal. Suppressed rather than reported with a "
                "caveat."
            )
    out["band"] = None
    out["not_a_replacement"] = (
        "Number 2 is reported unchanged beside this. Substituting a matched-coverage read for "
        "the pre-registered floor rule after seeing a number is exactly what STATE.md forbade."
    )
    out["band_source"] = "tools/preregistration-session-f.json, written before the fit"
    return out


def number_three(scored: dict[str, list[dict]], records: list[dict]) -> dict:
    """Pre-registered Number 3: what each cue can do ALONE.

    Sweeps each raw cue score on its own and reads precision at the most selective cut still
    reaching coverage 0.25 -- the same read rule as Number 2, so the three numbers are directly
    comparable.

    **This is the only number in the session that separates a weak embedder from a broken
    fusion.** Both present as a flat Number 2 and they have opposite remedies.
    """
    answerable = [r["query_id"] for r in records if not r.get("is_abstention")]
    n = len(answerable)

    def sweep(feature: str) -> dict:
        values = sorted(
            {round(c[feature], 4) for q in answerable for c in scored.get(q, ())}
        )
        if not values:
            return {"value": None, "reason": f"no {feature} values on the held-out split"}
        # A coarse grid over the observed range; the exact cut does not matter, the reachable
        # precision at a fixed coverage does.
        step = max(1, len(values) // 200)
        best = None
        for cut in values[::step]:
            gold = distractor = covered = 0
            for query_id in answerable:
                kept = [c for c in scored.get(query_id, ()) if c[feature] >= cut]
                hit = False
                for item in kept:
                    if item["attribution"] == "gold":
                        gold += 1
                        hit = True
                    elif item["attribution"] == "distractor":
                        distractor += 1
                if hit:
                    covered += 1
            coverage = covered / n if n else 0.0
            attributed = gold + distractor
            if coverage >= NUMBER_2_COVERAGE_TARGET and attributed:
                point = {
                    "cut": cut,
                    "precision": round(gold / attributed, 6),
                    "coverage": round(coverage, 6),
                    "gold": gold,
                    "distractor": distractor,
                }
                # Most selective cut still meeting the coverage floor.
                if best is None or cut > best["cut"]:
                    best = point
        return best or {"value": None, "reason": "coverage never reached the floor"}

    lexical = sweep("lexical_bm25")
    dense = sweep("dense_cosine")
    # The MARGIN sweeps, new in Session E. Not a replacement for the raw sweeps -- both are
    # reported, because they answer different questions:
    #
    #   the RAW pair  is the cross-session anchor. Sessions B, C and D swept exactly this, and a
    #                 move here means the held-out POPULATION changed rather than the features.
    #   the MARGIN pair is what the v4 curves are fit on, so it is the only held-out measurement
    #                 that is like-for-like with the fit-split prediction the calibration
    #                 generalization check compares against. Reading that check against the raw
    #                 sweep would silently compare a margin-fit curve to a raw-score measurement.
    lexical_margin = sweep("lexical_margin")
    dense_margin = sweep("dense_margin")
    reading = None
    if lexical.get("precision") is not None and dense.get("precision") is not None:
        if dense["precision"] >= lexical["precision"]:
            reading = (
                "dense is the stronger cue alone. A flat Number 2 therefore points at the "
                "FUSION or the calibration, not at the embedder."
            )
        else:
            reading = (
                "dense is WEAKER than lexical alone. If Number 2 also failed to improve, the "
                "embedder is suspect -- check pooling and normalization against the committed "
                "reference vectors first, then the truncation rate."
            )
    return {
        "_what": (
            "Each cue swept ALONE at the same read rule as Number 2. Driver-side; no second "
            "fit and no second artifact."
        ),
        "_the_two_pairs": (
            "The RAW pair is the cross-session anchor and must match Sessions C and D exactly -- "
            "the cue set did not change, so a move there means the held-out population did. The "
            "MARGIN pair is what the v4 curves are fit on and is what the calibration "
            "generalization check reads, so that comparison stays like-for-like."
        ),
        "lexical_bm25_alone": lexical,
        "dense_cosine_alone": dense,
        "lexical_margin_alone": lexical_margin,
        "dense_margin_alone": dense_margin,
        "reading": reading,
        "band_source": "tools/preregistration-session-f.json number_3, written before the fit",
    }


def read_scored(dump: Path, transcript: Path, corpus: Corpus) -> dict[str, list[dict]]:
    """Every candidate the gate judged, joined to gold. Keyed by query_id.

    The gold join reuses the same `Attributor` the report was built with, rebuilt from the
    transcript's ingest responses.
    """
    attributor = Attributor()
    for frame in iter_ndjson(transcript):
        body = frame.get("body")
        if frame.get("op") == "ingest" and isinstance(body, dict) and "written" in body:
            for written in body["written"]:
                attributor.record(written["turn_id"], list(written["memory_ids"]))

    gold_map = corpus.gold_map()
    out: dict[str, list[dict]] = defaultdict(list)
    for row in iter_ndjson(dump):
        if "calibrated_precision" not in row:
            raise SystemExit(
                "the dump carries no calibrated_precision: it was produced in --fit-mode, "
                "which loads no gate. The scoring run must dump WITH the gate."
            )
        attribution, turn_id = attributor.attribute(
            row["memory_id"], gold_map.get(row["query_id"], frozenset())
        )
        out[row["query_id"]].append(
            {
                "memory_id": row["memory_id"],
                "score": row["score"],
                "calibrated_precision": row["calibrated_precision"],
                "passes": row["passes"],
                "attribution": attribution,
                "turn_id": turn_id,
                # The per-cue features, carried so Number 3 can sweep each cue alone. Read from
                # the dump the implementation wrote, never recomputed here: a second
                # implementation of a cue's arithmetic sitting beside the real one with nothing
                # comparing them is the mismatch pattern this project keeps paying for.
                #
                # BOTH the raw scores and the margins. The raw pair is the cross-session anchor
                # -- Sessions B, C and D swept exactly these, and the unchanged-cue check reads
                # them. The margin pair is what the v4 curves are actually fit on, and it is what
                # the calibration generalization pair has to be read against for the comparison
                # to stay like-for-like.
                "lexical_bm25": row["lexical_bm25"],
                "dense_cosine": row["dense_cosine"],
                "lexical_margin": row["lexical_margin"],
                "dense_margin": row["dense_margin"],
                "margin": row["margin"],
                "winning_cue": row["winning_cue"],
            }
        )
    return dict(out)


def score_one(
    corpus: Corpus, out_dir: Path, seed: int, clock: int
) -> tuple[dict, list[dict], dict[str, list[dict]]]:
    out_dir.mkdir(parents=True, exist_ok=True)
    dump = out_dir / "scored-candidates.ndjson"
    # `--reranking` is REQUIRED by the binary and takes an explicit value, so it appears here
    # spelled out rather than omitted. `_reranking` is set from the CLI and defaults to the
    # pinned cross-encoder directory; passing `off` measures the pruning-only ablation, and the
    # value is echoed into the run's summary so a number can never be read without knowing which
    # configuration produced it.
    target = (
        f"exec://{BINARY} --eval-adapter --profile-root {{profile_root}} "
        f"--embedder-model {MODEL_DIR} --embedding-cache {_cache_dir} "
        f"--reranking {_reranking} "
        + (f"--embedder-workers {_embedder_workers} " if _embedder_workers is not None else "")
        + (f"--rerank-threads {_rerank_threads} " if _rerank_threads is not None else "")
        + (f"--rerank-batch {_rerank_batch} " if _rerank_batch is not None else "")
        + (f"--injection-coverage {_injection_coverage} " if _injection_coverage is not None else "")
        + (f"--rerank-provider {_rerank_provider} " if _rerank_provider is not None else "")
        + (f"--rerank-plan {_rerank_plan} " if _rerank_plan is not None else "")
        + (f"--gpu-runtime {_gpu_runtime} " if _gpu_runtime is not None else "")
        + f"--dump-gate-features {dump}"
    )
    result = run(
        lambda c: build_target(target, c),
        {corpus.name: corpus},
        # **Benchmark only, and the dump is why.** The harness spawns a fresh process per
        # suite -- the clock probe alone spawns four -- and every spawn opens the dump with
        # truncate, so a probe running after the benchmark would leave a dump containing the
        # probe's candidates and nothing else. The curve would then be built from four
        # synthetic queries while every other number came from 249 real ones, and nothing in
        # the output would say so. The probes are run separately, without a dump.
        RunConfig(target=target, seed=seed, clock_ms=clock, suites=("benchmark",)),
    )
    paths = write_artifacts(result, out_dir)
    transcript = Path(paths["transcript"])
    return (
        result.report,
        read_records(transcript, corpus),
        read_scored(dump, transcript, corpus),
    )


def benchmark_block(report: dict) -> dict:
    """The per-corpus block the harness scored. One corpus per run here."""
    benchmarks = report.get("benchmarks") or {}
    if len(benchmarks) != 1:
        raise SystemExit(f"expected exactly one scored corpus, got {sorted(benchmarks)}")
    return next(iter(benchmarks.values()))


def budget_verdict(report: dict, records: list[dict]) -> dict:
    """Read from the harness's own cost block, never recomputed from the records."""
    block = benchmark_block(report)
    cost = (block.get("answer_accuracy") or {}).get("cost") or {}
    tokens = cost.get("retrieval_tokens") or {}
    latency = cost.get("latency_ms") or {}
    p95 = latency.get("p95")
    over_token = tokens.get("over_budget")
    if p95 is None or over_token is None:
        raise SystemExit(
            "the harness's cost block did not carry a p95 latency and an over-budget count. "
            "Refusing to substitute a number computed here: section 5.7's pair is the "
            "harness's to report."
        )
    passes = over_token == 0 and p95 <= BUDGET_P95_MS
    return {
        "passes": passes,
        "retrieval_p95_ms": p95,
        "p95_budget_ms": BUDGET_P95_MS,
        "retrieval_tokens_max": tokens.get("max"),
        "cases_over_token_budget": over_token,
        "token_budget": BUDGET_TOKENS,
        "source": "harness report.json cost block, not recomputed",
        "if_violated": (
            "the precision numbers are VOID, not caveated: K1 is defined at these budgets."
        ),
    }


def power_verdict(at_op: dict, prereg: dict, answerable: int) -> dict:
    """Pre-registered power floor: is the operating-point precision READABLE at all?

    **Partial coverage at high precision is a PASS** — brief §5.5 is precision-first and K1 carries
    no coverage term. What this floor catches is different and narrower: a precision computed over
    a handful of injections cannot distinguish 0.95 from 0.90 whatever it reads, and quoting it as
    if it could is the failure. Below `n_min` the number is reported UNDERPOWERED, which is a
    statement about the INSTRUMENT rather than about the gate.

    `n_min` is read from the pre-registration, never recomputed here — the whole point is that it
    was fixed before the number existed.
    """
    floor = prereg["power_floor"]
    n_min = floor["n_min"]
    attributed = at_op["attributed"]
    cases_firing = at_op["cases_with_any_injection"]
    m = (attributed / cases_firing) if cases_firing else 0.0
    return {
        "n_min": n_min,
        "attributed_injections": attributed,
        "powered": attributed >= n_min,
        "verdict": "POWERED" if attributed >= n_min else "UNDERPOWERED",
        "derivation": floor["derivation"],
        "mean_attributed_injections_per_firing_case": round(m, 4),
        "cases_with_any_injection": cases_firing,
        "answerable_cases": answerable,
        "implied_coverage_floor": (
            round(n_min / (answerable * m), 6) if m > 0 else None
        ),
        "implied_coverage_floor_note": (
            f"the coverage this split would need at the MEASURED {m:.2f} injections per firing "
            f"case to reach {n_min} attributed injections. Derived after the fact and reported; "
            "it never moves the floor."
        ),
        "partial_coverage_is_a_pass": floor["_registered_explicitly"],
        "if_underpowered": floor["if_violated"],
    }


def label_set_projection(records: list[dict], prereg: dict) -> dict:
    """COMPANION, no verdict. Is K1's own validation instrument reachable at this operating point?

    K1's headline is human-judged and needs ≥400 judged injections with ≥50 per category. A gate
    whose output cannot support that draw has a headline nobody can compute — which is worth
    knowing early, and is **not** a reason to fail a precision-first result. Registered with no
    verdict precisely because at one injection per firing case it is unmeetable by construction,
    and a band on an unreachable quantity is the ADR-010 error.

    Counted from the **all-500** run. That run is contaminated for PRECISION, and these are counts
    rather than precisions, so the contamination does not apply to what is reported here — stated
    explicitly rather than left for a reader to work out.
    """
    spec = prereg["label_set_feasibility"]
    per_category: dict[str, int] = defaultdict(int)
    for record in records:
        if record.get("is_abstention"):
            continue
        for item in record["injected"]:
            if item["attribution"] in ("gold", "distractor"):
                per_category[record["category"]] += 1

    total = sum(per_category.values())
    want_total = spec["requirement"]["total_judged_injections"]
    want_each = spec["requirement"]["per_category"]

    # **Iterate the corpus's OWN category list, not the observed keys.** A category with zero
    # injections does not appear in `per_category` at all, so `{c: n for c, n in per_category ...}`
    # would find nothing short and report the requirement MET at zero injections -- the exact
    # empty-denominator failure `evidence_precision` already carries a vacuity note for. The
    # category list is read from the pre-registration, which took it from the corpus.
    known = spec["corpus_category_counts"]
    short = {
        c: per_category.get(c, 0)
        for c in sorted(known)
        if c != "abstention" and per_category.get(c, 0) < want_each
    }
    return {
        "_status": spec["_status"],
        "_why_no_verdict": spec["_why_no_verdict"],
        "counted_from": (
            "the all-500 run. Contaminated for PRECISION, but these are COUNTS, so the "
            "contamination does not apply to this projection."
        ),
        "requirement": spec["requirement"],
        "attributed_injections_total": total,
        "meets_total": total >= want_total,
        "attributed_injections_per_category": dict(sorted(per_category.items())),
        "categories_below_the_per_category_target": short,
        "meets_per_category": not short,
        "reading": (
            "K1's label set is drawable at this operating point"
            if total >= want_total and not short
            else "K1's label set is NOT drawable at this operating point; the headline metric "
            "would remain uncomputable even with a human judge available"
        ),
    }



def _record_binary_identity(out: Path) -> None:
    """Stamp **which binary produced this run**, beside the run.

    This exists because a run was very nearly attributed to a source change it did not measure.
    `score_longmemeval.py` drives `target/release/marlowe.exe`, and `cargo run --example` does not
    rebuild it — so a scoring pass launched right after editing a constant happily measures the
    *previous* value and writes it into a directory named for the new one. The source said 1024,
    the artifact said 8192, and the directory name agreed with the source.

    It is the pipe-verified/live-verified family that CLAUDE.md already logs four times: the source
    emits it versus the *running process* emits it. The instrument there was `--dev`'s outbound
    dump — the bytes the running process actually sent. The instrument here is the binary's own
    mtime and digest, written where the numbers are, so a later reader can check the artifact rather
    than trusting the directory name.

    Cheap on purpose: a digest of a 30 MB file takes milliseconds and it is the only thing standing
    between a stale build and a published number.
    """
    import hashlib

    if not BINARY.exists():
        (out / "BINARY.json").write_text(
            json.dumps({"error": f"{BINARY} does not exist"}, indent=2) + "\n", encoding="utf-8"
        )
        return
    stat = BINARY.stat()
    digest = hashlib.sha256(BINARY.read_bytes()).hexdigest()
    (out / "BINARY.json").write_text(
        json.dumps(
            {
                "_what": (
                    "the binary that produced this run. Compare its mtime against the source you "
                    "believe you measured -- `cargo run --example` does not rebuild marlowe.exe."
                ),
                "path": str(BINARY),
                "sha256": digest,
                "size": stat.st_size,
                "mtime_epoch": int(stat.st_mtime),
            },
            indent=2,
        )
        + "\n",
        encoding="utf-8",
    )


def main() -> int:
    global BINARY
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out", default=str(DEFAULT_OUT))
    parser.add_argument(
        "--binary",
        default=str(BINARY),
        help="the marlowe binary to score. Default target/release/marlowe.exe; its digest and "
        "mtime are stamped into BINARY.json beside the run.",
    )
    parser.add_argument(
        "--embedding-cache",
        default=str(CACHE_DIR),
        help="embedding cache directory. Point at an empty one to measure COLD retrieval "
        "latency -- the number the budget condition is read from.",
    )
    parser.add_argument(
        "--heldout-only",
        action="store_true",
        help="score the held-out split alone. Used for the cold-cache latency read, where the "
        "all-500 pass would double the embedding cost for a number already measured warm.",
    )
    parser.add_argument(
        "--max-cases",
        type=int,
        default=None,
        help="score only the first N cases of the split, in sorted query_id order (deterministic, "
        "so the subset is reproducible). **Only legitimate for the CACHE-COLD latency read**, and "
        "the reason it exists is a measured constraint rather than convenience: on a cold cache "
        "the implementation must embed a whole session's turns inside one section 4.6 ingest "
        "call, and some LongMemEval sessions exceed the harness's section 4.0.7 30 s deadline, "
        "which aborts the run before it scores anything. Retrieval P95 is a PER-QUERY property -- "
        "every query still ranks the same ~493 candidates -- so a bounded subset measures the "
        "same quantity with a smaller n. It must never be used for a QUALITY number: the "
        "pre-registered population is the whole held-out split, and scoring a subset would be "
        "choosing the population after seeing the split.",
    )
    parser.add_argument(
        "--reranking",
        required=True,
        help=(
            "the cross-encoder directory (shipped: models/ms-marco-MiniLM-L-2-v2-ft-session-j), or "
            "the literal 'off' for the pruning-only ablation. "
            "Passed straight through to the binary, which requires the flag and has no default. "
            "REQUIRED here too, since Session K: this defaulted to the int8 directory, and after "
            "the shipped graph moved to the Session J fine-tune that default would have scored "
            "the OLD graph and labelled the result shipped. A default whose staleness is "
            "unobservable is the pattern CLAUDE.md names."
        ),
    )
    parser.add_argument(
        "--fit-only",
        action="store_true",
        help="score the FIT split alone and write its dump. Session K, Part 3: the conformal tau "
        "is calibrated on fit-split margins and must come through the same pipeline the held-out "
        "curve is measured through. Writes no summary.json and produces no quality number -- the "
        "gate's parameters have seen this split, so a number from it is not a held-out number.",
    )
    parser.add_argument(
        "--rerank-threads", type=int, default=None,
        help="ONNX intra-op threads for the cross-encoder. Omit to use the binary's measured "
             "default of 1 (ADR-003's 1-vCPU target). Adoption requires byte-identity of the "
             "dump: multi-threaded ORT can change reduction order inside a matmul.")
    parser.add_argument(
        "--rerank-batch", choices=["on", "off"], default=None,
        help="score the depth-10 slate in one forward pass. Omit to use the binary's default.")
    parser.add_argument(
        "--injection-coverage", choices=["declared", "full"], default=None,
        help="K1 condition 3's controlled-comparison arm. Omit for the binary's shipped "
             "`declared` operating point. `full` is the MEASUREMENT arm: admit rank 1 whenever "
             "a margin exists, ignoring the threshold -- never shippable, used so downstream "
             "consumers (e.g., the QA tester) see memories on every query. The chosen arm is "
             "stamped into every response's gate version (+coverage-full).")
    parser.add_argument(
        "--embedder-workers", type=int, default=None,
        help="ONNX sessions for batch embedding at ingest. Throughput only: a vector is a pure "
             "function of its text. Omit for the binary's default, min(cpus, 8).")
    parser.add_argument(
        "--rerank-provider", choices=["cpu", "cuda", "directml"], default=None,
        help="cross-encoder device; GPU values need a `--features cuda` binary and are required, "
             "never fallen back from. Omit for the binary's CPU default.")
    parser.add_argument(
        "--rerank-plan", choices=["shipped", "cascade"], default=None,
        help="`cascade` = depth 30 -> 10 then six-graph RRF (crates/marlowe-memory/src/cascade.rs).")
    parser.add_argument(
        "--gpu-runtime", default=None,
        help="root written by scripts/fetch-gpu-runtime.ps1 (the harness strips the environment, "
             "so $MARLOWE_GPU_RUNTIME would not reach the binary).")
    parser.add_argument("--seed", type=int, default=7)
    parser.add_argument("--clock", type=int, default=1_780_000_000_000)
    args = parser.parse_args()

    if args.fit_only and (args.heldout_only or args.max_cases is not None):
        raise SystemExit(
            "--fit-only is exclusive with --heldout-only and --max-cases. It scores one split for "
            "one purpose (the tau calibration dump) and combining it with the latency read would "
            "produce a subset of the wrong split under either flag's label."
        )

    artifact = json.loads(ARTIFACT_PATH.read_text(encoding="utf-8"))
    if artifact.get("state") != "fitted":
        raise SystemExit(
            f"{ARTIFACT_PATH} is not fitted. Run `python tools/fit_gate.py` and "
            "`cargo build --release` first."
        )
    split = json.loads(SPLIT_PATH.read_text(encoding="utf-8"))
    if artifact["split_digest"] != split["digest"]:
        raise SystemExit(
            "the artifact was fit under a different split than tools/split.json now holds "
            f"({artifact['split_digest']} vs {split['digest']}). Refusing to report a "
            "held-out number against a split the gate did not actually hold out."
        )

    global _cache_dir, _reranking, _rerank_threads, _rerank_batch, _injection_coverage
    global _embedder_workers, _rerank_provider, _rerank_plan, _gpu_runtime
    BINARY = Path(args.binary).resolve()
    _cache_dir = Path(args.embedding_cache)
    _reranking = args.reranking
    _rerank_threads = args.rerank_threads
    _rerank_batch = args.rerank_batch
    _injection_coverage = args.injection_coverage
    _embedder_workers = args.embedder_workers
    _rerank_provider = args.rerank_provider
    _rerank_plan = args.rerank_plan
    _gpu_runtime = str(Path(args.gpu_runtime).resolve()) if args.gpu_runtime else None

    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    _record_binary_identity(out)
    (out / "ENVIRONMENT.json").write_text(
        json.dumps(
            {
                "_what": (
                    "the scorer this run used. The embedder is always CPU; the cross-encoder runs "
                    "on rerank_provider (required, never fallen back from). The harness hands the "
                    "target a minimal environment (section 4.0.9), so paths travel on the command line."
                ),
                "embedder_provider": "cpu",
                "rerank_provider": args.rerank_provider or "cpu",
                "rerank_plan": args.rerank_plan or "shipped",
                "gpu_runtime": _gpu_runtime,
                "models_dir": str(MODELS_DIR),
                "reranking": args.reranking,
            },
            indent=2,
        )
        + "\n",
        encoding="utf-8",
    )
    corpus = longmemeval.load(REPO / split["corpus_path"])

    heldout_ids = set(split["heldout"])
    if args.max_cases is not None:
        if not args.heldout_only:
            raise SystemExit(
                "--max-cases is only for the cache-cold latency read and requires --heldout-only. "
                "A quality number computed over a subset would be choosing the scored population "
                "after seeing the split, which is exactly what the pre-registered population "
                "exists to prevent."
            )
        heldout_ids = set(sorted(heldout_ids)[: args.max_cases])
        print(
            f"NOTE: scoring a {len(heldout_ids)}-case SUBSET of the held-out split. Latency only; "
            "no quality number from this pass is comparable to a full-split one."
        )

    if args.fit_only:
        # Session K, Part 3. The conformal tau is calibrated on FIT-split queries whose rank 1 is
        # not gold, and it has to be calibrated through the same pipeline the held-out curve is
        # measured through. Carving fit ids out of the `all` pass would NOT do: the `all` pass
        # ingests all 500 cases' sessions, so a fit query there ranks against a different store
        # than it does in a fit-only run. Same construction as Session J's per-split caches.
        fit_ids = set(split["fit"])
        print(f"scoring fit ({len(fit_ids)} cases) ...")
        sub = subset(corpus, fit_ids, "-fit")
        score_one(sub, out / "fit", args.seed, args.clock)
        print(f"  -> {out / 'fit'}")
        print(f"\nreranking: {_reranking}")
        # No summary.json, for the same reason --heldout-only writes none: this pass exists to
        # produce a dump, and a half-populated summary is how a partial run gets quoted as a full
        # one. Every published QUALITY number still comes from the held-out split.
        print("No summary.json written -- this pass produces the fit-split dump only.")
        return 0

    passes = [("heldout", heldout_ids, "-heldout")]
    if not args.heldout_only:
        passes.append(("all", {c.query_id for c in corpus.cases}, ""))

    runs = {}
    for name, ids, suffix in passes:
        print(f"scoring {name} ({len(ids)} cases) ...")
        sub = subset(corpus, ids, suffix)
        report, records, scored = score_one(sub, out / name, args.seed, args.clock)
        recall = recall_at_k(out / name, sub)
        (out / name / "recall.json").write_text(json.dumps(recall, indent=2) + "\n", encoding="utf-8")
        runs[name] = {"report": report, "records": records, "scored": scored, "recall": recall}
        print(f"  -> {out / name}")

    if args.heldout_only:
        # The latency read and nothing else. No summary.json is written, deliberately: a
        # half-populated summary sitting where the real one belongs is how a partial run gets
        # quoted as a full one.
        budget = budget_verdict(runs["heldout"]["report"], runs["heldout"]["records"])
        print()
        print(f"cache: {_cache_dir}")
        print(f"reranking: {_reranking}")
        print(f"cases scored: {len(heldout_ids)}"
              + ("  (SUBSET -- latency only)" if args.max_cases is not None else ""))
        print(f"retrieval P95: {budget['retrieval_p95_ms']} ms  (budget {BUDGET_P95_MS} ms)")
        print(f"max retrieval tokens: {budget['retrieval_tokens_max']} (budget {BUDGET_TOKENS})")
        print(f"budget passes: {budget['passes']}")
        rk = runs["heldout"]["recall"]
        label = (
            "SUBSET -- NOT comparable to the published held-out R@k"
            if args.max_cases is not None
            else "held-out, driver-side; published n=229: R@1 0.6725 R@5 0.8865 R@10 0.9039"
        )
        print(
            f"R@k (n={rk['n']}): R@1 {rk['R@1']}  R@3 {rk['R@3']}  R@5 {rk['R@5']}  "
            f"R@10 {rk['R@10']}   [{label}]"
        )
        print("\nNo summary.json written -- this pass writes recall.json and the latency read only.")
        return 0

    heldout_diag = diagnostics(runs["heldout"]["records"], runs["heldout"]["scored"])
    all_diag = diagnostics(runs["all"]["records"], runs["all"]["scored"])

    heldout_ep = benchmark_block(runs["heldout"]["report"])["evidence_precision"]
    all_ep = benchmark_block(runs["all"]["report"])["evidence_precision"]
    prereg = json.loads(PREREG_PATH.read_text(encoding="utf-8"))
    n2 = number_two(heldout_diag["curve"], prereg["number_2"].get("bands"))
    # The matched coverage is Session C's own realised coverage, read out of the pre-registration
    # rather than retyped here -- the same value its Number 2b definition text refers to.
    n2b = number_two_b(
        heldout_diag["curve"],
        prereg["session_c_baseline"]["number_2"]["coverage_there"],
        prereg["number_2b"]["session_c_reference_at_this_coverage"],
    )
    n3 = number_three(runs["heldout"]["scored"], runs["heldout"]["records"])
    budget = budget_verdict(runs["heldout"]["report"], runs["heldout"]["records"])

    at_op = heldout_diag["on_the_wire_at_frozen_threshold"]
    abstention_rate = (
        heldout_diag["injections_on_abstention_cases"] / heldout_diag["abstention_cases"]
        if heldout_diag["abstention_cases"]
        else 0.0
    )

    def precision_block(ep: dict, split_name: str, n: int) -> dict:
        """One reported precision, with everything that qualifies it attached to the value.

        `evidence_precision` is a ratio over ATTRIBUTED injections, so it reads 0.0 both when
        every injection was wrong and when there were no injections at all. Those are opposite
        findings and the field cannot tell them apart, so the distinction is carried here --
        beside the value, not in surrounding prose.
        """
        attributed = ep["gold"] + ep["distractor"]
        block = {
            "value": ep["value"],
            "split": split_name,
            "n": n,
            "corpus_variant": split["corpus_variant"],
            "metric": "evidence_precision",
            "gold": ep["gold"],
            "distractor": ep["distractor"],
            "attributed_injections": attributed,
            "note": (
                "precision against benchmark gold evidence. NOT the K1 headline, which is "
                "human-judged and needs a label set that does not exist yet."
            ),
        }
        if attributed == 0:
            block["value_is_vacuous"] = True
            block["vacuity"] = (
                "0.0 here means NOTHING WAS INJECTED, not that the injections were wrong. The "
                "ratio has an empty denominator and the metric cannot distinguish the two. "
                "Read number_1_operating_point and number_2_cue_capability instead."
            )
        return block

    # Per-cue top blocks. Under the v3 fusion the reachable precision is a PER-CUE quantity --
    # the fusion takes the max, so it does not enter this number at all. That is what makes it a
    # clean read on the best cue's most confident region, undiluted by a joint logistic.
    per_cue_top = {
        name: max(b[1] for b in artifact["cue_curves"][name])
        for name in artifact["cue_features"]
    }

    # **The artifact this driver READ must be the artifact the run SCORED WITH.**
    #
    # It is read from disk by path while the binary embeds its own copy with `include_str!`, so
    # the two can disagree the moment a session bumps the artifact version and this path is not
    # updated with it — which is exactly what happened here on the first Session F scoring pass.
    # Every measured number stayed correct, because those come off the wire; but the reported
    # `gate` block and the calibration-generalization PREDICTIONS were the previous version's,
    # compared against this version's held-out measurements. Nothing failed and nothing looked
    # wrong.
    #
    # §4.2's gate stamp is the run's own statement of what scored it, so that is what this checks.
    stamps = {r["gate_version"] for r in runs["heldout"]["records"]}
    if stamps != {artifact["version"]}:
        raise SystemExit(
            f"{ARTIFACT_PATH.name} declares version {artifact['version']!r}, but the run stamped "
            f"{sorted(stamps)} on the wire. This driver is reading a different artifact than the "
            "binary scored with; refusing rather than reporting one gate's curves beside another "
            "gate's numbers."
        )

    summary = {
        "session": "M0b Session F",
        "what_this_is": (
            "Two cues -- lexical BM25 and the dense ONNX embedder -- fused by MAX OVER PER-CUE "
            "CALIBRATED PRECISIONS. The cue set is unchanged from Session C; only the combiner "
            "moved. NOT the five-cue system K1 measures; entity-graph, temporal and causal are "
            "still absent, so a low number remains a statement about an incomplete cue set."
        ),
        "gate": {
            "version": artifact["version"],
            "fusion": artifact["fusion"],
            "threshold": artifact["threshold"],
            "max_calibrated_precision_on_the_curve": max(per_cue_top.values()),
            "max_calibrated_precision_per_cue": per_cue_top,
            "cue_features": artifact["cue_features"],
            "cue_curve_blocks": {
                name: len(artifact["cue_curves"][name]) for name in artifact["cue_features"]
            },
            "inert_features": artifact["inert_features"],
            "fit_cases": artifact["fit_cases"],
            "fit_rows": artifact["fit_rows"],
            "fit_positives": artifact["fit_positives"],
        },
        "evidence_precision": {
            "headline": precision_block(heldout_ep, "heldout", split["heldout_cases"]),
            "all_cases": {
                **precision_block(all_ep, "all", split["cases"]),
                "contaminated": True,
                "contamination": CONTAMINATION.format(
                    fit_cases=split["fit_cases"], n=split["cases"]
                ),
            },
        },
        "recall_at_k": {
            "_what": "driver-side R@k on the shipped ranking key (tools/recall_at_k.py)",
            "headline": {**runs["heldout"]["recall"], "split": "heldout"},
            "all_cases": {
                **runs["all"]["recall"],
                "split": "all",
                "contaminated": True,
                "contamination": CONTAMINATION.format(
                    fit_cases=split["fit_cases"], n=split["cases"]
                ),
            },
        },
        "number_1_operating_point": {
            "precision": at_op["precision"],
            "coverage": at_op["coverage"],
            "cases_with_any_injection": at_op["cases_with_any_injection"],
            "gate_abstained_everywhere": at_op["cases_with_any_injection"] == 0,
            "band": None,
            "note": (
                "No band by pre-registration. Reported whatever it is, including a gate that "
                "abstained everywhere. Read from the WIRE -- what the implementation actually "
                "injected at the frozen 0.95 threshold."
            ),
        },
        "number_2_cue_capability": n2,
        "number_2b_at_matched_coverage": n2b,
        "number_3_per_cue": n3,
        "calibration_generalization": calibration_generalization(per_cue_top, n2, n3),
        "power_floor": power_verdict(at_op, prereg, heldout_diag["answerable_cases"]),
        "label_set_feasibility": label_set_projection(runs["all"]["records"], prereg),
        "conditions": {
            "budget": budget,
            "false_evidence_on_abstention_cases": {
                "value": round(abstention_rate, 6),
                "ceiling": ABSTENTION_INJECTION_CEILING,
                "passes": abstention_rate <= ABSTENTION_INJECTION_CEILING,
                "injections": heldout_diag["injections_on_abstention_cases"],
                "cases": heldout_diag["abstention_cases"],
            },
            "degenerate_pass_guard": {
                "coverage": at_op["coverage"],
                "floor": DEGENERATE_COVERAGE_FLOOR,
                "triggered": at_op["coverage"] < DEGENERATE_COVERAGE_FLOOR,
                "if_triggered": (
                    "the operating-point precision is NOT a quality signal: a gate that "
                    "abstains its way to a ratio has not retrieved anything."
                ),
            },
        },
        "diagnostics": {"heldout": heldout_diag, "all_cases": all_diag},
        "provenance": {
            "corpus_sha256": split["corpus_sha256"],
            "corpus_variant": split["corpus_variant"],
            "split_digest": split["digest"],
            "split_rule": split["rule"],
            "preregistration": [
                "tools/preregistration-session-f.json",
            ],
            "eval_unchanged": "no file under eval/ is modified by this driver",
            "reading_the_sub_reports": (
                "heldout/report.json is scored under the corpus name "
                "'longmemeval-s-heldout' so that nothing in it claims the full 500 cases were "
                "run -- its benchmark_coverage row correctly shows longmemeval-s as not_run. "
                "The cost of that naming is that the harness's corpus_variant block in THAT "
                "file reads 'unknown', because the manifest is keyed on the full corpus name. "
                "The variant is `cleaned` for both runs; it is stated here and beside every "
                "value in this file. all/report.json is the genuine full-corpus report and its "
                "variant block is correct."
            ),
        },
    }

    (out / "summary.json").write_text(json.dumps(summary, indent=2) + "\n", encoding="utf-8")

    print()
    print(f"headline (held-out, n={split['heldout_cases']}): {summary['evidence_precision']['headline']['value']}")
    print(f"all-500 (CONTAMINATED):                {summary['evidence_precision']['all_cases']['value']}")
    rk = runs["heldout"]["recall"]
    print(f"R@k held-out (n={rk['n']}): R@1 {rk['R@1']}  R@3 {rk['R@3']}  R@5 {rk['R@5']}  R@10 {rk['R@10']}")
    print(f"Number 1 @ frozen 0.95: precision={at_op['precision']} coverage={at_op['coverage']}")
    print(f"Number 2 cue capability: {n2}")
    print(f"Number 3 per-cue: {n3}")
    print(f"budget: {budget}")
    pw = summary["power_floor"]
    print(
        f"power floor: {pw['verdict']} -- {pw['attributed_injections']} attributed injections "
        f"against a pre-registered n_min of {pw['n_min']}"
    )
    print(f"label set (companion, no verdict): {summary['label_set_feasibility']['reading']}")
    print(f"summary: {out / 'summary.json'}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
