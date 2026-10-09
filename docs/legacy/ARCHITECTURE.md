# Marlowe — Architecture

**Status:** Design. Settled unless `DECISIONS.md` says otherwise.
**Read before touching any subsystem.** Contracts live in `CONTRACTS.md` and are pinned.

---

## 1. The core abstraction

> **Everything Marlowe knows, is doing, or has done is a materialized view over one
> append-only, provenance-signed event log — and the agent loop is a transaction that reads
> a view, acts, and appends.**

That is the whole system. Every other statement in this document is a consequence.

It is what makes "memory is the spine, not a subsystem" (brief §1) structural rather than
rhetorical. Memory is not a store the loop calls; the log *is* the substrate, and memory,
context, runs, sessions, and the audit trail are different views over it:

| What you would call a subsystem | What it actually is |
|---|---|
| Episodic memory | A fidelity-tiered index over the log |
| Semantic memory | A derived belief store, rebuildable from the log |
| The durable-run WAL (invariant 6) | The log |
| The audit trail (invariant 7) | The log, read directly |
| A session | A partition of the log |
| Compaction lineage (§6) | Two appends and a new view |
| The trust ledger (§A8) | An aggregate view over typed events |
| Self-improvement (§13) | Views over the agent's own past events |

Because these are the same substrate, the invariants do not each need their own mechanism.
Invariant 7 (reconstructable) is not a feature — replay is the log's primary read path.
Invariant 1 (nothing lost on the boundary) is not a discipline — append-before-discard is the
only eviction path that exists.

### The one honest qualification

The accurate claim is **one append-only event log plus one evictable content store**, not "one
log, full stop." Two things write outside the append-only model and are named here rather than
absorbed:

- **Blob eviction.** Large tool results and fetched content are content-addressed and
  referenced by the log, never stored in it. They are evictable, leaving hash + typed summary
  + tombstone.
- **Privacy redaction.** Invariant 5's *delete* is physically destructive and audited. A
  logical tombstone is not a delete when the user asked for a delete.

### Availability vs. accessibility

Forgetting (§5.4) does not rewrite the log. The distinction it rests on:

- The log preserves **availability** — the record exists.
- Forgetting removes **accessibility** — it cannot be recalled, injected, or acted on.

Graduated fidelity (record → summary → gist → tombstone) is a property of the *retrieval
index*, not of storage. This is the reason the single-journal design holds; it is not a
convenient reframing. See `DECISIONS.md` ADR-003, where it is recorded with the measured curve
that makes it viable.

---

## 2. Components

Every component below lists what it **owns**, what it **must never touch**, and what
**crosses** its boundary. Boundaries marked ⚑ are enforced by PreToolUse hooks and are in the
brief §13 do-not-touch set.

```
                            ┌──────────────────────────────┐
   surfaces ───────────────►│         THE LOOP             │
   TUI · CLI · gateway      │  one loop, many capability   │
   voice · triggers         │  profiles                    │
                            └──────┬────────────────┬──────┘
                                   │ requests       │ reads
                                   ▼                ▼
                    ⚑┌─────────────────────┐  ┌──────────────────┐
                     │ PERMISSION LAYER    │  │ CONTEXT ASSEMBLER│
                     │ consequence, (action│  │ budget per source│
                     │ ,target), approvals │  │ compaction @70%  │
                     │ egress, spend caps  │  └────────┬─────────┘
                     └──────────┬──────────┘           │
                                │ adjudicated          │ materializes
                                ▼                      ▼
   ┌────────────────┐   ⚑┌──────────────┐      ┌──────────────────┐
   │ RUN CONTROL    │───►│   JOURNAL    │◄─────│    RETRIEVAL     │
   │ PLANE          │    │ append-only  │      │ router · 5 cues  │
   │ lifecycle,     │    │ typed·signed │      │ fusion · GATE    │
   │ checkpoints,   │    └──────┬───────┘      └────────▲─────────┘
   │ steer, orphans │           │ derives               │ reads
   └────────────────┘           ▼                       │
                         ┌──────────────┐      ┌────────┴─────────┐
   ┌────────────────┐    │ BELIEF STORE │─────►│    INDEX SET     │
   │ CONSOLIDATION  │───►│ facts,people,│      │ live-only hot ix │
   │ (a run, not a  │    │ commitments, │      │ + cold/all ix    │
   │  second loop)  │    │ procedures   │      │ + graph, temporal│
   └────────────────┘    └──────────────┘      └──────────────────┘

   ┌──────────────────┐  ┌──────────────────┐  ⚑┌─────────────────┐
   │ CONTENT STORE    │  │ TOOL/SKILL       │   │ TRUST LEDGER    │
   │ addressed blobs, │  │ REGISTRY         │   │ action classes, │
   │ eviction, keys   │  │ registration ≠   │   │ tiers, evidence │
   └──────────────────┘  │ exposure         │   └─────────────────┘
                         └──────────────────┘
   ┌──────────────────┐
   │ CONNECTION BROKER│  credentials never reach the model
   └──────────────────┘
```

