# marlowe-eval — M0a

A benchmark harness that scores **any** memory implementation behind the interface pinned in
`docs/design/CONTRACTS.md` §4. Built before and without the retriever it will score: the
measurement cannot be authored by the thing being measured.

Contains no retriever, no storage, no gate, and no embedding model — enforced by a test, not
by intent.

**Read `METHODOLOGY.md` before trusting any number this produces.** It states what each
metric licenses you to say and, in §12, what the harness cannot tell you.

---

## Install

```bash
cd eval
pip install -e .            # add '[dev]' for pytest, '[judge]' for the API judge
```

`pytest` also works from a plain clone with no install — `conftest.py` puts `src/` on the
path so a third party's first command succeeds.

---

## Quick start

```bash
marlowe-eval targets                                  # what you can point it at
marlowe-eval run --target stub://oracle --out runs/a  # score the reference stub
marlowe-eval conformance --target stub://oracle       # does it obey section 4?
marlowe-eval repro --runs 2                           # is it reproducible?
```

---

## Every acceptance criterion, as a command

| ROADMAP.md acceptance | Command | Result |
|---|---|---|
| Scores the reference stub end to end | `marlowe-eval run --target stub://oracle --out runs/a` | full report + `run.jsonl` |
| **Rejects a response missing `cost` — on all three interfaces** | `marlowe-eval conformance --target stub://broken.missing_cost` | 3 findings (ingest, retrieve, answer), exit 1 |
| **Rejects `answered: false` with a populated `answer`** | `marlowe-eval conformance --target stub://broken.hedged_abstention` | `hedged_abstention`, exit 1 |
| **Fixed seed and clock reproduces bit-identically** | `marlowe-eval repro --runs 2` | two identical sha256, exit 0 |
| **Asserts derived trust, not declared trust** | `marlowe-eval run --target stub://broken.trust_launderer --suite poisoning` | laundering assertions fail, naming `web → user_asserted` |
| Judge agreement is published | `marlowe-eval labels agreement --labels L --verdicts V --draws D` | κ, raw agreement, per-decile |
| Blinded sample for human judging | `marlowe-eval labels draw --out packets.json` | packets with no score, no decile, no injected flag |
| Corpus integrity | `marlowe-eval verify-corpus --dataset longmemeval-s --path <file>` | structural check; fails loudly on drift |
| Third-party reproducible | `marlowe-eval schema-export --out schemas` | JSON Schema for all three interfaces |
| The whole suite | `pytest` | 72 passing |

Two commands are worth running for the answer rather than the exit code:

```bash
# The clock probe has teeth: this one FAILS, and should.
marlowe-eval conformance --target stub://broken.clock_reader

# The instrument recovers a known input: set the stub's precision knob, read it back.
pytest -k recovers
```

---

## Targets

```
stub://oracle                      reference implementation; reads the answer key
stub://oracle?precision=0.7        with knobs (precision, abstention_rate,
                                   answer_accuracy, maturation_ms, k, seed)
stub://broken.<name>               a conformance fixture; see `marlowe-eval targets`
exec://<cmd> --profile-root {profile_root}
                                   a real implementation, spawned over the §4.0 transport
```

`exec://` takes a **command line**, not a URL — a URL cannot carry one without mangling
Windows paths — and it must contain the literal token `{profile_root}`, which is replaced
with a freshly created empty directory on **every** spawn:

```bash
marlowe-eval conformance \
  --target "exec://./target/release/marlowe --eval-adapter --profile-root {profile_root}"
```

The placeholder is required rather than optional. The harness spawns a target once per
corpus and four more times in the clock probe alone, and each spawn must start from empty
state — the probe compares a run at epoch against the same run shifted ten years, so state
surviving between them would silently corrupt the comparison. A missing placeholder is an
error at target-construction time, not a default that quietly shares one directory.

`run.jsonl` is written in §4.0.3 frame shape for every target, so a stub run and a real run
produce diffable transcripts.

---

## Layout

```
src/marlowe_eval/
  contract/     section 4 as executable schema. Imports nothing else — enforced by a test
  adapter/      MemorySystem ABC + the validating Client
  datasets/     LongMemEval / LoCoMo adapters, fixtures, checksummed fetch, verify-corpus
  metrics/      precision (three of them), accuracy, cost, staleness, security
  suites/       benchmark, conformance, clock probe, staleness sweep, poisoning
  judge/        offline judge (HP1 tier 2), cache, agreement — gated
  labels/       blinded stratified sampler; the human label-set schema
  runner.py canonical.py determinism.py cli.py
src/marlowe_eval_stubs/
  oracle.py     the reference stub. A sibling package, not a submodule: the measurement
                must not contain the thing measured, and the import graph should show it
  broken.py     one deliberately non-conforming fixture per acceptance criterion
```

---

## Three things that will surprise you

**1. There is no field called `injection_precision`.** There are three differently-named
precisions — human-judged (the K1 headline), judge-derived, and evidence-based — so nobody
can fill the headline in with whichever proxy they happened to have. A run without human
labels reports `null` and says so in words rather than falling back.

**2. Several types refuse to be constructed.** An accuracy figure without its cost, an
attack-success rate without utility retention, a judge number without its agreement against
the human set — none of these can be built, so the corresponding rules survive someone who
has not read §5.7 or HP1.

**3. The reference stub is not a retriever and cannot become one.** Its selection function
takes `(query_id, rng)` and never receives the query text, so there is nowhere for a
similarity search to live. A test asserts the signature.

---

## What is not verified

- **LongMemEval-S is verified** against the real release (500 q / 246,750 turns, 2026-08-02),
  `cleaned` variant — the maintainer's own published replacement. **Variants differ ~0.5–2 pp,
  so our number is not comparable to a published one that doesn't state its variant, and many
  don't.** Ours always states `cleaned`. **LoCoMo is not verified** — its adapter is still
  written from the published schema only, and the fixtures cannot catch a misreading because
  the same hand wrote both.
- **LongMemEval-S penalises correct clock handling on 76 of 500 cases** — questions dated
  before their own history, 43 with gold evidence postdating the question. Reproduced
  faithfully, not corrected. The headline covers all 500; a temporally-clean subset is
  reported beside it. See METHODOLOGY.md §11 before comparing our number to anyone's.
- **The stub's latencies are synthetic.** A stub run's P95 measures nothing.
- **No human label set exists yet**, so the headline metric has never been produced.
