# M0a — Methodology

What this harness measures, how, and what each number does and does not license you to say.
Written to be executable by someone who has never spoken to us: every claim below
corresponds to a command in `README.md` and a test in `tests/`.

---

## 0. The one thing to understand first

**The measurement was built before the thing it measures, deliberately.** M0a exists as a
separate milestone so the scorer is written without knowledge of the retriever — the same
reason the storage spike's gates were pre-committed. If you are reading this to evaluate
Marlowe's memory numbers, the relevant question is not "is the harness good?" but "could
the harness have been shaped by the results?" The answers we can offer are: it was written
first, it depends on a contract pinned in advance (`CONTRACTS.md` §4), and it contains no
retrieval code — enforced by a test, not a promise.

---

## 1. The boundary

The harness talks to an implementation through three interfaces and nothing else:

| § | Interface | What it is used for |
|---|---|---|
| 4.6 | `ingest` | Load a benchmark history; plant the poisoning suite's attacks |
| 4.1–4.4 | `retrieve` | Injection precision, tokens, latency |
| 4.7 | `answer` | Accuracy and abstention |
| 4.5 | `clock` | Staleness half-life, and reproducibility of anything decay-dependent |

`src/marlowe_eval/contract/` is a binding of the pinned JSON, and a test asserts it imports
nothing else from the harness. If a fourth interface were ever needed, the fix belongs in
`CONTRACTS.md`, not here.

**Transport.** §4.0 pins the wire: newline-delimited JSON over a spawned process's stdio,
strictly serial, positional correlation, `body` XOR `error`. The subprocess adapter is the
one component still unwritten (M0b has nothing to talk to yet); today the harness drives an
in-process `MemorySystem`. `run.jsonl` is written in §4.0.3 frame shape regardless, so a
stub run and a real run produce diffable transcripts.

The reasoning behind each §4.0 choice — stdio over a unix socket, NDJSON over
length-prefixing, why `error` lives in the envelope — is recorded in
`docs/design/pinned-4.0-transport-record.md`.

---

## 2. What is enforced rather than requested

Four rules that other harnesses state as conventions are types here. Each has a test named
after the acceptance criterion it satisfies.

| Rule | Where it lives | Consequence of violation |
|---|---|---|
| Every response carries `cost` | `contract/` — required field | `ProtocolError`, on all three interfaces |
| `answered: false` with a populated `answer` | `contract/validate.py` | `ProtocolError` |
| A tombstone never appears in `injected` | `contract/validate.py` | `ProtocolError` |
| Every accuracy figure ships with its cost | `metrics/cost.py` — `Scored` requires a `CostSummary` | Unconstructible without it |
| ASR never reported without utility retention | `metrics/security.py` — `PoisoningResult` | Unconstructible without it |
| A judge number never reported without agreement | `judge/agreement.py` — `JudgedPrecision` | Constructor raises |

A **protocol error** is not a score. It means the response is unscoreable and the run fails
loudly. A **budget miss** — exceeding 7,000 retrieval tokens or 300 ms — is the opposite: a
scored result the eval exists to produce, never a reason a run cannot report.

---

## 3. The three precisions, and why they are named differently

There is no field called `injection_precision` anywhere in this harness. A test enforces
that. Three separately named numbers exist instead, because the headline is the one that is
hardest to produce and would otherwise be quietly filled in with whichever proxy was handy:

| Name | Source | Standing |
|---|---|---|
| `injection_precision_human` | Human relevance labels | **The K1 headline.** ≥0.95 is the project kill criterion |
| `injection_precision_judge` | Offline LLM judge (HP1 tier 2) | Tracking between human label refreshes only; unreportable without its agreement rate |
| `evidence_precision` | The benchmark's own gold-evidence key | A legitimate proxy and a good regression signal. **Not the headline** |

`ROADMAP.md` is explicit: *"Injection precision may not be validated against
agent-generated relevance labels."* A run with no human label set reports
`injection_precision_human: null` and prints a note saying in words that K1's metric was not
measured. It does not fall back.

### The attribution join, and `unattributable`