### 2.1 Journal ⚑

**Owns** event identity (monotonic `seq`), typed event kinds, write-time origin binding,
signature, ordering, durability, and the replay path.

**Never touches** payload interpretation. The journal does not know what a memory means.

**Crosses:** `JournalEvent` in; ordered event streams out.

The single most important property: **the model never appends.** The model *requests*; the
harness validates, stamps origin / trust class / derivation / signature, and appends. There is
no unsigned write path, which is how §8.3's 0% ASR target on unsigned memory writes is met
structurally rather than by filtering.

**Replay is not model-reachable.** The replay path is operator and audit only. It is absent
from the tool registry at every exposure tier, and the journal lives outside the model's
filesystem scope so it cannot be reached through `read` either. Without this, forgetting is
cosmetic and §5.4's worst-failure clause — *a memory the user can no longer surface but the
system silently acted on* — is violated by construction.

### 2.2 Content store

**Owns** blob identity (content hash), eviction policy, and per-subject encryption keys.

**Never touches** the journal's ordering or the belief store's semantics.

**Crosses:** `ContentRef` (hash + typed summary + size + trust class) in both directions. The
blob bytes themselves cross only into a tool's transport layer, never into the loop's context
unless explicitly dereferenced.

This store does double duty: it is the reference target that keeps untrusted bytes out of
attention (§2.8), *and* it is the eviction unit that keeps the log small.

### 2.3 Belief store

**Owns** current beliefs: semantic facts, entities, relationship edges, commitments,
procedures, voice parameters. Mutable only by supersession.

**Never touches** the journal (it is *derived from* it), permission decisions, or the trust
ledger.

**Crosses:** `MemoryEntry` records with one universal provenance envelope and a closed set of
typed payloads.

Fully rebuildable from the log. That is what makes invariant 5 (user can see, edit, delete)
tractable — correcting a belief is an append plus a view refresh, not a destructive edit.

### 2.4 Index set

**Owns** retrieval structures and fidelity tiers.

**Never touches** authoritative state. Every index is a cache with a rebuild path.

**The hot index is live-only.** This is a **requirement, not an optimization** — see ADR-003
and the measured curve. Auto-injection retrieval reads an index containing only injectable
entries; tombstones and superseded entries never enter it. They live in a cold index reachable
by the explicit `recall` tool and by the abstention check.

**The index rebuilds to its current state, not to full fidelity.** Every fidelity transition,
supersession, and tombstoning is a typed journal event, the same treatment §5.3 already
requires for consolidation. A rebuild that restores full fidelity would resurrect forgotten
memories — a correctness failure, not a cache miss. Verified in the spike: exact per-entry
match, 0 mismatches.

### 2.5 Retrieval

**Owns** query-type routing, the five cues (dense, lexical, entity-graph, temporal, causal),
fusion, reranking, and **the injection gate**.

**Never touches** writes, permission decisions, or the trust ledger.

**Crosses:** `RetrievalRequest` in; `RetrievalResult` out, with token cost and latency attached
to the result object so §5.7's "report the pair" is structurally enforced rather than
remembered.

The gate is precision-tuned and **frozen in M0** — no online learning. Recall is recovered by
making the explicit `recall` tool excellent, per §5.5.

