"""`marlowe-eval` -- the command line.

Every M0a acceptance criterion is a subcommand here. ("A target that is not a command
printing a number does not exist.")

    run             score a target end to end and write the full report
    conformance     probe section 4 on all three interfaces; show what was rejected
    repro           run twice at a fixed seed and clock; compare the deterministic core
    labels draw     draw the blinded, stratified sample a human judges from
    labels agreement    kappa and raw agreement against a human label set
    verify-corpus   validate a real downloaded corpus against the adapter's expectations
    schema-export   emit JSON Schema for the three section 4 interfaces
    targets         list the built-in stub targets
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path
from typing import Any

from . import canonical
from .contract import schema_export
from .datasets import fixture_locomo, fixture_longmemeval, synthetic, verify
from .datasets.model import Corpus
from .judge.agreement import compute_agreement
from .judge.scripted import ScriptedJudge
from .labels import decoy_pool_from, draw, write_packets
from .labels.schema import HumanLabelSet
from .runner import RunConfig, run, write_artifacts
from .suites import conformance

CORPORA = {
    "fixture-longmemeval": fixture_longmemeval,
    "fixture-locomo": fixture_locomo,
    "synthetic": lambda: synthetic.build(),
}


def _load_corpora(names: list[str]) -> dict[str, Corpus]:
    out: dict[str, Corpus] = {}
    for name in names:
        if name not in CORPORA:
            raise SystemExit(f"unknown corpus {name!r}; known: {sorted(CORPORA)}")
        corpus = CORPORA[name]()
        out[
            "longmemeval-s" if name == "fixture-longmemeval" else
            "locomo" if name == "fixture-locomo" else name
        ] = corpus
    return out


def _make_factory(target: str):
    from marlowe_eval_stubs import build_target

    def make(corpus: Corpus):
        return build_target(target, corpus)

    return make


# ---------------------------------------------------------------------------------------


def cmd_run(args: argparse.Namespace) -> int:
    corpora = _load_corpora(args.corpus)
    make = _make_factory(args.target)
    labels = HumanLabelSet.load(Path(args.labels)) if args.labels else None

    judge = None
    if args.judge == "scripted":
        judge = ScriptedJudge()
    elif args.judge == "anthropic":
        from .judge import build_anthropic_judge
        from .judge.cache import VerdictCache

        judge = build_anthropic_judge(
            allow_api=True,
            label_set=labels,
            cache=VerdictCache(Path(args.judge_cache) if args.judge_cache else None),
        )

    result = run(
        make,
        corpora,
        RunConfig(
            target=args.target,
            seed=args.seed,
            clock_ms=args.clock,
            suites=tuple(args.suite),
            labels=labels,
            judge=judge,
        ),
    )

    if args.out:
        paths = write_artifacts(result, Path(args.out))
        print(f"report:     {paths['report']}")
        print(f"transcript: {paths['transcript']}")
    else:
        print(canonical.pretty(result.report), end="")

    print(f"core sha256: {result.core_hash}")
    for note in result.report.get("notes", []):
        print(f"\nNOTE: {note}", file=sys.stderr)
    return 1 if result.report["protocol"]["errors"] else 0


def cmd_conformance(args: argparse.Namespace) -> int:
    corpus = fixture_longmemeval()
    report = conformance.run(_make_factory(args.target), corpus, target=args.target)
    print(canonical.pretty(report.as_dict()), end="")
    if report.conforms:
        print("\nCONFORMS: no section 4 violations, clock probe passed.")
        return 0
    print(
        f"\nREJECTED: {len(report.findings)} finding(s); clock probe {report.clock_verdict}."
    )
    return 1


def cmd_repro(args: argparse.Namespace) -> int:
    corpora = _load_corpora(args.corpus)
    make = _make_factory(args.target)
    hashes = []
    for _ in range(args.runs):
        result = run(
            make,
            corpora,
            RunConfig(target=args.target, seed=args.seed, clock_ms=args.clock,
                      suites=tuple(args.suite)),
        )
        hashes.append(result.core_hash)
        print(result.core_hash)

    identical = len(set(hashes)) == 1
    print(
        f"\n{args.runs} runs at seed={args.seed} clock={args.clock}: "
        f"{'IDENTICAL' if identical else 'DIVERGED'}"
    )
    if identical:
        print(
            "Timing is excluded from this hash by an explicit allowlist "
            f"({', '.join(sorted(canonical.TIMING_ALLOWLIST))}); everything else must match "
            "byte for byte. There are no tolerance windows."
        )
    return 0 if identical else 1


def cmd_labels_draw(args: argparse.Namespace) -> int:
    corpora = _load_corpora(args.corpus)
    make = _make_factory(args.target)
    result = run(
        make,
        corpora,
        RunConfig(target=args.target, seed=args.seed, clock_ms=args.clock,
                  suites=("benchmark", "labels")),
    )
    plan = result.report.get("labels", {}).get("sample")
    if plan is None:
        print("no sample drawn (no benchmark records)", file=sys.stderr)
        return 1

    # Re-draw to obtain the packet objects themselves; the report holds only the summary,
    # deliberately -- packets are the blinded artifact and belong in their own file.
    from .suites import benchmark as bench

    records, attributions, questions = [], [], {}
    for corpus in corpora.values():
        from .adapter.base import Client

        client = Client(make(corpus))
        run_result = bench.run_benchmark(client, corpus)
        records.extend(run_result.records)
        questions.update({c.query_id: c.question for c in corpus.cases})
        for session in corpus.sessions:
            for turn in session.turns:
                for mid in run_result.attributor.memories_of(turn.turn_id):
                    attributions.append((session.session_id, mid, turn.text))
        client.close()

    redrawn = draw(
        records,
        seed=args.seed,
        decoy_pool=decoy_pool_from(attributions),
        questions=questions,
    )
    out = Path(args.out)
    write_packets(list(redrawn.packets), out)
    print(f"wrote {len(redrawn.packets)} blinded packets to {out}")
    print(canonical.pretty(redrawn.as_dict()), end="")
    return 0


def cmd_labels_agreement(args: argparse.Namespace) -> int:
    labels = HumanLabelSet.load(Path(args.labels))
    verdicts_raw = json.loads(Path(args.verdicts).read_text(encoding="utf-8"))
    from .judge.protocol import Verdict

    verdicts = [
        Verdict(
            packet_id=str(v["packet_id"]),
            relevant=bool(v["relevant"]),
            judge_id=str(v.get("judge_id", "unknown")),
        )
        for v in verdicts_raw["verdicts"]
    ]
    draws_raw = json.loads(Path(args.draws).read_text(encoding="utf-8"))
    from .labels.schema import SampleDraw

    draws = [
        SampleDraw(
            packet_id=str(d["packet_id"]),
            query_id=str(d["query_id"]),
            memory_id=str(d["memory_id"]),
            category=str(d["category"]),
            score=float(d["score"]),
            calibrated_precision=float(d["calibrated_precision"]),
            decile=str(d["decile"]),
            was_injected=bool(d["was_injected"]),
        )
        for d in draws_raw["draws"]
    ]
    agreement = compute_agreement(verdicts, labels, draws)
    print(canonical.pretty(agreement.as_dict()), end="")
    return 0


def cmd_verify_corpus(args: argparse.Namespace) -> int:
    try:
        result = verify.verify(args.dataset, Path(args.path), strict_counts=not args.lenient)
    except verify.VerificationFailed as exc:
        print(str(exc), file=sys.stderr)
        return 1
    print(canonical.pretty(result), end="")
    return 0


def cmd_schema_export(args: argparse.Namespace) -> int:
    written = schema_export.write(Path(args.out))
    for path in written:
        print(path)
    return 0


def cmd_targets(_: argparse.Namespace) -> int:
    from marlowe_eval_stubs import describe_targets

    for spec, what in sorted(describe_targets().items()):
        print(f"{spec:42s} {what}")
    return 0


# ---------------------------------------------------------------------------------------


def build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(prog="marlowe-eval", description=__doc__)
    sub = p.add_subparsers(dest="command", required=True)

    def add_common(sp: Any) -> None:
        sp.add_argument("--target", default="stub://oracle")
        sp.add_argument("--seed", type=int, default=7)
        sp.add_argument("--clock", type=int, default=1_780_000_000_000)
        sp.add_argument(
            "--corpus", action="append", default=None,
            choices=sorted(CORPORA), help="repeatable; defaults to the LongMemEval fixture",
        )

    run_p = sub.add_parser("run", help="score a target end to end")
    add_common(run_p)
    run_p.add_argument(
        "--suite", action="append", default=None,
        choices=["benchmark", "probes", "poisoning", "labels"],
    )
    run_p.add_argument("--labels", help="path to a human label set")
    run_p.add_argument("--judge", choices=["scripted", "anthropic"], default=None)
    run_p.add_argument("--judge-cache", default=None)
    run_p.add_argument("--out", help="directory for report.json and run.jsonl")
    run_p.set_defaults(func=cmd_run)

    conf_p = sub.add_parser("conformance", help="probe section 4 on all three interfaces")
    conf_p.add_argument("--target", default="stub://oracle")
    conf_p.set_defaults(func=cmd_conformance)

    repro_p = sub.add_parser("repro", help="verify bit-identical reproduction")
    add_common(repro_p)
    repro_p.add_argument("--runs", type=int, default=2)
    repro_p.add_argument(
        "--suite", action="append", default=None,
        choices=["benchmark", "probes", "poisoning", "labels"],
    )
    repro_p.set_defaults(func=cmd_repro)

    labels_p = sub.add_parser("labels", help="the human label-set workflow")
    labels_sub = labels_p.add_subparsers(dest="labels_command", required=True)

    draw_p = labels_sub.add_parser("draw", help="draw a blinded, stratified sample")
    add_common(draw_p)
    draw_p.add_argument("--out", default="label-packets.json")
    draw_p.set_defaults(func=cmd_labels_draw)

    agree_p = labels_sub.add_parser("agreement", help="judge vs human agreement")
    agree_p.add_argument("--labels", required=True)
    agree_p.add_argument("--verdicts", required=True)
    agree_p.add_argument("--draws", required=True)
    agree_p.set_defaults(func=cmd_labels_agreement)

    verify_p = sub.add_parser("verify-corpus", help="validate a real downloaded corpus")
    verify_p.add_argument("--dataset", required=True, choices=sorted(verify.LOADERS))
    verify_p.add_argument("--path", required=True)
    verify_p.add_argument("--lenient", action="store_true", help="do not enforce counts")
    verify_p.set_defaults(func=cmd_verify_corpus)

    schema_p = sub.add_parser("schema-export", help="emit JSON Schema for section 4")
    schema_p.add_argument("--out", default="schemas")
    schema_p.set_defaults(func=cmd_schema_export)

    targets_p = sub.add_parser("targets", help="list built-in stub targets")
    targets_p.set_defaults(func=cmd_targets)

    return p


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    if getattr(args, "corpus", None) is None:
        args.corpus = ["fixture-longmemeval"]
    if getattr(args, "suite", None) is None:
        args.suite = ["benchmark", "probes", "poisoning", "labels"]
    return int(args.func(args))


if __name__ == "__main__":  # pragma: no cover
    raise SystemExit(main())
