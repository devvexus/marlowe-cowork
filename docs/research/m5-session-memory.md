# M5 — Session memory (endless conversation on local models)

Matthew, 2026-10-12. Not started. Local models only.

## Goal

Talk to Marlowe endlessly. He is sharp on the present, and like a person he "refreshes his memory"
on something from 500k tokens ago: before he generates anything, not through a tool he must think
to call. Context stops being a budget and becomes working memory.

## Three tiers

| Tier | What | Status |
|---|---|---|
| Long-term | Cross-session BeliefStore (BM25 + dense + rerank + gate) | Shipped |
| Medium-term | Per-session index of everything that slid out of the window, verbatim | New |
| Short-term | The model's window, KV-cached | Exists |

## Prompt order (the cache rule)

The KV cache holds only up to the first changed token; everything after a change is re-read. So
the part that changes every turn goes last:

```
<system> <tools>          cached once
<conversation>            append-only; cached as it grows
<scratchpad + recalled>   rebuilt every turn, small
<user message>
```

A turn re-reads only the last exchange plus the tail. Editing the scratchpad never touches the
conversation's cache. A scratchpad placed before the conversation would force a full re-read on
every edit.

## Sliding window

- Slide in chunks, not per turn: at ~90% of the window, evict the oldest down to ~60%. That is one
  re-read every so often instead of every turn, shown live (`re-reading 19.2k · 2,100 tok/s`).
- dusk is qwen35, which has recurrent layers, so llama.cpp's cheap context shift is not assumed to
  work. Design for the chunked re-read; a cheap shift on pure-attention models is a bonus.
- The window is a dial the context planner sets from VRAM: small on the 16 GB card, large on the
  128 GB machine. The mechanism is the same.

## Medium-term memory

- Evicted chunks are indexed per session with the long-term pipeline, scoped to the session.
  Everything is kept verbatim: code, tool output, decisions.
- Before every generation it runs automatically, shown live as a recall stage
  (`recall · session 1,840 chunks · bm25 · dense · rerank`).
- Excerpts go in the tail with provenance ("you wrote this 41 min ago, turn 212"). The gate
  abstains when nothing is relevant, so most turns add zero tokens.
- Excerpts keep their original trust: an untrusted web page stays untrusted when recalled.
- No model call anywhere. This replaces today's summariser-call compaction
  (`crates/marlowe-agent/src/context.rs`).

## Scratchpad

State, not search. "How's that workflow?" has no keyword; the answer has to be pinned. The harness
maintains the scratchpad, never a model call:

- active workflows and tasks (the background-work notice);
- todos and the pinned working set;
- open files;
- the current goal;
- the session's decisions.

## Risks

- Recall is cue-dependent: a thread nobody references by keyword fades. Mitigation: a short
  extractive digest per evicted chunk (files touched, decisions, first lines), plus episodes.
- A chunked slide costs one visible re-read of the remaining window.
- Prefill of recalled excerpts every turn: keep the tail bounded.

## Done when

On a long-session eval (facts and code planted at 50k tokens, asked at 300k), session memory beats
plain truncation and summary compaction on recall and answer accuracy, on dusk at the planner's
window. LongMemEval's long-term numbers are unchanged (R@1 0.6725 / R@5 0.8865).

## Later (M5.5)

Retrieval inside the model: evicted KV blocks are kept and attended to on demand (the InfLLM /
Memorizing Transformers line). This needs the patched llama.cpp engine and is part of M5.5's
activation-level memory spike.