Proactive salience runs **outside the hot path**, pre-staging candidates continuously so that
the eleven-week callback does not have to be computed inside a 300 ms budget.

### 2.6 Consolidation

**Owns** belief derivation, duplicate merging, contradiction resolution, fidelity-demotion
proposals, and skill induction.

**Never touches** the permission layer, sandbox config, audit logging, memory provenance, or
the trust ledger's promotion logic (brief §13, hard boundary).

**Consolidation is a run on the one loop**, with a memory-only capability profile and a hard
step budget. It is not a second loop. It inherits checkpointing, budgets, audit, and
resumability for free, and it is itself an episodic event (§5.3).

### 2.7 Context assembler

**Owns** the token budget per source, prompt tiering (stable / context / volatile),
tool-result clearing, the compaction trigger, and cache invalidation.

**Never touches** security policy or memory semantics.

Three rules from §6 that are structural here, not advisory:

- **Compaction triggers at ~70% of the effective window**, never at exhaustion. A model already
  impaired by context rot writes a degraded summary.
- **Governance survives compaction.** Permissions, approvals, and user-asserted constraints
  live in the stable tier and are re-asserted post-compaction *structurally* — never left to a
  summarizer's judgment.
- **Compaction invalidates cache.** Tested explicitly; the documented failure mode is serving
  stale pre-compaction prefixes into post-compaction turns.

### 2.8 The loop

**Owns** turn execution. One loop. Research, voice, coding, automation, consolidation, and
quarantined reading are **capability profiles** — differing in tool exposure, budgets, and
interrupt policy — not variants.

**Never** appends to the journal directly, evaluates a permission decision, reads a credential,
or reaches the replay path.

**Two independent axes govern what a tool result does to the loop:**

| Axis | Driven by | Decides |
|---|---|---|
| inline vs. reference | size | whether bytes enter attention |
| trust class | origin | whether bytes may parameterize an action |

Conflating these is the trap. A workspace file read can inline *and* carry
`untrusted_content` — informing analysis while being barred from targeting a gated tool call.

**A tool result carrying the class that blocks composed targets does not enter the run's window at
all.** It is read by a quarantined child and the parent receives a validated summary
(ADR-039). Since ADR-041 the unit is the **group**, not the call:

```
model emits [web, web, web, edit, web]
        │
        ├─ group [web web web]   ← Inert: executed CONCURRENTLY (ADR-040)
        │        └─ ONE quarantined reader, 1 model call, 1 subagent
        ├─ group [edit]          ← mutating: alone, in position
        └─ group [web]
```

Grouping is by declared `ConsequenceLevel` over **maximal runs of consecutive** `Inert` calls, so a
mutating call never moves relative to anything around it. The justification for running a group
concurrently is *not* the note on `ModelStep::ToolCall` — that establishes no call was **shaped by**
another's output, which is a claim about data flow and says nothing about side-effect ordering.

Condensed documents are cached by content hash, so a repeated document costs no model call.

### 2.9 Permission and approval layer ⚑

**Owns** consequence evaluation, the `(action, target)` provenance check, risk-tiered
approvals, egress allowlisting, capability manifests, and spend / blast-radius caps.

**Never** consults the model. Enforced in code that runs whether or not the model agrees
(invariant 3).

The **`(action, target)` split**: untrusted-derived content may shape inert payload fields
only — never control flow, never targets (recipient, path, host, amount, identifier). The
dangerous thing is not the text; it is the pair.

**Default-deny on consequence.** A tool with no consequence declaration is treated as maximum
consequence. An undeclared or malformed manifest **fails at load time with an error**, not at
call time with a warning — the system must be unable to start with an unannotated tool.

### 2.10 Run control plane

**Owns** run identity, lifecycle, budgets, checkpoints, steering, orphan policy, and the
parent/child graph.

**Never touches** memory semantics or permission decisions.

Runs are daemon-owned with lifecycle independent of whatever started them (invariant 6).
Children outlive parents; orphan policy is declared at spawn, not inferred. Isolation is the
default topology — orchestrator-worker, never swarm.

### 2.11 Tool / skill registry

