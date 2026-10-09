# The persona artifact

**`v2.md` is the shipped persona** (M2 C2f). `v1.md` is retained as the superseded version — it was
pinned from Addendum C §C10 verbatim. The artifact is loaded with `include_str!` and placed in the
stable tier of the system prompt (§C6).

**A `vN.md` contains nothing but the persona text.** Not a header, not a version comment, not a note
like this one — every byte of it is sent to the model. Anything explanatory belongs in this file.

## v2 — adopted from a prior harness, translated rather than copied

v2 is a working prompt from a previous harness that performed well against this model class. It was
**not** copied in. Four kinds of change were made, and the first is the one that matters:

### Sections cut, because they described tools that do not exist

A prompt that promises a capability the harness lacks is the same defect as `bash`'s description
claiming a *"persistent shell session"* when `spawn_shell` runs a fresh `cmd /C` per call — except
authored fresh, with nothing to catch it. The model reads it, acts on it, fails, and blames itself.
M2 C2e watched exactly that happen: asked to showcase its tools, the model produced a **fabricated
shell transcript** echoing our own false description back.

| Cut | Why |
|---|---|
| `<vision>` | Screen and webcam tools. None exist. The section instructed the model to *"never say I can't see"* — a lie it would have had to construct on demand. |
| `<redteam_routing>` | `redteam.nmap`, `redteam.wsl_exec`, `engagement.activate`. None exist. |
| `<git>` | No git tool. `bash` exists, and the section forbade using it for git. |
| `<knowledge_and_search>`, search half | Web **search** does not exist and is not in this session's scope. Reduced to the training-cutoff rule, which is true. |
| `<memory_and_continuity>`, most of it | `profiles`, `profile.get`, `skill_read`, `marlowe_stack` do not exist. `recall` is registered with **no executor** and is not exposed. Rewritten as `<memory>`, which says only what `remember` can actually do. |

### `<tool_use>` — inverted, because the harness does the opposite

The source prompt's first non-negotiable rule was *"Emit multiple independent tool calls in a single
response."* `OllamaDriver::parse_step` does `calls.first()` and **silently discards the rest**. That
is worse than a missing tool: a missing tool returns an error the model can read, and a dropped call
simply never happens.

**CLOSED (M2 C2f).** `ModelStep::ToolCall` now carries a `Vec<ToolInvocation>`, the loop executes
every call in it, and each result is attributed to its call by a harness-assigned id — so a batch
of three where one fails tells the model *which* one. `<tool_use>` reverts to permitting
parallelism **in the same commit as the loop change**, which was the ordering constraint: the
prompt must never lead the harness.

The interim state is worth keeping in the record. Between the two, the adapter returned a
**non-retriable `ProviderError`** naming the count and refusing the whole batch. That was worse
than the eventual behaviour and better than the original: refusing to run a third of a plan beats
running a third of it and reporting nothing.

### `<untrusted_content>` — kept, and it is redundancy rather than the mechanism

The section survives nearly intact and it is worth having. **But it is not what stops an injection,
and it must never be read as such.**

Marlowe enforces this structurally. A tool result carries an origin-bound `TrustClass` assigned by
the executor (§2.8), `Run::latch_trust_floor` drops the run's floor monotonically to the worst class
ever in view, and `marlowe_permission::blocks_composed_targets` refuses every model-composed Target
from that point on (ADR-023). The model cannot opt out of that by being persuaded, because the model
is not consulted — invariant 3.

So the prompt text is a second line that costs nothing: it makes the model *report* the injection
attempt rather than merely failing to act on it. If the two ever disagree, **the latch is right.**

One correction of fact in the translation: the source prompt described untrusted results arriving
wrapped in an `<untrusted source="...">` envelope. **Marlowe has no such envelope.** `Block::tool_result`
carries the trust class out of band, in the block, not in the text. v2 describes what actually
happens instead of teaching the model to look for a delimiter that will never arrive.

### `<consent>` — rewritten against the real approval path

The source named specific tools (`outlook.send_email`, `fs.delete`, `http.download`, `plan.propose`),
none of which exist. v2 states the rule by *consequence* — irreversible, spending, reaching outside
the workspace — which is how `ConsequenceLevel` and the adjudicator actually decide, and which stays
true as tools are added.

The shape of the advice is unchanged and is correct: the harness gates, so the model calls the tool
rather than asking permission in prose.

### What was preserved from v1 that the source prompt did not have

v2 is not a replacement of §C10 by something with less in it. Three requirements had no counterpart
in the source and are carried across intact:

- **§C2's five drop conditions**, in full (`<dropping_the_register>`). The source covered distress
  only. §C7 makes drop-condition compliance a **100%, single-failure-is-blocking** criterion.
- **§C3 cold-start honesty** — *"You have no history with this person until you do."* The source
  prompt pulled the other way, treating recall of "three conversations ago" as identity. §C7 scores
  this at 100%.
- **The emoji and exclamation-mark absolutes**, which §C1 makes unconditional *including on request*,
  because a single exception makes it a variable.

### The operator is not named in the artifact, deliberately