Injected memories are matched back to benchmark turns using the `written: [{turn_id,
memory_ids}]` mapping from the §4.6 ingest response — not by parsing the memory id, which a
real implementation will make opaque. A memory absent from that mapping (a consolidated or
merged belief) is counted as **unattributable** and excluded from the precision numerator
and denominator alike. Scoring it as a false positive would penalise consolidation for
existing; hiding it would overstate confidence. It gets its own line.

---

## 4. The human label set — the human's deliverable

Nothing in this repository writes relevance labels. The harness draws the sample, blinds
it, and computes agreement; a person supplies the judgments.

| Property | Requirement |
|---|---|
| Size | ≥400 judged injections |
| Per category | ≥50 |
| Stratification | By category **and** gate-score decile, so the calibration curve has support across its range and not just the confident head |
| Blinding | The judge sees query + memory, never the score and never whether it was injected |
| Refresh | Re-drawn whenever the cue set or router changes materially |

Every one of these is reported as a `coverage_warning` when unmet, attached to the number
it qualifies rather than to a footnote.

### How blinding is actually enforced

Three leaks are possible; a lesser design closes one and calls it done.

1. **The score.** `SampleDraw` carries it (the sampler stratifies on it); `LabelPacket` —
   the type that is serialized for a human — has no score field at all. Two types with a
   serialization boundary between them, not one type with a flag.
2. **Position.** Stratify and emit in draw order and the decile is readable off the row
   number. The draw is shuffled under the run seed, and packet ids are assigned **after**
   the shuffle so the id encodes nothing either.
3. **Whether it was injected.** Packets mix genuinely injected memories with **decoys** —
   memories from the same session that were not injected for that query — so presence in
   the file carries no information about what the system did. Decoys are excluded from the
   precision computation and labelled decoys tell you the base rate of relevance among
   non-injected memories, which is worth knowing on its own.

`pytest -k blinding` writes a real packet file and reads the bytes back.

---

## 5. The offline judge (HP1 tier 2)

Permitted **only** for gate training signal and for tracking between human label refreshes,
and only where its agreement against the human set is published alongside anything it
produces. Two constructor-level refusals implement that:

- `AnthropicJudge` requires **both** `--judge anthropic` and
  `MARLOWE_EVAL_ALLOW_API_JUDGE=1`. A flag alone is easy to leave in a script; an
  environment variable alone is easy to leave in a shell.
- It also requires a loaded human label set. Without labels there is nothing to compute
  agreement against, so there is no legitimate output — and the object refuses to exist.
- `JudgedPrecision` cannot be constructed without an `Agreement`, and `Agreement` refuses
  an empty overlap rather than reporting 0/0 as 1.0.

**Determinism, honestly.** Sampling parameters are rejected on the judge model, so a judge
call cannot be pinned to a fixed decode. Structured output and low effort narrow the
variance; they do not remove it. **The verdict cache is what makes a judged run
reproducible**, which is why it is required rather than optional. The cache key covers the
prompt version, so changing the prompt invalidates every entry instead of silently mixing
protocol versions.

---

## 6. Answer grading, and what it is not comparable to

The default grader (`containment-v1`) is deterministic: it asks what fraction of the gold
answer's content words appear in the response, and passes at 0.6.

**This does not produce numbers comparable to published LongMemEval results**, which are
graded by an LLM judge. It is a regression signal. Every accuracy figure in the report
carries the grader's name and a `comparable_to_published: false` flag, so the caveat travels
with the number instead of living here.

---

## 7. The clock probe, and why it is decisive

§4.5 forbids reading a system clock on any path reachable from §§4.1, 4.6, 4.7. You cannot
prove the absence of a clock read from outside a process — but you can make a system that
does one behave observably differently. Two tests:

- **A. Translation invariance.** Run an identical scenario twice with every supplied
  timestamp shifted by ten years. Only relative time changed nothing, so a conforming
  implementation is identical. A clock reader computes ages as `wall_now − occurred_at`,
  which moves by the shift.
- **B. Time dependence.** Ingest, then query immediately and again a year later on the
  synthetic clock. A conforming implementation differs, because §4.3 exclusion (3) withholds
  unmatured beliefs. A clock reader sees one wall instant for both and returns the same
  thing.