**Owns** manifests, signature verification, install-time diff review, and **exposure
selection** — a large registry presenting ≤13 model-visible tools per run.

> **Amended 2026-08-27, ≤12 → ≤13 (ADR-058).** Eleven exposed builtins plus two MCP slots.
> `write` was split out of `edit` and would otherwise have been paid for out of a user's MCP
> allowance, taking a server from two tools to one. The cap moved instead. It carries **no spare**:
> the next builtin raises it again, deliberately.

**Never** executes anything or holds a credential.

Skills are `SKILL.md` per the open Agent Skills standard, unmodified where the standard
specifies it. MCP is the tool transport. Neither is reinvented.

### 2.12 Trust ledger ⚑

**Owns** action classes, tiers, agreement evidence, promotion proposals, demotion.

**Never** promotes itself. Promotion is proposed to the user and granted only by the user;
the event kind that grants a tier is appendable only by the permission component. Because the
model cannot append to the journal at all, self-promotion is not merely forbidden — it is
unrepresentable.

### 2.13 Connection broker

**Owns** credentials, token refresh, just-in-time authorization checks.

**Never** exposes a token to the model. The agent holds a *connection ID*; credentials are
injected at the transport layer at call time.

### 2.14 Surface adapters

**Owns** rendering and input. TUI, classic CLI, messaging gateway, voice.

**Never** holds policy, and never holds state the daemon does not have. Surfaces are
projections; closing one does not affect a run.

**No TUI-only capabilities** (§B11). The classic CLI has 100% parity on commands, sessions, and
data. **Layout is allowed to differ** — Addendum B v2 amends v1's "no TUI-only features" to
"no TUI-only *capabilities*", because layout is exactly what a grid buys and the classic CLI is
the narrow, SSH, piped-stdin and no-TTY path rather than a lesser product.

---

## 3. The agent loop

One loop. Pseudocode below is the whole thing, including every interrupt, checkpoint,
compaction trigger, and failure path.

