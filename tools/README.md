# tools/: LongMemEval reproduction

These scripts drive `marlowe --eval-adapter` (`crates/marlowe/src/eval/`) through the `eval/`
harness over the §4.0 NDJSON transport. The pipeline is the shipped one and runs on CPU only:
lexical and dense cues, the frozen gate `frozen-v5`, session pruning, the cross-encoder rerank at
depth 10, the declared operating point, and frozen consolidation. `eval/` is not modified.

| File | Role |
|---|---|
| `score_longmemeval.py` | The scorer, ported from the old harness with its reporting rules intact: held-out is the headline, all-500 carries its contamination label, and diagnostics are marked driver-side. |
| `recall_at_k.py` | Computes R@1, R@3, R@5 and R@10 for one pass directory, from the binary's own dump and the shipped ranking key. |
| `warm_embedding_cache.py` | Fills `.embedding-cache/` first, so that no cold ingest hits the harness's 30 s deadline. |
| `admission_study.py` | Rank-1 floor / rank-2..3 band study on the fit split, through a debug binary. Writes `crates/marlowe-memory/artifacts/admission-floor-v1.json`; see `docs/research/admission-floor.md`. |
| `split.json` | The pre-registered fit/held-out split, digest `3a6857…`. The gate artifact checks against it. |
| `preregistration-session-f.json` | The bands and power floor the full two-pass summary reads. |

## One-time setup

```
powershell -NoProfile -File scripts/cargo-idle.ps1 build --release --package marlowe -j 4
```

The corpus is LongMemEval-S, cleaned variant. It is git-ignored and pinned by sha256 in
`eval/src/marlowe_eval/datasets/fetch.py`. That fetcher has no pinned URL, so it refuses to
download and names the destination. Put the maintainer's file there and verify it with the
package's own checker:

```
curl -L -o data/longmemeval_s_cleaned.json https://huggingface.co/datasets/xiaowu0162/longmemeval-cleaned/resolve/main/longmemeval_s_cleaned.json
PYTHONPATH=eval/src python -m marlowe_eval.cli verify-corpus --dataset longmemeval-s --path data/longmemeval_s_cleaned.json
```

The checker must print `"verdict": "ok"` and sha256 `d6f21ea9…a3c442`. The models come from
`models/` by default, or from `$MARLOWE_MODELS` if it is set.

## The full held-out run (n = 229 scored for R@k)

```
python tools/warm_embedding_cache.py --heldout-only --reranking models/ms-marco-MiniLM-L-2-v2-ft-session-j
python tools/score_longmemeval.py   --heldout-only --reranking models/ms-marco-MiniLM-L-2-v2-ft-session-j
```

The first command is optional, but skipping it is risky. On a cold cache, one ingest embeds a
whole haystack of about 493 turns. With 8 workers that took about 14 s per case, and with
1 worker it went past the 30 s deadline. Warming the cache cannot move a quality number, because
vectors are content-addressed by text and model digests. It does make the reported P95 latency a
warm reading.

The scorer prints the result and writes it to `data/runs/longmemeval/heldout/recall.json`.
**Expected headline: R@1 0.6725, R@5 0.8865, R@10 0.9039, n = 229** (R@3 0.8515). The n
counts the 249 held-out cases, minus the abstention cases, minus any case whose gold turn never
reaches the dump. This is the rule every published read used. `recall_at_k.py` reproduces these
four numbers exactly from the old harness's `runs/session-k/heldout` dump.

Drop `--heldout-only` for the two-pass run, held-out plus all-500. It also writes `summary.json`
with the pre-registered Numbers 1, 2, 2b and 3, the power floor and the budget conditions. It
takes about three times as long.

## The GPU cascade (M4.5, `--features cuda`)

Check the GPU is idle first (no compute-only process, util < 10%). Build into its own target dir,
fetch the pinned runtime and the five extra graphs, then score:

