> Superseded: [projects-design.md](projects-design.md) is the plan of record; this file stays as the source of the research invariants and the v2 stage map.

# Deep Research v2 on team tasks: design

Status: design only, nothing built. Tags: (m) measured, (e) estimate, (u) unmeasured.

## 1. Summary

- **Code stays code.** Search, fetch, dedup, quote location, corroboration, passage re-reads, standing words, confidence, report render and remember become `research.*` host functions. They are the existing `research_v2` Rust, re-plumbed. The script only wires, and holds handles, never page text.
- **Models stay models.** Pick, compress, group, review, cite-check, write and audit are one-shot, tool-less `agent()` calls. Prompts and parsers live in Rust, so fences, "data not instructions" wording and line formats are Rust-tested.
- **No agent touches the network.** This targets the 131 approval prompts, the 15 compactions (38% of prefill) and the 3.4x duplicate fetching seen in the 35-agent run (m). Zero compactions is a live gate (section 13), not a promise.
- **Two lanes.** Agents use slots 1-5. Fetch (12) and search (4) run in a task lane and hold no model slot, so compress starts the moment a page lands.
- **One chain per sub-question** (search, pick, fetch, compress, group, verify) with no global barrier. Barriers remain only at the plan gate, corroboration, and write/audit.
- **Plan approval parks the run.** No thread or slot held, survives a restart, the conversation never blocks.
- **Every v2 invariant is kept:** verbatim quotes, independence counted by code, required verification with refusal, citation audit, code-derived confidence, no model-written URL. A differential test (same recorded replies in, byte-identical `report.md` and `bundle.json` out) proves parity.
- **Expected time on dawn: about 22-46 min** for the default 5-question, 40-source run (e; assumes 3x aggregate decode on 5 streams, never measured). Dispatch is about 45 min (e); today's thin team took 53 min (m) but checks far less, so they are not like for like. The win is watch, steer, resume and zero-wait, plus hiding fetch time under model time. It is not a model-throughput win.

Built from three candidate designs (faithful port, throughput-first, team-native); this is the merge. Single script, no nested teams.

## 2. Example run on dawn

Matthew: "Run the web research team task. Focus on the latest upcoming AI models, specificially new Kimi models, deepseek, and qwen4. Compile a report md and send it to me when you are done"

Routing note: "web research team task" matches `web-research` today. The `web-research` description will say it is the quick unchecked lookup, and `deep-research` says "researched, cited, verified". Decision D5 settles which one his wording hits.

Plan: 3 sub-questions (Kimi, DeepSeek, Qwen4), breadth 4, depth 1, 24 sources. (e) timings, slots counted on dawn's agent slots 1-5; slot 0 is the conductor.