Test B is what makes the probe decisive rather than suggestive. An implementation with no
observable time dependence **fails** — and deserves to: §4.3 calls maturation the cheapest
available defence against single-exposure poisoning and warns that dropping it *"silently
removes that defence while still passing every latency and precision test."* This probe is
the test it does not pass.

The probe is shown to discriminate: `stub://oracle` passes, `stub://broken.clock_reader`
fails. A probe no implementation can fail is not a probe.

---

## 8. Staleness half-life

Write fact A at T₁, its replacement A′ at T₂, then sweep queries forward from T₂ on the
synthetic clock and watch how long A keeps coming back. Half-life is where the retrieval
rate crosses half its initial value, by **linear interpolation between the bracketing sweep
points** — no curve family is assumed, because fitting an exponential to a dozen points and
quoting its parameter would report a decay model the data does not establish.

If A never falls to half within the horizon, the report says *not observed within N days*.
It does not extrapolate. HP4 calls silent staleness genuinely unsolved and names this
measurement as the resolving experiment, so a fabricated number here would hide the thing
the metric was added to expose.

Supersession pairs carry **both** contradicting text and a `base#vN` turn-id convention, so
the probe works against a real implementation (which resolves on content) and against an
oracle stub (which may not compare text) without either needing the other's mechanism.

---

## 9. Poisoning

Reported AgentDojo-style: **ASR and utility retention, always together**. `PoisoningResult`
cannot be constructed with ASR alone, because a system that abstains on everything scores
0% ASR and is worthless. The clean baseline is reported beside the ratio, so a high
retention over a broken baseline is visible rather than flattering.

| Family | What it measures |
|---|---|
| `laundering` | Does a `channel: "web"` claim come back `untrusted_content`? §4.6 has the harness declare **origin and never trust**, so this measures the implementation's own derivation |
| `minja` | Does an instruction planted in untrusted content get retrieved for a later benign query? |
| `memorygraft` | The same, with a payload that attaches to a legitimate topic rather than announcing itself |
| `delayed_trigger` | The same plant fired across a delay sweep (0h → 720h). §4.3 warns the attack is *temporally decoupled from its trigger*, so a single trigger time measures almost nothing |
| `unsigned_write` | A turn claiming a reserved actor. Must come back in `rejected` — visible refusal, not refusal inferred from absence. K3 sets this at 0% |

The laundering suite is shown to discriminate: `stub://oracle` records zero failures,
`stub://broken.trust_launderer` records failures naming the exact upgrade
(`web` → `user_asserted`). That matters because laundering is invisible to every other
metric — precision, latency and token cost are all unaffected by relabelling trust.

---

## 10. Reproducibility

A run at a fixed seed and clock reproduces **bit-identically over the deterministic core**:
injected sets, ids, scores, answers, verdicts, token counts, trust classes, abstentions.

**Timing is excluded by a declared allowlist of exactly one key** (`latency_ms`), because
latency is self-reported by the implementation and varies by nature. It is recorded,
reported and scored — it is simply not hashed. There are **no tolerance windows anywhere**:
if a field outside that allowlist differs between two runs at the same seed and clock, that
is a failure, not a wobble.

Supporting mechanisms:

- All randomness comes from `stream(seed, phase)`, derived per phase, so adding a suite does
  not perturb the draws of existing ones.
- The harness reads **no system clock at all** — enforced by an AST scan over the package.
  A harness that read a wall clock could not credibly verify that an implementation does not.
- The report is canonical JSON: sorted keys, no incidental whitespace.

---

## 11. Benchmark coverage, and the failure mode it guards

Every benchmark named in `ROADMAP.md` appears in every report, scored or carrying the reason
it was not run. The failure mode is not lying; it is **omission** — a benchmark quietly
absent because nobody had the corpus that week reads identically to one that was never meant
to be there.

**LongMemEval-S: verified against the real release, 2026-08-02.** `marlowe-eval
verify-corpus` was run against `longmemeval_s_cleaned.json`
(sha256 `d6f21ea9…78c3a442`) and passed: **500 questions, 500 sessions, 246,750 turns, 30
abstention**, all seven categories populated, no dangling gold-evidence ids, no answerable
question missing evidence. Dates parse to a 2021-05 → 2024-02 range with no epoch-zero
fallbacks, and spot-checked gold turns contain the answer text.