```python
def run(run_id):
    # ── resume: a run survives whatever started it (invariant 6) ──────────────
    ckpt   = journal.last_checkpoint(run_id) or Checkpoint.initial(run_id)
    budget = Budget.load(run_id)        # tokens, wall, tool_calls, subagents, depth, money
    profile= CapabilityProfile.load(run_id)   # tool exposure ≤12, interrupt policy, egress

    while True:
        # ── hard stops, checked before any model spend ───────────────────────
        if budget.exhausted():
            journal.append(RunPaused(run_id, reason=budget.which()))
            escalate(decision_package(run_id))      # §A7: never fail silently
            return
        if control.cancelled(run_id):
            journal.append(RunCancelled(run_id)); return

        # ── mid-flight steering, no restart (§10.1) ──────────────────────────
        if steer := control.take_steer(run_id):
            journal.append(SteerReceived(run_id, steer))
            ckpt = ckpt.with_guidance(steer)

        # ── assemble the view: this is the memory read ────────────────────────
        view = assemble(ckpt, budget, profile)
        #   1. stable tier    — identity, governance, user-asserted constraints
        #   2. context tier   — project files, loaded skills, exposed tool schemas
        #   3. volatile tier  — history, tool results, injected memories
        #
        #   injection: retrieval.gate(query) -> candidates the gate scored ≥ threshold.
        #   live-only hot index; tombstones excluded (they reach only `recall`/abstention).
        #   budget: ≤7,000 tokens, P95 ≤300 ms; degrade to lexical-only if the dense cue
        #   is unavailable, and mark the run degraded (invariant 4).

        # ── context pressure, checked before the call, never at exhaustion ────
        if view.fill_pct >= 0.70:
            # append BEFORE discard — invariant 1, not negotiable, not concurrent
            summary  = summarize(ckpt)                    # cheap model, tiered routing
            journal.append(SessionSummarized(run_id, summary))
            child_id = journal.append(SessionSpawned(parent=ckpt.session_id, seed=summary))
            #   ^ compaction produces LINEAGE, not a rewrite (§6). Only after both appends
            #     are durable may the parent's volatile state be dropped.
            cache.invalidate(ckpt.session_id)             # documented failure mode; tested
            ckpt = ckpt.rotate(child_id)
            reassert_governance(ckpt)                     # structural, not summarizer's job
            surface.notify_compacted(turns=ckpt.turns)    # one line, does not interrupt
            continue
        elif view.tool_result_pressure():
            # cheaper lever first (§6): mask older tool results to placeholders
            ckpt = ckpt.clear_tool_results(keep_last=N)
            continue

        # ── the model call, with failover ────────────────────────────────────
        try:
            step = model.call(view, tools=profile.exposed())
        except ProviderError as e:
            if provider.failover(run_id, e):              # run state preserved across
                journal.append(ProviderFailedOver(run_id, e)); continue
            journal.append(RunFailed(run_id, e)); escalate(...); return

        journal.append(ModelStep(run_id, step))           # every step, before acting

        # ── interrupts: user may cut in mid-turn ─────────────────────────────
        if interrupt := surface.take_interrupt(run_id):
            # §9 policy, in code, not a config knob:
            #   idempotent reads complete; mutations cancel on contradiction
            for call in in_flight():
                call.complete() if call.idempotent else call.cancel()
            journal.append(Interrupted(run_id, partial=step))   # partial output kept
            ckpt = ckpt.absorb(interrupt)                 # continue from the new premise
            continue

        match step:
            case Done(result):
                journal.append(RunCompleted(run_id, result))     # against output contract
                return result

            case Ask(question):
                journal.append(EscalationRaised(run_id, decision_package(question)))
                return  # resumes on answer; the run does not hold a channel open

            case ToolCall(tool, args):
                # ── the security boundary, outside the model's cooperation ────
                decision = permissions.adjudicate(
                    tool=registry.manifest(tool),   # load-time validated; default-deny
                    args=args,
                    provenance=taint.of(args),      # (action, target) split enforced here
                    ledger=trust_ledger.tier_for(action_class(tool, args)),
                    novelty=novelty.score(tool, args),   # unusual-for-class drops one tier
                )
                journal.append(PermissionDecided(run_id, decision))

                if decision.blocked:
                    ckpt = ckpt.with_tool_error(decision.reason); continue
                if decision.needs_approval:
                    journal.append(ApprovalRequested(run_id, decision.blast_radius))
                    if not surface.await_approval(decision):     # states blast radius,
                        ckpt = ckpt.with_tool_error("declined"); continue   # not the command
                    journal.append(ApprovalGranted(run_id, decision))

                result = sandbox.execute(tool, args, egress=profile.egress)
                ref    = content.put(result.bytes, trust=result.trust_class)
                journal.append(ToolCompleted(run_id, tool, ref, result.summary))

                # inline-vs-reference by SIZE; trust class by ORIGIN. Independent axes.
                ckpt = ckpt.with_tool_result(
                    summary=result.summary,          # the §B6 one-line contract, also the
                    body=(result.bytes if result.small else ref),   # model's default view
                )

            case MemoryWrite(claim):
                # `remember` is a REQUEST. The harness adjudicates and stamps. There is no
                # unsigned write path — invariant 2 depends on this.
                receipt = memory.adjudicate_and_append(run_id, claim)
                ckpt = ckpt.with_receipt(receipt)

            case Spawn(task, contract, orphan_policy):
                child = control.spawn(parent=run_id, task=task, contract=contract,
                                      orphan=orphan_policy,        # declared, never inferred
                                      budget=budget.slice_for(task))
                journal.append(RunSpawned(run_id, child))
                # worker gets a fresh window and a self-contained brief; it does not know
                # the other workers exist, and returns findings, not transcript

        # ── checkpoint every iteration: resume at the last completed step ────
        journal.append(Checkpointed(run_id, ckpt))
```

### Failure paths, collected

| Failure | Response |
|---|---|
| Budget exhausted (any dimension) | Pause, escalate with a decision package. Never spend past the line. |
| Provider error | Failover preserving run state; if none, fail loud with diagnosis. |
| Host reboot / gateway restart | Resume from last `Checkpointed` event. |
| Dense retrieval unavailable | Lexical-only path; run marked `degraded`; surfaced in one word. |
| Voice unavailable | Text. TTS unavailable → text. Never silent. |
| Tool manifest undeclared/malformed | **Load-time error.** The system does not start. |
| Approval declined | Tool error into context; the loop continues, it does not retry around it. |
| Non-converging retry | Bounded structurally by tool-call and step caps, not by hoping. |
| Unattended run fails | Notify with diagnosis and proposed fix (§11). |

