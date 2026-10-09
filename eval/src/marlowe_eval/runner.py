"""The run: drive the suites, assemble the report, keep the whole thing deterministic.

Three properties this module is responsible for:

  * **Every declared benchmark appears in the report**, scored or with the reason it was
    not. Omission is the failure mode, not lying (see datasets/reporting.py).
  * **The headline is absent, loudly, when the human label set is.** A run without labels
    does not fall back to a proxy; it says in words that K1's metric was not measured.
  * **The deterministic core is reproducible.** No wall clock, no unseeded randomness, and
    every timing field lands under a key the canonical hash strips.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable

from . import canonical
from .adapter.base import Client, MemorySystem
from .contract import CONTRACT_VERSION, ProtocolError
from .datasets import fetch, reporting
from .datasets.model import Corpus
from .judge.agreement import NoOverlap, judged_precision
from .judge.protocol import Judge
from .labels import decoy_pool_from, draw
from .labels.schema import HumanLabelSet
from .metrics import ContainmentGrader, abstention_accuracy, accuracy, evidence_precision
from .metrics.precision import summarize_trust
from .suites import benchmark, clock, poisoning, staleness

MakeSystem = Callable[[Corpus], MemorySystem]


@dataclass
class RunConfig:
    target: str
    seed: int = 7
    clock_ms: int = 1_780_000_000_000
    token_budget: int = 7000
    latency_budget_ms: int = 300
    suites: tuple[str, ...] = ("benchmark", "probes", "poisoning", "labels")
    labels: HumanLabelSet | None = None
    judge: Judge | None = None
    poisoning_cases: int = 20
    staleness_pairs: int = 24


@dataclass
class RunResult:
    report: dict[str, Any]
    transcript: list[dict[str, Any]] = field(default_factory=list)

    @property
    def core_hash(self) -> str:
        return canonical.core_hash(self.report)


def run(
    make: MakeSystem,
    corpora: dict[str, Corpus],
    config: RunConfig,
) -> RunResult:
    report: dict[str, Any] = {
        "harness": {
            "contract_version": CONTRACT_VERSION,
            "contract_scope": "CONTRACTS.md section 4 only",
        },
        "run": {
            "target": config.target,
            "seed": config.seed,
            "clock_ms": config.clock_ms,
            "suites": list(config.suites),
            "token_budget": config.token_budget,
            "latency_budget_ms": config.latency_budget_ms,
        },
        "notes": [],
    }
    notes: list[str] = report["notes"]
    transcript: list[dict[str, Any]] = []
    protocol_errors: list[dict[str, str]] = []

    all_records = []
    attributions: list[tuple[str, str, str]] = []
    questions: dict[str, str] = {}

    # -- benchmarks -----------------------------------------------------------------
    if "benchmark" in config.suites:
        scored: dict[str, Any] = {}
        for name, corpus in corpora.items():
            client = Client(make(corpus))
            try:
                result = benchmark.run_benchmark(
                    client,
                    corpus,
                    token_budget=config.token_budget,
                    latency_budget_ms=config.latency_budget_ms,
                )
            except ProtocolError as exc:
                protocol_errors.append(exc.as_dict())
                continue
            finally:
                for exchange in client.transcript:
                    transcript.extend(exchange.as_frames())
                client.close()

            grader = ContainmentGrader()
            cost = result.cost
            assert cost is not None
            # The corpus variant travels with the number, not in a footnote. LongMemEval-S
            # results differ ~0.5-2 pp between the original and `cleaned` variants, so a
            # score whose variant is unstated is not comparable to anyone's -- including a
            # later run of our own. Emitted structurally so it cannot be forgotten.
            spec = fetch.MANIFEST.get(name)
            scored[name] = {
                "corpus_variant": {
                    "variant": spec.version if spec else "unknown",
                    "sha256_pinned": bool(spec and spec.sha256),
                    "note": (
                        "LongMemEval-S variants differ by roughly 0.5-2 pp; a published "
                        "number that does not state its variant is not comparable to this "
                        "one"
                    )
                    if name.startswith("longmemeval")
                    else "",
                },
                "answer_accuracy": accuracy(result.records, grader, cost).as_dict(),
                "abstention_accuracy": abstention_accuracy(result.records, cost).as_dict(),
                "evidence_precision": evidence_precision(result.records).as_dict(),
                "effective_trust_of_injected": summarize_trust(result.records),
                "run": result.as_dict(),
            }
            all_records.extend(result.records)
            questions.update({c.query_id: c.question for c in corpus.cases})
            for session in corpus.sessions:
                for turn in session.turns:
                    for mid in result.attributor.memories_of(turn.turn_id):
                        attributions.append((session.session_id, mid, turn.text))

        report["benchmarks"] = scored
        report["benchmark_coverage"] = reporting.coverage(scored)
    else:
        report["benchmark_coverage"] = reporting.coverage([])

    # -- probes ---------------------------------------------------------------------
    # A target that breaks section 4 breaks the probes too. Record the violation and carry
    # on: the run should report which suites could not execute, not die on the first one.
    if "probes" in config.suites:
        probes: dict[str, Any] = {}
        try:
            clock_result = clock.run(make)
            probes["clock_conformance"] = clock_result.as_dict()
            if not clock_result.passed:
                notes.append(
                    f"clock probe verdict: {clock_result.verdict}. Section 4.5 is binding, "
                    "and every decay-dependent number here is suspect until it passes."
                )
        except ProtocolError as exc:
            protocol_errors.append(exc.as_dict())
            probes["clock_conformance"] = {"probe": "clock_conformance", "verdict": "not_run"}
        try:
            curve, staleness_detail = staleness.run(make, pairs=config.staleness_pairs)
            probes["staleness"] = {**curve.as_dict(), "detail": staleness_detail}
        except ProtocolError as exc:
            protocol_errors.append(exc.as_dict())
            probes["staleness"] = {"metric": "staleness_half_life", "measured": False,
                                   "note": "not run: the target violated section 4"}
        report["probes"] = probes

    # -- poisoning ------------------------------------------------------------------
    if "poisoning" in config.suites:
        try:
            report["poisoning"] = poisoning.run(make, cases=config.poisoning_cases)
            failed = report["poisoning"]["trust_assertions"]["failed"]
            if failed:
                notes.append(
                    f"{failed} laundering assertion(s) failed: content that entered through "
                    "an untrusted origin was reported at a higher effective trust."
                )
        except ProtocolError as exc:
            protocol_errors.append(exc.as_dict())
            report["poisoning"] = {"note": "not run: the target violated section 4"}

    # -- labels and the headline ------------------------------------------------------
    if "labels" in config.suites and all_records:
        plan = draw(
            all_records,
            seed=config.seed,
            decoy_pool=decoy_pool_from(attributions),
            questions=questions,
        )
        label_block: dict[str, Any] = {"sample": plan.as_dict()}

        if config.labels is None:
            label_block["human_labels"] = "absent"
            label_block["injection_precision_human"] = None
            notes.append(
                "NO HUMAN LABEL SET WAS SUPPLIED, so injection precision -- the K1 headline "
                "-- was not measured. `evidence_precision` in this report is precision "
                "against benchmark gold evidence and is NOT that metric. ROADMAP.md: the "
                "human-judged relevance labels are the human's deliverable, and injection "
                "precision may not be validated against agent-generated labels."
            )
        else:
            label_block["human_labels"] = {
                "label_set_id": config.labels.label_set_id,
                "labelled": len(config.labels),
                "coverage_warnings": list(config.labels.coverage_warnings(list(plan.draws))),
            }

        if config.judge is not None:
            verdicts = [config.judge.judge(p) for p in plan.packets]
            if config.labels is None:
                label_block["judge"] = (
                    "verdicts produced but NOT reported: HP1 permits a judge number only "
                    "alongside its agreement against the human label set, and there is no "
                    "label set here"
                )
            else:
                try:
                    label_block["judge"] = judged_precision(
                        verdicts,
                        config.labels,
                        list(plan.draws),
                        judge_id=config.judge.judge_id,
                    ).as_dict()
                except NoOverlap as exc:
                    label_block["judge"] = {"error": str(exc)}

        report["labels"] = label_block

    report["protocol"] = {"errors": protocol_errors}
    return RunResult(report=report, transcript=transcript)


def write_artifacts(result: RunResult, out_dir: Path) -> dict[str, Path]:
    """report.json, run.jsonl, and the hash the reproduction check compares."""
    out_dir = Path(out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)

    report_path = out_dir / "report.json"
    report_path.write_text(canonical.pretty(result.report), encoding="utf-8")

    # run.jsonl is written in proposed section 4.0.3 frame shape, so a stub run and a real
    # M0b run produce diffable transcripts in the same format.
    transcript_path = out_dir / "run.jsonl"
    transcript_path.write_text(
        "".join(canonical.dumps(frame) + "\n" for frame in result.transcript),
        encoding="utf-8",
    )

    hash_path = out_dir / "core.sha256"
    hash_path.write_text(result.core_hash + "\n", encoding="utf-8")

    return {"report": report_path, "transcript": transcript_path, "hash": hash_path}