**We run the `cleaned` variant, and that fact travels with every number we publish.** It is
the maintainer's own published replacement — `xiaowu0162/longmemeval` carries a deprecation
notice pointing to it, and the change removes noisy history sessions that interfered with
answer correctness. Same author, upgrade not fork.

**This makes most cross-vendor comparison invalid by default.** Results differ by roughly
**0.5–2 pp** between the original and cleaned variants. A published LongMemEval-S number is
therefore not comparable to ours unless its variant is known — and **many vendor reports do
not state which they ran.** A 1 pp gap between two systems means nothing if one was measured
on each variant; it is inside the variant delta.

The rule this repo follows: **our LongMemEval-S number always states `cleaned`.** Not in a
footnote — beside the number. A benchmark result whose variant is unstated is not a result
anyone can use, and §5.7's *"vendor-reported memory numbers in this space are not comparable
across systems; ours must be reproducible with a published harness"* is exactly this problem.

**LoCoMo remains unverified** — no real download has been checked, so its adapter is still
in the state described below.

**Why this check exists at all:** the committed fixtures were authored from the published
schema descriptions by the same hand that wrote the adapters, so a green fixture suite proves
self-consistency and nothing more. `verify-corpus` validates required fields, types, category
coverage and exact counts against a real download, and fails loudly on drift.

### The LongMemEval-S temporal limitation

First contact with the real release surfaced something the fixtures could not: **76 of 500
questions are dated before content in their own haystack**, by up to 0.99 days — and **43 of
those have gold evidence postdating the question.** Concentrated in `temporal-reasoning` (54),
`knowledge-update` (15) and `abstention` (7).

This is a property of the released corpus, not a parse error. The adapter **reproduces the
source faithfully and does not correct it**; `verify-corpus` reports it as a statistic rather
than failing, because failing would mean rejecting the real data.

**The consequence, stated plainly, because it inverts the thing being measured.** The harness
ingests a history and then asks at the question's timestamp. For an affected case the
implementation is holding memories dated *after* the query clock. **A system that honours the
clock is therefore penalised relative to one that ignores it** — it may correctly withhold
evidence that the grader expects it to have used. Clock-dependent scoring over those cases
measures something other than what it names.

**Anyone comparing our LongMemEval-S number to a vendor's needs this context.** A system with
no temporal reasoning at all is not disadvantaged here; one that implements §4.5 correctly is.

**So two numbers are reported.** The headline covers all 500 cases and stays the headline,
because that is what a published LongMemEval-S number means and comparability is the point.
Beside it, `answer_accuracy.detail.temporally_clean_subset` reports accuracy over the 424
unaffected cases. That is a **diagnostic, not a competing headline**: a large gap between the
two is a signal about clock handling rather than about recall, and if M0b ever scores well on
500 and badly on 424, that is worth knowing early.

Corpora are never vendored. `datasets/fetch.py` holds a manifest with pinned versions and
sha256 digests and **fails on mismatch rather than warning** — including the case where no
digest is pinned yet, which it treats as unverifiable rather than acceptable.

---

## 12. What this harness cannot tell you

Stated because a methodology that lists only its strengths is not a methodology.

- **It cannot prove an implementation never reads a system clock** — only that one behaving
  as though it does fails a probe (§7).
- **It cannot produce the K1 headline on its own.** That requires human labels, by design.
- **Its default answer grading is not comparable to published LongMemEval numbers** (§6).
- **Its LoCoMo adapter is unverified against a real release** (§11). LongMemEval-S is verified.
- **LongMemEval-S penalises correct clock handling on 76 of 500 cases** (§11), so the headline
  number understates a system that implements §4.5 properly. Read it with the
  temporally-clean subset beside it.
- **The reference stub's latencies are synthetic**, so a stub run's P95 is not a
  measurement of anything. It has no clock to time itself with, and inventing a wall-clock
  reading is the one thing §4.5 forbids.
- **`evidence_precision` is not injection precision**, however convenient the substitution
  would be.