---

## 4. How the seven invariants are structurally enforced

"The agent will be careful" is not a mechanism. Each row names the code path that makes the
invariant hold whether or not the model cooperates.

| # | Invariant | Mechanism |
|---|---|---|
| 1 | Nothing is lost on the boundary | Append-before-discard is the **only** eviction path. Compaction cannot begin until `SessionSummarized` and `SessionSpawned` are durable; the parent's volatile state is dropped after, never concurrently. Enforced by the assembler having no API that discards without a prior durable append. |
| 2 | Every belief has a birth certificate | **No unsigned write path exists.** The model cannot append; `remember` is a request the harness adjudicates and stamps with origin, timestamp, derivation, trust class, and signature. A memory without provenance is not rejected at read time — it is unrepresentable at write time. |
| 3 | The model is not the security boundary | The permission layer is a separate component the loop calls *before* execution; it reads manifests and taint, never model output. Default-deny on consequence, validated at load. The model has no path to the layer's inputs. |
| 4 | Degrade, never break | Every cue, provider, and surface is individually optional behind an interface with a declared fallback. Degradation sets a run flag that the surface renders (§B5 — in the status band, in amber, with the reason in the Status tab). Silent degradation is impossible because the flag is on the run object, not a log line. |
| 5 | See, edit, delete | The belief store is derived and rebuildable, so correction is an append. Deletion is destructive redaction with per-`(profile, person)` key destruction — a real delete, not a logical tombstone. |
| 6 | A running task survives its starter | Runs are owned by the daemon, not the client. The client is thin by construction: it holds no run state, so there is nothing for it to take down. Checkpoint-per-iteration + WAL gives resume from last completed step. |
| 7 | Every autonomous action is reconstructable | Replay is the log's **primary read path**, not an added feature. Every model step, tool call, permission decision, approval, and injected memory is a typed event under one trace ID. |

### Two further invariants, pinned here

| # | Invariant | Mechanism |
|---|---|---|
| 8 | Replay is not model-reachable | Absent from the tool registry at every exposure tier; the journal is outside the model's filesystem scope so `read` cannot reach it. Without this, forgetting is cosmetic. |
| 9 | The index rebuilds to current state, not full fidelity | Fidelity transitions, supersessions, and tombstonings are typed events; rebuild replays them. Verified: exact per-entry match, 0 mismatches. A rebuild that resurrects forgotten memories is a correctness failure. |

---

## 5. Simplicity budget audit (§4)

The budget is a hard constraint. Here is the accounting.

### Eight user-facing nouns

`session · memory · skill · tool · run · trigger · profile · provider`

**Seven until 2026-08-22.** `provider` is the eighth, added with `/provider` (ADR-049 §7) by an
explicit decision rather than by a command quietly claiming a neighbouring noun — the first
attempt mapped it onto **profile**, which `model` and `workspace` already claim, and that would
have held the count at seven by making one of the seven mean two things. Every other concept in
the brief and both addenda still maps onto one of them, and nothing earns a ninth:

| Concept | Noun | How |
|---|---|---|
| Commitment, Person, Relationship, voice params | **memory** | Typed payloads under the one provenance envelope. Real, not rhetorical: they decay, supersede, and are corrected by the same mechanism as any belief. |
| Connection / "app" | **tool** | A connection is what makes a tool callable. The user connects Gmail; the system exposes mail tools. |
| Action class, trust tier | **tool** + **run** | A class is a (tool, argument-shape) pair; the ledger is an aggregate view over run events. |
| Noticing, daily brief | **trigger** → **run** | The salience process is a scheduled trigger firing a run. Not a subsystem. |
| Subagent, deep research, consolidation | **run** | Capability profiles, not new kinds of thing. |
| Compaction lineage | **session** | A chain of sessions. |
| Which company serves the weights, and its catalogue | **provider** | The eighth. A profile *points at* a provider; it does not contain one, and the model list is a consequence of which one is active. |

