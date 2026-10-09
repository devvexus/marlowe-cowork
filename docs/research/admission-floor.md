# Admission floor and band (study, v1)

Artifact: `crates/marlowe-memory/artifacts/admission-floor-v1.json`. Code: `tools/admission_study.py`.
This is a measurement. No product code or shipped numerics changed.

## Question

The shipped gate admits `ADMIT_TOP_K = 3` when the rank-1/rank-2 rerank margin is at least
1.165071 (`precision-coverage-heldout-v1.json`). On a small or young store this goes wrong in two
ways. With one candidate there is no margin, so the gate abstains. With a few unrelated facts, the
margin is huge, so all of the top 3 are admitted. Two candidate rules were measured:

- **Floor:** admit rank 1 when its absolute logit is at least `t`, with target precision 0.90.
- **Band:** admit rank k (k = 2, 3) when `logit_k >= logit_1 - d`, with target precision 0.80.

## Method

- **Slates.** The study ran the shipped pipeline (`--eval-adapter`: lexical and dense cues,
  frozen-v5 gate, pruning, cross-encoder at depth 10) through `target/debug/marlowe.exe`, driven
  by `score_longmemeval.score_one` without changes. It used a private copy of the embedding cache
  and lifted the harness deadline. Neither can move a logit.
- **Check against the shipped path.** The fit dump reproduces the published fit R@1 exactly:
  0.7555, n = 229. R@3 is 0.8996.
- **Population.** All 251 fit cases were used, with no subsample. Of these, 236 are answerable.
  A case whose gold turn never reaches the slate counts as "rank 1 not gold". That covers 25
  cases: 7 have no gold anywhere in the dump and 18 have gold that is not in the top 10.
- **Selection rules.** These are read literally from the brief and chosen on fit only.
  - `t` is the lowest cut whose cumulative precision is at least 0.90.
  - `d` is the widest band whose pooled cumulative precision is at least 0.80.
  - Each cut sits midway to the next observed value.
  - CIs are exact Clopper-Pearson 95%.
- **Held-out check.** After freezing, the rules were read once on the first 40 held-out ids in
  sorted order (34 answerable).

```
python tools/admission_study.py collect --split fit                    # ~51 min, idle priority
python tools/admission_study.py collect --split heldout --max-cases 40 # ~6 min
python tools/admission_study.py analyze
```

## Results (fit, n = 236)

| | value | precision | 95% CI | coverage / admissions |
|---|---|---|---|---|
| Floor `t` as specified | -5.0551 | 1/1 = 1.00 | [0.025, 1.00] | 0.42% |
| Best floor, ≥ 20 admitted | -6.2094 | 23/26 = 0.885 | [0.70, 0.98] | 11.0% |
| Best floor, ≥ 30 admitted | -6.7022 | 59/67 = 0.881 | [0.78, 0.95] | 28.4% |
| Shipped margin gate (≥ 1.165) | n/a | 36/38 = 0.947 | [0.82, 0.99] | 16.1% |
| Band `d` | 0.086 | 17/21 = 0.81 | [0.58, 0.95] | 21 rank-2 admissions, 0 rank-3, 6 queries rescued |
| Band, given that the floor fired | 0 | n/a | n/a | 2 events, none admitted |

**Floor.** On this corpus the absolute rank-1 logit never reaches 0.90 with real support. Its
precision is flat at about 0.81–0.88 across most of the range. The only qualifying cut admits one
case, so the artifact marks it `floor_status: target_not_attained_with_support`.

**Rank-1 discrimination.**
- Rank-1 gold vs non-gold, AUC: logit1 0.70, margin 0.67.
- Gold rank-1 logit, p10/p50/p90: -7.60, -6.99, -6.18. Non-gold: -8.35, -7.42, -6.33.
- The 13 abstention cases with a slate have rank-1 logits between -8.54 and -6.51, inside the gold
  range. The frozen floor admits none of them.

**Candidate level.** Across every slate member, the logit separates gold from non-gold much
better: AUC 0.93.

**Band.** The band only covers near-ties (gaps of 0.086 or less) between rank 1 and rank 2. Beyond
that, precision falls quickly: 0.72 at d = 0.1, 0.49 at d = 0.25 and 0.39 at d = 1.0.

## Held-out check (40 cases, 34 answerable; chose nothing)

- **Floor:** 0 admitted, coverage 0. No abstention case was admitted.
- **Band:** 0 of 4 admissions were gold (precision 0.00, CI [0, 0.60]). The fit precision of 0.81
  did not hold on this slice. With n = 4 that is weak evidence, but it is a warning, not a
  confirmation.

## Caveats

- **LongMemEval negatives are harder than a personal store's.** Each query competes with about
  490 long, topically close turns, and gold turns are long, noisy user messages. That pushes every
  logit into roughly -9 to -5. A small personal store of short, unrelated facts should yield
  higher logits for a true match and lower ones for unrelated facts. So a floor fitted here is
  **conservative there**: it admits too little, not too much. The flip side is that this study
  cannot say where the floor should sit on a small store. The candidate-level AUC of 0.93 suggests
  the logit carries the needed signal, but it was not measured on that distribution.
- **The frozen floor is effectively "almost never" on this corpus.** It is licensed only as an
  upper-conservative cut. Using it to fix the single-candidate case would admit a lone fact only
  when it scores above -5.06. That level was reached by 1 of 236 fit queries here. It is
  unmeasured on short facts.
- **Band and margin gate.** Any `d` below 1.165 cannot co-fire with the shipped margin gate. When
  the gate fires, logit2 is already more than 1.165 below logit1, so a band rule would turn top-3
  into top-1 on those queries. The band generalised poorly on the held-out slice.
- **Wrong threshold in the brief.** The brief quoted a margin threshold of 6.229. The shipped
  artifact declares 1.165071, and this study used the artifact's value.
- **Population caveat.** The 0.90 and 0.80 targets are conditional on answerable LongMemEval
  questions, one query per haystack. As the operating-point docstring warns, the precision does
  not transfer to live use, where "no relevant memory" is common.
- **Not measured: logits on short facts in a small store.** The next measurement that would
  license a small-store floor is a small set of short-fact (fact, question) pairs, both matched
  and unmatched, scored by the pinned graph. Alternatively, use a temp copy of the real profile.