The source prompt names its operator throughout. v2 does not, and this was already decided:
`daemon.rs`'s `IDENTITY_FACTS` is kept separate from `PERSONA` so that *"the artifact stays
deployment-independent: a persona that named the workspace would not be the same artifact across two
runs."* The same argument covers a person's name. Operator facts belong in the identity block beside
the persona, not inside it.

### OPEN, for the human: the artifact no longer names a namesake, and the two candidates differ

§C1 derives the whole register from **Chandler's** Marlowe — the detective: *"observant, laconic,
unimpressed by status, dry to the point of deadpan, and loyal without being servile."* The source
prompt derives it from **Christopher Marlowe**, the poet who *"looked into the abyss of forbidden
knowledge and didn't flinch."*

Those are different dispositions. The detective is restraint; the poet is transgression. v2 states
neither, because §C8 forbids backstory outright — *"no name for the persona beyond Marlowe, no
backstory, no simulated inner life"* — and the behavioural sections both readings agree on
(unimpressed, unflinching, says the unwelcome thing) are stated directly instead.

**This is flagged rather than resolved.** If the poet is the intended anchor, §C1 needs rewriting,
not v2.

### Cost, measured

| | v1 | v2 |
|---|---|---|
| lines | 27 | 230 |
| words | 226 | 1,845 |
| bytes | 1,229 | 10,632 |
| tokens (assembler's 3-chars estimator) | ~409 | **~3,544** |
| share of the 30,720-token effective window | 1.3% | **11.5%** |

**The stable tier is not trimmable** (`SourceKind::trimmable`), so this is never truncated to fit —
it displaces everything else first, and §6's compaction trigger at 70% fill arrives correspondingly
earlier. §C0 budgeted *"roughly twenty lines"*; v2 is an order of magnitude past that, which is a
deliberate trade and is recorded so a later session investigating early compaction finds the cause
named rather than having to derive it.

## Why an artifact and not a `const`

§C6: *"**Versioned as an artifact**, `persona/vN.md`, with changes reviewed as diffs. **Not a string
in the code** and not a user setting."*

Until M2 C2d it *was* a string in the code — a 40-word `const IDENTITY` in `daemon.rs` whose doc
comment claimed it "carries the persona (Addendum C)". It carried no part of Addendum C. The
comment was the only thing asserting otherwise, and nothing tested it.

## The check that matters is emission, not loading

**Loading the file proves the file loaded. It does not prove the persona is applied.** The standing
test asserts the persona text reaches the **outbound provider request** — the same distinction as
`get_providers()` reporting *registered* providers versus where nodes actually ran, which this
project has already paid for once (M0c Session L).

`marlowe-provider/tests/persona_emission.rs` builds the real request body and asserts a marker
phrase is present in the `system` role. A persona that loads and never reaches the wire fails it.

## DEFERRED: §C2's drop conditions are still not enforced structurally in v2

§C2 requires the persona to drop entirely — to plain, warm, unadorned prose — in five cases:
distress, safety-relevant refusal, reporting its own error, third-party-visible output, and
factual reporting under uncertainty.

The addendum is explicit about the mechanism:

> **These conditions are a permission gate, not a prompt instruction.** A persona that drops
> because the model judged the moment correctly will fail exactly when it matters.

**v1 stated the drop conditions as prompt text, which is the weaker form. v2 states them more
fully — and it is the same weaker form.** `<dropping_the_register>` asks the model to notice; nothing
in the harness suppresses the persona block when those conditions fire. A longer, better-written
instruction is still an instruction, and this is worth being explicit about because v2's added
detail could easily be mistaken for added enforcement.

**What closing it requires:** the wellbeing path §C2 names, so the distress condition can suppress
the stable-tier persona block structurally rather than asking the model to notice. Third-party
output (§A13/§A3) can be gated earlier — it is a known context, not a judgment — and is the
cheapest of the five to make structural.

**Milestone: M6**, with the trust ledger and the wellbeing path. Until then, §C2 is a prompt
instruction and its failure mode is exactly the one the addendum predicts.

## UNBUILT REQUIREMENT: a persona version bump must invalidate the prefix cache

§C6 puts the persona in the **stable tier**, which is the cached prefix. So bumping `v1.md` → `v2.md`
— or editing an artifact in place — **must invalidate the cache key**, or a stale system prompt
survives the change with nothing reporting it.

This is brief §6's compaction/cache-invalidation failure in a new place, and it is cheaper as a note
today than as a bug later: the symptom would be a persona change that appears to do nothing, on some
sessions but not others, with no error anywhere.

**The v1 → v2 bump has now happened and cost nothing, because there is still no prefix cache.** That
is luck, not design. When one lands, its key must include a digest of this artifact.

**There is a live analogue that already bit this project, and it is not hypothetical.** A running
daemon serves the binary it was started with, so a persona change is invisible until the daemon is
restarted — M2 C2e lost two turns to exactly that, with the persona correctly wired, the test green,
and the deployed process serving pre-persona code. `marlowe-daemon/src/staleness.rs` exists for this.
**After changing an artifact here, verify against the running process, not the source.**

## Do not touch

`persona/` is on brief §13's do-not-touch list and is guarded by
`.claude/hooks/protect-boundaries.py`. A change here needs a `DECISIONS.md` entry and a human who
read the reason. Bounded self-improvement does not extend to Marlowe editing who it is (§C6).