### One loop

Research, voice, coding, automation, unattended triggers, quarantined reading, and
consolidation are all `CapabilityProfile` values over the loop in §3. The profile varies tool
exposure, budgets, egress, and interrupt policy. There is no second `while` loop in the design.

### Model-visible tools: 12 against a budget of 14

**This table was stale in three ways at once and is corrected here rather than annotated.** It
said 11 with a spare slot when `BUILTIN_TOOLS` holds 12 against `MAX_EXPOSED_TOOLS` = 14; it
still listed `done`, which was removed when a small model proved unable to emit a terminator
reliably; and its `find` row promised *"index-backed symbol lookup where available"*, an
operation that has never existed. ADR-034 §79 records that exact sentence as a description
promising something the executor cannot do, and STATE.md marks that family CLOSED — it was closed
in the MANIFEST and left standing here, which is what a design doc drifting from a shipped
artifact looks like. `BUILTIN_TOOLS` is the definition; this table is a map of it.

| Tool | Purpose |
|---|---|
| `bash` | One command line through bash (Git Bash on Windows) — the universal adapter (§7.2) |
| `read` | Read a file by path, or a fetched document by `ref` — returns a WINDOW |
| `write` | Create or overwrite a whole file |
| `edit` | Replace one exact snippet inside a file; `replacing` is required |
| `glob` | List files by NAME under a directory |
| `grep` | Search file CONTENTS with a regular expression (ADR-059; was `find`) |
| `web` | Fetch one URL — always returns `untrusted_content` |
| `recall` | Explicit memory search; where recall is recovered (§5.5) |
| `remember` | **A request**, adjudicated and stamped by the harness |
| `use` | Find and load a skill or tool — one discriminated return, not two slots |
| `run` | Spawn, steer, await a child run |
| `ask` | Escalate with a decision package |

Registration is unlimited; exposure is budgeted. Twenty connected apps still present ≤14
(§A2.3) because connectors register and are found by `use`, not front-loaded.

### Zero-config first run

No configuration file is read on first run. Every knob has a defensible default. The daemon
auto-spawns on first client invocation.

---

## 6. Process model

One binary, two roles.

- **`marlowe`** — thin client. Holds no run state. Renders. Auto-spawns the daemon if absent.
- **`marlowe --serve`** — the daemon. Owns the journal, belief store, indexes, runs, triggers,
  gateway, and voice pipeline. One daemon per profile.

The client/daemon split is forced by invariant 6: if the client owned the run, closing the
terminal would kill it. It also buys §B13's 150 ms first frame — the client has almost nothing
to initialize, and the header paints before the daemon connection resolves.

Profiles are isolated agent roots: separate journal, belief store, credentials, and
permissions. People data does not cross a profile boundary (§A4).

See `DECISIONS.md` for language/runtime and the storage substrate, both of which are ADRs with
measured or defended justification.

---

## 7. What crosses which boundary

Quick reference; the pinned types are in `CONTRACTS.md`.

| From → To | What crosses |
|---|---|
| Loop → Permission layer | `ToolCall` + `TaintSet` |
| Permission layer → Loop | `PermissionDecision` |
| Loop → Journal | Nothing directly. Requests only, via the harness. |
| Loop → Memory | `ClaimRequest` (model-authored, with the run's latched floor) \| `ExternalContent` (arrived from outside, with its channel). Pinned in `CONTRACTS.md` §12.1. The trust class comes **back**; it never goes in. |
| Harness → Journal | `JournalEvent` (typed, signed) |
| Journal → Belief store | Ordered event stream (consolidation derives) |
| Retrieval → Assembler | `RetrievalResult` with token cost + latency attached |
| Assembler → Loop | `ContextView` with per-source budget accounting |
| Tool → Loop | `ToolResult` = typed one-line summary + (bytes \| `ContentRef`) |
| Run plane → Loop | `Checkpoint`, `SteerMessage` |
| Loop → Surface | `TurnEvent` stream (render-only) |
| Broker → Transport | Credential material. **Never** into the loop. |