| Clock | Phase | What runs | Slots busy |
|---|---|---|---|
| 0:00 | conductor | `team("deep-research", {goal})` returns a run id at once; Marlowe says "started, I'll tell you when the plan is ready" | conductor only |
| 0:05 | plan | `research.recall` (memory, no LLM), channel probe, one `planner` agent (about 50 s) | 1 |
| 1:10 | plan gate | Plan card posted (minutes recalibrated for 5 slots, not v2's single-stream `QuoteModel`, or the card lies): `3 questions · breadth 4 · depth 1 · 24 sources · ~0.6M tokens in+out · ~25 min · Not searched: none`. Run parks, NeedsYou | 0 |
| (his time) | | "go" resumes the run in place from its journal | |
| +0:00 | explore wave 0 | 3 lanes: `search` per node (arXiv, web, community, 4 wide), `picker` per node (about 30 s) | 1-3 |
| +2:00 | read | `fetch` (12 wide, 8-14 s each, politeness inside), `compress` agents start per page as it lands, 2 chunks per source | 4-5 |
| +9:00 | explore wave 1 | follow-ups from GAP lines, same lanes; quota is fixed per wave | 4-5 |
| +14:00 | group | one `compressor` call per lane (about 25 claims each) | 3 |
| +16:00 | verify | per lane: review batches (4 groups) and cite batches, up to 3 rungs of 4/2/1; passages re-read in code | 5 |
| +21:00 | corroborate | `research.corroborate` (code, global, independent roots) | 0 |
| +21:30 | write | 3 `writer` sections in parallel, then the answer | 3 then 1 |
| +25:00 | audit | `verifier` re-checks every cited sentence, 6/2/1 per batch | 3 |
| +28:00 | finish | repair, standing words, renumber, confidence, `report.md`, `research.json`, `bundle.json`; `remember` (code only) | 0 |
| +28:30 | relay | notice to session inbox: confidence, first paragraph of the answer, path to `report.md` | |

Result: about 30 min after approval (e), one approval total, zero compactions expected, every URL fetched once. "Send it to me" is the existing relay: the run finishing posts a notice with the answer and the report path; emailing or pushing the file elsewhere is outside this design.

## 3. Stage map

H = host function, A = `agent()`, T = journaled task, P = pure (memoised, not journaled).

| v2 stage | Host | Agent (card) | Parallelism | Journaled |
|---|---|---|---|---|
| Plan | T `recall`, T `channels`, P `plan_prompt`, P `parse_plan`, T `gate` | A `planner` | serial | agent, gate |
| Search | T `search(node, channel)`, P `widen` | none | 4 in task lane | yes |
| Select | P `pick_prompt`, P `picks` | A `picker`; skipped if candidates fit breadth | per node | agent |
| Fetch | P `quota(wave)`, T `fetch(src)` | none | 12 in task lane | yes |
| Compress | P `chunks`, P `compress_prompt`, P `claims` (locate or drop) | A `compressor` per chunk | pipelined per page | agent |
| Group | P `group_prompts`, P `groups` | A `compressor`, per lane | per lane | agent |
| Verify | P `verify_batches` (re-read), P `verdicts`, P `require_verified`, T `refuse` | A `verifier` | 3 rungs | agent |
| Corroborate | P `corroborate` (global, graph) | none | one | no (pure) |
| Write | P `section_prompt`/`section`, P `answer_prompt`/`answer` | A `writer` | sections parallel | agent |
| Audit | P `draft`, P `audit_batches`, P `audits`, P `assemble` | A `verifier` | 3 rungs | agent |
| Finish | T `finish` (writes 3 files) | none | one | yes |
| Remember | T `remember` | none | one | yes |

Dropped vs v2: the cross-question merge model call (D4). Cross-question duplicates may show in two sections; independence counting is still global in code.

## 4. Host functions

Namespace `research`, keyword arguments, small JSON-like returns (a test rejects any task result over a few KB). Handles are strings: source `s_<8 hex>` (from the identity-cascade key, so order-independent), node `n<k>`, claims `C…` assigned at the claim-table barrier in (take order, chunk, item) order as v2.

### Tasks (journaled, laned, tainting)

Key `wf:{id}:{blake3({fn,args})[..16]}`, stored as a task line with `tokens: 0`.

| Signature | Returns | Lane | Notes |
|---|---|---|---|
| `research.recall(query, n=8)` | `{handle, n, untrusted}` | io 2 | Taints only if an untrusted memory is returned. |
| `research.channels()` | `{online, notes}` | io 2 | Time-varying, so journaled. Dead channels become "Not searched". |
| `research.gate(plan, auto_under=0)` | `{decision, plan}` | gate 1 | Parks the run on first call. `auto_under` comes only from the user's own figure (D7). |
| `research.search(plan, node, channel)` | `{cands, n, sent, failed, blind}` | search 4 | Always tainted. A channel panic is one failed query. |
| `research.fetch(plan, src)` | `{doc, title, chars, status, cached}` | fetch 12 | `src` must match `^s_[0-9a-f]{8}$` or it throws. `status`: ok, thin, robots, http, error, memory, over_quota, dup. 404 and robots are `Done` data (never refetched). Tainted except memory. |
| `research.refuse(plan, lanes)` | `{message}` | state 1 | Writes `refusal.md`. |
| `research.finish(report)` | `{report, json, bundle}` | state 1 | Bundle keeps `marlowe.research-bundle/1`. |
| `research.remember(report)` | `{stored, tainted}` | state 1 | v2 `remember.rs` rules; idempotent via the `v2:remember` checkpoint. |

Error model: per-item problems are `Done` data; call-level problems (cancel, no channel online, store corrupt) are `Failed` and give the script `None`, retried on resume; bad arguments throw at call time.

### Pure functions (P)

Deterministic given handles, agent text and the content-addressed store; memoised per `(fn,args)`; blob writes idempotent. Every `*_prompt` returns `{prompt|None, batch}` (None means no call needed); every parser takes `(batch, text)` and treats missing text as a failed reply.

| Signature | Returns |
|---|---|
| `plan_prompt(goal, memory, options)` | prompt (profile block, memories as not-instructions, live channels) |
| `parse_plan(goal, text, options)` | `{handle, subs, depth, breadth, budget, notes, quote{tokens, minutes, fetches, agents}}` (lenient v2 parser, validated bounds) |
| `root_node(plan, sub)`, `quota(plan, wave)` | node; per-node source allowance `min(breadth, ceil(remaining/nodes))` in node order |
| `pick_prompt(cands, node)`, `picks(plan, node, cands, text)` | `{fetch:[src], followups:[node]}`; GAP lines with a URL dropped |
| `chunks(doc, node, max_chunks)` | chunk handles (12-14k chars, paragraph splits, relevance-picked, default 2) |
| `compress_prompt(chunk, node)`, `claims(chunk, text)` | fenced `<<< >>>` prompt; `{handle, kept, dropped, unparsed}` (quote must be found verbatim) |
| `group_prompts(question, claims)`, `groups(batch, text)` | batches by character budget; `{handle, n}` |
| `verify_batches(groups, claims, done, rung)`, `verdicts(batch, text)` | claims whose stored passage no longer matches are Unsupported at once, no call; batches still missing a verdict (review 4/2/1, cite 8/3/1); `{handle, missing}` |
| `verify_status`, `require_verified(lanes)` | `{complete, missing, off_goal}`; `{ok, missing}` (every group reviewed and at least one citation check exists, else refuse) |
| `corroborate(plan, lanes)` | `{handle, roots, established, contested, coverage}`. Applies v2's evidence filter first: drops refuted and off-goal groups, members whose re-read passage is not supported or partial, and groups left with no supporting member. Memory is its own bucket, never a supporting root. |
| `section_prompt`/`section`, `answer_prompt`/`answer` | no evidence gives the "Nothing the run read answers this." section with no call |
| `draft`, `audit_batches(draft, done, rung)`, `audits(batch, text)` | strips any URL the model wrote; audit 6/2/1; missing verdict is Unsupported |
| `assemble(draft, audits)` | `{handle, confidence, answer_first_paragraph, counts, removed, regraded}` (repair, standing words, renumber, code confidence, coverage caps) |
| `progress(stage)` | one-line counts for `log()` |

Truncation contract: no `max_output` or `detail` option exists, so every prompt ends "finish with a line END". A reply without `END` leaves its items pending and the next rung re-asks with a smaller batch. Replaces v2's `:again` doubling. A section still lacking `END` after one retry is cut back to its last full sentence with `CUT_NOTE` and listed in `Coverage.truncated_sections`, which caps confidence at Low (v2 invariant 15). No `schema=` anywhere: verdicts are text lines, so dawn's ~50% first-try tool quality and the effort-off `finalise()` path are never reached.

## 5. The built-in script

`workflows/deep-research.star`, about 80 lines of wiring, all prompt text in Rust. Dialect rules respected: no f-strings, `is`, `while` or recursion; lambdas bind with `x = x`; no top-level host calls. Spike to confirm: `break`, ternary, and `parallel` nested in `pipeline`.

```python
META = {
    "name": "deep-research",
    "description": "Researched, verified, cited report: a plan you approve, parallel search and reading of public sources, quotes checked in code, independence counted by code, every cited sentence re-checked. Args: topic.",
    "when_to_use": "The user wants deep, sourced, verified research, a cited report, or the research team.",
    "phases": ["plan", "explore", "read", "check", "write", "audit", "remember"],
    "args": "topic (required, the user's own words); sources, depth (optional); auto_approve_tokens (only if the user gave a figure)",
}

def read_source(plan, node, ref, qi):
    doc = research.fetch(plan = plan, src = ref)
    if doc == None or doc["doc"] == None:
        return []
    chunks = research.chunks(doc = doc["doc"], node = node, max_chunks = 2)
    outs = parallel([lambda c = c: agent(research.compress_prompt(chunk = c, node = node), label = "compress q" + str(qi + 1), phase = "read", agent_type = "compressor") for c in chunks])
    kept = []
    for j in range(len(chunks)):
        r = research.claims(chunk = chunks[j], text = outs[j] or "")
        if r["kept"] > 0:
            kept.append(r["handle"])
    return kept

def do_node(plan, node, qi):
    found = research.search(plan = plan, node = node)
    if found == None or found["n"] == 0:
        return {"claims": [], "followups": []}
    sel = research.pick_prompt(cands = found["cands"], node = node)
    text = ""
    if sel["prompt"] != None:
        text = agent(sel["prompt"], label = "pick q" + str(qi + 1), phase = "explore", agent_type = "picker") or ""
    picks = research.picks(plan = plan, node = node, cands = found["cands"], text = text)
    per = pipeline(picks["fetch"], lambda prev, ref, k: read_source(plan, node, ref, qi))
    claims = []
    for s in per:
        if s != None:
            claims.extend(s)
    return {"claims": claims, "followups": picks["followups"]}

def verify_lane(qi, groups, claims):
    done = []
    for rung in range(3):
        todo = research.verify_batches(groups = groups, claims = claims, done = done, rung = rung)
        if len(todo) == 0:
            break
        outs = parallel([lambda b = b: agent(b["prompt"], label = "check q" + str(qi + 1), phase = "check", agent_type = "verifier") for b in todo])
        for j in range(len(todo)):
            done.append(research.verdicts(batch = todo[j]["batch"], text = outs[j] or "")["handle"])
    return research.verify_status(groups = groups, claims = claims, done = done)

def lane(sub, qi, plan):
    nodes = [research.root_node(plan = plan["handle"], sub = qi)]
    claims = []
    for wave in range(plan["depth"] + 1):
        if len(nodes) == 0:
            break
        done = parallel([lambda n = n: do_node(plan["handle"], n, qi) for n in nodes])
        nodes = []
        for d in done:
            if d != None:
                claims.extend(d["claims"])
                nodes.extend(d["followups"])
    batches = research.group_prompts(question = qi, claims = claims)
    outs = parallel([lambda b = b: agent(b["prompt"], label = "group q" + str(qi + 1), phase = "read", agent_type = "compressor") for b in batches])
    groups = []
    for j in range(len(batches)):
        groups.append(research.groups(batch = batches[j]["batch"], text = outs[j] or "")["handle"])
    return {"qi": qi, "claims": claims, "groups": groups, "status": verify_lane(qi, groups, claims)}

def main(args):
    if type(args) != "dict":
        args = {"topic": str(args)}
    goal = str(args.get("topic") or args.get("question") or "").strip()
    if goal == "":
        fail("deep-research needs a topic: team(name = \"deep-research\", args = {\"topic\": \"what to research\"})")
    phase("plan")
    mem = research.recall(query = goal, n = 8)
    chans = research.channels()
    text = agent(research.plan_prompt(goal = goal, memory = mem, channels = chans, options = args), label = "plan", phase = "plan", agent_type = "planner")
    plan = research.parse_plan(goal = goal, text = text or "", options = args)
    verdict = research.gate(plan = plan["handle"], auto_under = args.get("auto_approve_tokens") or 0)
    if verdict == None or verdict["decision"] == "reject":
        return {"status": "cancelled", "summary": "The plan was not approved; nothing was read."}
    plan = verdict["plan"]
    phase("explore")
    lanes = pipeline(plan["subs"], lambda prev, sub, qi: lane(sub, qi, plan))
    good = [l for l in lanes if l != None and l["status"]["complete"]]
    req = research.require_verified(lanes = good)
    if len(good) < len(plan["subs"]) or not req["ok"]:
        r = research.refuse(plan = plan["handle"], lanes = [l for l in lanes if l != None])
        fail(r["message"])
    phase("write")
    corro = research.corroborate(plan = plan["handle"], lanes = good)
    secs = parallel([lambda i = i: write_section(corro["handle"], i) for i in range(len(plan["subs"]))])
    handles = [s["handle"] for s in secs if s != None]
    ans = agent(research.answer_prompt(corro = corro["handle"], secs = handles), label = "answer", phase = "write", agent_type = "writer")
    answer = research.answer(corro = corro["handle"], secs = handles, text = ans or "")
    draft = research.draft(plan = plan["handle"], corro = corro["handle"], secs = handles, answer = answer["handle"])
    phase("audit")
    audits = audit_lane(draft["handle"])
    report = research.assemble(draft = draft["handle"], audits = audits)
    out = research.finish(report = report["handle"])
    phase("remember")
    mem_out = research.remember(report = report["handle"])
    return {"status": "done", "confidence": report["confidence"], "summary": report["answer_first_paragraph"], "counts": report["counts"], "report": out["report"], "bundle": out["bundle"]}
```

The block is excerpted: `write_section` (one retry with a shorter prompt if `END` is missing) and `audit_lane` (same shape as `verify_lane`) are omitted here for length, but the real file must define them, since names are resolved before approval. Agent calls sit in helper `def`s called only from `main`, never at top level. The return value has no `findings` key. Strict by default: an incomplete lane refuses the run (no report), per v2 invariant 9.

## 6. Roles

| Step | Card | Tools | Effort on dawn (off/medium only) |
|---|---|---|---|
| Plan | `planner` (new) | none | medium |
| Pick | `picker` (new; `scout` has web tools) | none | off (low) |
| Compress, group | `compressor` (existing) | none | off (low), re-check yield (M3) |
| Review, cite, audit | `verifier` (existing) | none | medium, **never off**, retries keep effort |
| Sections, answer | `writer` (new, tool-less `researcher`) | none | medium |

Every call names `agent_type`; without it an agent gets the broad `workflow` role (shell, write, web). New cards declare `sandbox: none`, which needs a fix: the host ignores a card's `sandbox:` today, so a tool-less agent would still create a git worktree. Old `scout`/`researcher` cards stay for dispatch and `web-research`.

## 7. Slots, fetches and context

Lanes: agents 5 (`min(16, cpus-2, local slots)`); `search` 4; `fetch` 12; `gate`/`finish`/`remember` 1. The engine queue must scan for the first runnable item per lane, or a full agent lane blocks fetches (head-of-line). Drain-first: a later phase's queued agents run before an earlier phase's, so started work finishes.

Window 49,152; compaction at 40.5k (m); reply cap 13,312 (m); about 36.6k room. Rule: every prompt under the derived cap (about 24k on dawn).

| Step | Prompt (e) | Worst total (prompt + 13k reply) |
|---|---|---|
| Plan | 3-4k | ~16k |
| Pick | 4-6k | ~19k |
| Compress (14k-char chunk) | 6-8k | ~21k |
| Group (about 25 claims per lane) | 2-3k | ~16k |
| Review (4 groups) | ~7k | ~20k |
| Cite (8 claims) | ~3k | ~16k |
| Section (up to 40 claims) | 6-8k | ~21k |
| Answer | ~6k | ~19k |
| Audit (6 sentences) | ~3k | ~16k |

No step is an open-ended loop, so typical steps stay under the 40.5k trigger. Caveat: the engine continues a cut-off reply up to 3 times (`MAX_CONTINUES`), each continuation adding up to a reply to the context, so a 24k prompt plus one continuation can reach the trigger. The 24k cap is a ceiling; the cap and batch sizes are derived from the model's window at run time (window minus 4,096, times 0.9, minus reply), not hardcoded to dawn. `*_prompt` splits a batch when it would exceed it. Prompt cache: role-invariant instruction block first, fenced data last; histories are never rewritten. Review batch is 4 not 8 because dawn's thinking and reply share one 13k budget (the v2 "empty reply" bug).

Time (e), default 5 questions, 40 sources: single-stream model work about 3,250-6,800 s; at 3x aggregate speedup on 5 streams about 18-38 min, plus about 4-8 min of serial tails. Fetches (8-14 s, arXiv 3 s limiter) hide under model time. Live targets: average busy slots at least 3 (m baseline 1.08), zero compactions, fetches per unique page 1.0, approvals asked 1.

## 8. Plan approval and the conversation

1. `team` returns a run id at once. The plan agent drafts (about 1 min).
2. `research.gate` validates bounds (not clamped for edits), re-quotes in tokens in+out, stores the proposed plan, sets `ctl.parked`, status `NeedsYou`, emits `PlanProposed`, and ends the pass cleanly. No thread or slot held; a daemon restart changes nothing.
3. Inbox notice: `Plan ready: 3 questions · breadth 4 · depth 1 · 24 sources · ~0.6M tokens · ~25 min · Not searched: none · approve / edit / reject`.
4. Existing `Supervisor::approve_plan` re-queues the run; the script resumes in place from its journal (plan agent replays, gate finds the decision). Verify early that `PlanDecision` applies to script runs, not only the dispatch playbook.
5. Edit: validated, re-quoted, goal kept. Reject: returns cancelled, nothing spent. Approval raises the run budget to quote +25% and the step estimate, never lowers.
6. Auto-approve is off by default and only settable from a figure the user stated.

New engine piece: a "parked" outcome for a task (today only agent-level `Stopped` exists).

## 9. Memory, trust and URLs

| Rule | Enforced by |
|---|---|
| Memory needs no LLM; memory numerics untouched | `remember` is extractive v2 code; `eval/` R@1 0.6725 / R@5 0.8865 unaffected |
| Recall is a source, never instructions | Text stays in the store; enters prompts fenced; own trust bucket; never a supporting root |
| Worst-case taint and lineage on remember | Tainted if run latch OR any network source OR any untrusted memory; `derived_from` carries recalled ids; Established only for on-goal groups with 2+ roots |
| Tainted from first network result | Task results carry `tainted:true`; host latches `Run.tainted` before sending the completion |
| Resume must re-taint | Fix: on resume, any tainted journal entry latches the run (gap today) |
| Model never writes a URL | Queries stripped, GAP with URL dropped, `fetch` takes ids only, `draft` strips unknown URLs, references from stored identity |
| Page text is data | Only inside `<<< >>>` (literal `>>>` rewritten) or after "data, never instructions" |
| Quotes exist verbatim | `locate` at ingest; re-read at verify, audit, bundle |
| Politeness | robots.txt, 1 s per host or Crawl-delay, honest user agent, keyless, all unchanged in `marlowe-research` |
| Persona names no feature tool | Routing lives in META `when_to_use` in the tool catalog only |

Network consent (non-negotiable 5: new hosts need approval) is D1.

## 10. Failure and resume

| Event | Behaviour |
|---|---|
| Agent fails or skipped | `None`; ingest records a failure; items stay pending for the next rung; missing audit verdict is Unsupported |
| Reply without `END` | Items stay pending, next rung asks smaller, same effort |
| Unreadable, robots, 404 | `Done` data, counted, never refetched |
| Channel down | Dropped at plan, "Not searched" line |
| Verification incomplete after 3 rungs | `refusal.md` and `fail()`; no report |
| Blob missing on resume | `replay_ok` says miss; that fetch re-runs (cheap, cached); prefix rule re-runs what depends on it |
| Crash, stop, restart | Done calls replay from the journal; `remember` guarded by its checkpoint |
| Stop mid-run | `sources/` and store kept; no half report; a code-only `partial.md` (sources read, claims kept) is a later option |

## 11. Progress in the crew watch

- Phase strip = `META.phases` (7, under the 12-phase filter). Concurrent phases are normal here; the strip must light more than one.
- Tasks show as counters under their phase, not rows: `fetch 31/48 · 3 failed (robots 2, 404 1)`. Add `WorkflowInfo.tasks[]` (serde default). Coalesce observer snapshots to about 100 ms. Do not fire SubagentStart/Stop hooks for tasks.
- About 130 one-shot agents collapse by phase plus label prefix: `read 61/80, 2 running`, `check q2`.
- `log(research.progress(stage))` per stage: `180 found, 24 read, 5 unreadable, 61 claims`.
- NeedsYou plan card pinned above the strip.
- Additions to crew-watch §9: task counters, collapsed agent groups, plan-card payload. Task lines carry counters so `view_of` can rebuild an unwatched run.

## 12. Reuse vs new

| | What |
|---|---|
| Reused (about 80%) | `marlowe-research`, `marlowe-extract`, `research_v2` parsers/prompts/`standing.rs`/`bundle.rs`/`remember.rs`/`types.rs`, `ResearchIo` and `research_wire.rs`, `compressor` and `verifier` cards, `research_eval.py`, parked-gate mechanism |
| Refactor, no behaviour change | Stage drivers split into (prompt-maker, reply-ingest) over a run store; dispatch keeps using them |
| New: engine (seams in `crates/marlowe-workflow/src`) | `research` namespace beside `budget` (`engine.rs:1077`); `task_call` sibling of `agent_call` (`engine.rs:394`); replay check beside the key match (`engine.rs:411-412`); lane scan in `launch` (`engine.rs:1240`); `start_task` default on `AgentHost` (`host.rs:123`); `Line::Task` beside `Line` (`journal.rs:36`) and a sibling of `Resume::from_records` (`journal.rs:84`; unknown lines are already skipped on load). Full list: task lane (`Queued` enum, `TaskCall`, per-lane caps, runnable-first scan), `Line::Task`, `replay_ok`, task events and counters, `AgentHost::start_task` default, parked outcome, snapshot throttle, drain-first, resume re-taint |
| New: crew | `script/research/` (run store, bindings), `NetGate`, card `sandbox:` honoured, `ResearchIo` injected into `CrewAgentHost` (built in `wire.rs:274` only when not scripted today) |
| New: content | `workflows/deep-research.star`, cards `planner`, `picker`, `writer`, `BUILTINS` entry (array length 3 to 4), catalog assertions in `marlowe-workflow/tests/builtins.rs` and `marlowe-exec/src/specs/workflow/tests.rs`, `workflow-authoring.md` note, fix `ARCHITECTURE.md:178` ("retry with thinking off"; code never does that) |
| Dispatch | Stays the reference until parity; then `deep_research` becomes a launcher for the team task |

## 13. Evaluation

Deterministic (no GPU, every commit):
- Differential test: recorded v2 replies through dispatch driver and team host with a scripted agent host; `report.md`, `research.json`, `bundle.json` identical apart from run id and times. Fixtures never regenerated.
- Replay is byte-stable; kill and resume at each phase boundary gives zero refetches and zero re-asked agents; removed blob refetches live; refusal path; taint latch survives resume; checker rejects a top-level `research.` call; task result size cap.

Live on dawn (needs GPU go-ahead):
- 10 questions in `tools/research_questions.json` through both paths (key points still drafts until Matthew reviews).
- Gates: micro citation accuracy at least 0.95 (strict also reported; v2 first run was 0.897 strict) and no worse than dispatch on dawn; key-point coverage not below dispatch; zero compactions; fetch duplication 1.0; median wall time 45 min or less; conductor slot wait at most 0.2 s.
- Judge `marlowe-dusk:27b-super` runs offline afterward with dawn unloaded. Known bias: judge and writer family overlap; rerun judge fixtures with dawn as verifier before trusting its numbers.
- Unaided test: his own wording, conductor picks the task by name, plan card appears, "go" resumes, report with Confidence arrives. Also 8 phrasings added to `eval/tool_probe/cases.jsonl`; target 8 of 10 route correctly. Kill the daemon during `read`, resume: no refetch, no second plan prompt.

## 14. Build plan

Smallest valuable first. Every cargo step through `scripts/cargo-idle.ps1`, `-j2`-`-j4`, one build, only when Matthew says the machine is free.

| Phase | Work | Acceptance |
|---|---|---|
| P0 Measure (no code) | Diagnose arXiv 56 s paper-fetch wait (m: 162 gaps); decode speed at 4-5 streams; how card efforts map to off/medium; compress yield at off vs medium on 20 stored chunks; spike dialect (`break`, ternary, nested parallel) | A one-page table; D3 decided |
| P1 Engine seam | Task lane, `Line::Task`, `replay_ok`, runnable-first scan, counters, parked outcome, resume re-taint, card `sandbox:`, snapshot throttle | `ScriptedHost` tests: fetch does not hold an agent slot, replay identical, cancel/skip reach tasks, resume re-taints, existing teams unchanged |
| P2 Split | Stage drivers into prompt-maker and ingest; dispatch uses them | Existing v2 tests and fixtures green and unchanged; no retrieval numerics change |
| P3 Host | Task functions, pure functions, `NetGate`, `ResearchIo` injection | Differential test passes; `fetch` rejects non-ids; atomic quota; id stability under shuffled completion order |
| P4 Script | `deep-research.star`, three cards, `BUILTINS`, catalog tests | Scripted run to a schema-valid bundle; park, restart, resume; kill mid-fetch and resume; refusal case |
| P5 Watch | Task counters, collapsed groups, plan card | Crew watch renders 80+ agents without scrolling the strip |
| P6 Live and eval | 1 question, then 10; unaided test | Section 13 gates |
| P7 Retire | `deep_research` launcher; `web-research` decision; docs and CHANGELOG | Catalog tests updated deliberately |

First valuable slice: P1 + a tiny script with `fetch` and `compress` only, to show fetch/compress overlap and the busy-slot gain before anything else is built.

## 15. Decisions for Matthew

1. **D1 Network consent for host fetches** (non-negotiable 5).
   - A: run-scoped grant at the plan gate: GET-only, public https, hosts reached via this run's own results, robots honoured, SSRF guard, no credentials, fetch quota. Raw URLs, http, private addresses still ask.
   - B: ask per new host (about 131 asks per big run).
   - Recommend A. Needs your explicit yes since it relaxes the rule for research only.
2. **D2 Where the plan gate waits.** A: park the run (no thread, survives restart). B: blocking ask on a task thread. Recommend A.
3. **D3 Effort on dawn for pick, compress, group.** A: off (quote check is code). B: medium. Recommend A, decided after P0 yield measurement; verifier, planner, writer at medium, verifier never off.
4. **D4 Cross-question merge call.** A: drop it. B: keep it as a final global call. Recommend A. Cost: a fact backed by one root in two questions shows as two Suggested groups, not one Established, so standing words and remember are slightly conservative. Check in the eval.
5. **D5 `web-research` future and your wording.** A: keep as quick unchecked lookup, `deep-research` for "deep/verified/cited", then alias later. B: alias `web-research` to `deep-research` now. Recommend A; note your example prompt ("web research team task") would still hit the quick one under A, so you decide which you want that phrase to mean.
6. **D6 One incomplete question.** A: refuse the whole run (v2 behaviour). B: omit it, cap confidence Low (`allow_partial`). Recommend A, B as opt-in.
7. **D7 Pre-approval.** A: `auto_approve_tokens` only when your own message gives a figure. B: never. Recommend A.
8. **D8 Chunks per source.** A: 2, relevance-picked. B: first 4 as v2. Recommend A, gated on eval claim yield.
9. **D9 Tasks in the crew watch.** A: counters per phase. B: rows. Recommend A.
10. **D10 `agent()` options.** A: none new (tool-less cards plus `END` contract). B: add `tools`, `max_output`, `retry`, `detail`. Recommend A.
11. **D11 Budget unit.** A: move the pool to tokens in+out first (crew-watch §9 #10) so quote and enforcement agree. B: keep output-only. Recommend A.
12. **D12 Dispatch retirement.** A: launcher after parity. B: keep both. Recommend A.

Reply with letters, e.g. `1A 2A 3A 4A 5A 6A 7A 8A 9A 10A 11A 12A` (all recommendations).

## 16. Provenance

Design only; nothing built or run. All times are estimates until P0 and P6.