```
powershell -NoProfile -File scripts/fetch-gpu-runtime.ps1
powershell -NoProfile -File scripts/fetch-models.ps1 -Cascade
powershell -NoProfile -File scripts/cargo-idle.ps1 build --release -p marlowe --features cuda --target-dir target/cuda -j 4
python tools/score_longmemeval.py --heldout-only --reranking models/ms-marco-MiniLM-L-2-v2-ft-session-j --binary target/cuda/release/marlowe.exe --rerank-provider cuda --rerank-plan cascade --gpu-runtime gpu-runtime --out data/runs/longmemeval-m45-cascade-cuda
```

Read on 2026-10-09 (RTX 4080 SUPER): cascade R@1 0.6769, R@3 0.8908, R@5 0.9170, R@10 0.9563,
p95 50 ms; `--rerank-plan shipped` on CUDA gives exactly the CPU headline at p95 16 ms. The
cascade on CPU ranks identically but reads p95 3,514 ms, so the product only takes it on a GPU.

## Checking a small slice

`--max-cases N` (it requires `--heldout-only`) scores only the first N held-out cases. Any R@k
from a slice is labelled SUBSET and is not comparable to the headline.

Verified on 2026-10-09 with an 8-case slice (6 scored for R@k). Each case's gold rank, candidate
set and rerank logits were identical to the published dump.

## Known deviation (memory owner, not this stage)

The published `session-k` dump embedded passages untruncated. Today's `MAX_SEQ_LEN` is 1024, and
a Python ORT probe shows the old cosines match full-length embedding. About 0.18% of turns run
past 1024 tokens. For those turns `dense_cosine` moves (by up to 0.015 on the slice), so
`dense_z`, `score` and sometimes `winning_cue` move for the other candidates of the same query.
On the slice no gold rank changed. A full run is what shows whether R@1 still reads exactly
0.6725.

# tools/: M4.5 research evals

| File | Role |
|---|---|
| `extraction_eval.py` | Fetches WCXB and the ScrapingHub benchmark into `data/extraction/` at pinned commits, drives the Rust harness `crates/marlowe-extract/examples/extract_eval.rs`, and scores token P/R/F1 per page type, WCXB with/without snippets, ScrapingHub shingle F1, link recall (anchor text, and URLs kept inline in the shipped path's markdown), thin/empty rate and p50/p95 ms. It prints the pass-bar verdict for switching the default extractor. |
| `research_eval.py` | FACT-style citation accuracy (each cited sentence is judged against the reference's stored passage) and key-point coverage for research reports. Its input is a bundle (`marlowe.research-bundle/1`; the bundle writer was the retired `deep_research` playbook, so bundles now come from whatever produces them). The judge is local Ollama; verdicts are cached by prompt digest. |
| `research_questions.json` | The 10-question set (4 ML research, 3 engineering, 3 general) with the key points a reviewer wrote. |
| `research_eval_fixtures/` | Hand-made bundles on q01 whose evidence is the verbatim arXiv abstracts. `good` has 11 supported citations. `bad` is the same report with one planted unsupported citation. `subtle` has three planted errors: a wrong number, a reversed direction and a misattributed source. |
| `test_research_eval.py` | Offline tests for both scorers, using a fake judge. |

```
python tools/extraction_eval.py fetch                         # once; ~300 MB, git-ignored
python tools/extraction_eval.py run --label <name>            # builds the harness (cargo-idle, -j 2), runs, scores
python -m pytest tools/test_research_eval.py -q               # offline
python tools/research_eval.py validate                        # live judge on the fixtures (GPU; checks it is free)
python tools/research_eval.py run --bundles <dir> --label <name> [--produce "<cmd with {qid} {question} {bundle}>"]
```

Results are written to `data/runs/extraction-eval/<label>/` and `data/runs/research-eval/<label>/`.
The summary is in `docs/research/deep-research-v2.md` (the playbook it describes is retired).
