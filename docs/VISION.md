# MarloweCowork: Vision

MarloweCowork is a research engineer that lives on your machine. It remembers everything you have worked on, watches your experiments while you are away, runs a crew of agents in sandboxes, and greets you every morning with what changed. One Rust binary, terminal-native, voice-capable, local-first, and your memory stays yours.

## Why this exists

Marlowe_Harness (about 127k lines of Rust, 18 crates) has a strong memory engine and a TUI that works. It stalled at the agent plane: a PI/intern hierarchy buried in security ceremony and sized for 4-9B local models, which never converged.

The category has since moved. OpenAI's Dots (2026-09-29) and SpaceXAI's Grok Bot (beta, 2026-08-11) define the "AI coworker": cloud agents with their own cloud computer, reached through Slack, Teams or chat. Their memory is opaque and vendor-side. Neither does briefings or ambient awareness well, voice is promised but not shipped, and both ignore local engineering and research workflows.

MarloweCowork keeps what worked (look, lightness, journal, memory, executors), replaces the agent plane, and makes memory the differentiator.

## Decisions

- **Model library.** Users start with no models: "You have no models. Add one?" leads to a provider list (Ollama, Local GGUF, OpenRouter, Anthropic, OpenAI). Only Ollama and OpenRouter are built first; the rest show as "coming soon". Adding a model auto-detects a running Ollama and API keys in the environment. Each slot (Conductor, each Crew role, Reflection, Quick) picks its own model and effort. The first model added becomes the default for every slot.
- **Voice** speaks and listens. Text stays primary.
- **Address.** "Sir" in greetings and alerts; "Matthew" or nothing mid-conversation.
- **Crew autonomy.** Free inside a sandbox. Merge, push, deletes outside the workspace, new network hosts and budget overrun need approval.
- **UI.** Layout and categories are open to reinvention. The colour scheme stays exactly.

## 1. The Wake: "Good morning, sir."

`marlowe` opens its own configured Windows Terminal window. The first frame paints in under 50 ms. A boot sequence of about 1.2 s follows (any key skips it; it is shorter on relaunch). The braille presence meter assembles, then the systems lines tick in:

```
⣿ MARLOWE · cowork
memory ····· 4,212 beliefs · 318 episodes · recall online
brains ····· sonnet (openrouter) · dusk-22b (ollama)
crew ······· 2 runs finished overnight · 1 awaiting review
watch ······ 3 repos · 2 training runs · arXiv: 6 new in your topics
voice ······ online · hold ^space to talk
```

Marlowe then speaks and writes a greeting that knows the time and how long Matthew has been away:

> "Good morning, sir. The valiance fine-tune finished at 04:12 with eval loss 1.82, the best of the dusk line. Overnight I finished the attention-sink sweep: 14 papers, report in `research/attention-sinks.md`. One thing needs you: the gate refactor is ready for review."

The briefing is real. It is composed from Sentinel observations, crew dispatches and open threads in memory. On first run it imports the old harness's journal, so the first Wake already knows Matthew.

## 2. Total Recall

Memory is the reason to prefer this over a cloud coworker.

- **Everything is captured automatically.** Every user turn is ingested at user trust, instantly, with no maturation delay. Assistant prose is stored as episodic memory and is never auto-injected as fact. Web and agent content keeps the 6-hour maturation window and untrusted lineage. Embeddings are computed on write by a low-priority background queue.
- **Retrieval before every turn is invisible.** BM25 and dense scores are fused with RRF, the cross-encoder reranks, and the calibrated gate abstains when confidence is too low. Marlowe never claims a memory it did not retrieve.
- **Memory is structured.** Facts and preferences carry supersession ("you changed your mind on 3 Sep, and here is why"). Decisions keep their reasons. Episodes (session summaries on a timeline) answer "what were we doing Tuesday?". Project scope is per repo or workspace. Research notes, experiments and procedures are `payload_kind` tags on beliefs.
- **Contradiction asides.** When a request conflicts with a stored decision, Marlowe raises it once: "You decided X on 3 Sep because Y. Still want this?"
- **Dreaming.** When idle or overnight, the daemon consolidates (dedup into supersession) and reflects: day to episode, open commitments into Threads, user-model update. This feeds the brief.
- **Owned and auditable.** The signed, hash-chained journal is tamper-evident and exportable. The Memory view lets Matthew audit, correct and forget, and "why do you think that?" returns provenance. Memory is invisible in conversation and inspectable on demand.

## 3. The Crew

- **An agent is four things:** a role card (a markdown file Matthew can edit), a model and effort slot, a tool allowlist, and a budget plus sandbox. Shipped roles: Researcher, Engineer, Reviewer, Analyst, Scout.
- **The conductor never blocks.** `dispatch` returns a run id immediately. Each run executes on its own thread as a durable Run: checkpointed, and resumed automatically after a daemon restart. Results land in the conductor's inbox and are relayed at the next step, or in a short relay turn if idle. Runs are depth 1 in v1.
- **Two Rust playbooks ship in v1**, as deterministic orchestration:
  - Deep Research: plan, parallel scouts (web, arXiv, local files, papers via marlowe-extract), claim extraction with citations, adversarial verify, cited report.
  - Build: plan, implement in a git worktree, build/test loop, Reviewer pass, diff presented for merge.
  Investigate, Review and Experiment ship as role-card prompts and are promoted to playbooks later.
- **Visibility.** The Crew rail shows status, step, spend and current action per run. The Run view shows the transcript, a steer box (guidance injected mid-flight), pause/cancel and diff review.
- **Dispatches** return summary, artifacts, confidence and citations, and are written to memory automatically.

## 4. The Sentinel

- **Experiment awareness.** Marlowe tails `trainer_state.json`, JSONL/CSV metric logs and tfevents, and reads GPU stats (NVML). It raises notices for NaN or divergence, plateau, best-so-far and finished: "the valiance run plateaued at step 12k; LR schedule still has 40% left."
- **Watchers:** git repos (commits, branch state, CI); file globs; processes ("tell me when this finishes"); the Ollama model store; arXiv topics from an explicit list (1 request per 3 s, cached).
- **Routines:** "weekdays 08:30 brief me", "Fridays summarise the week", an end-of-day wrap-up, and an overnight queue of crew work within budget.
- **Notices** are ranked, deduplicated, said once, and respect quiet hours. They appear on the board and are spoken when important.
- **Live repo context.** Marlowe always knows the active workspace's branch, dirty files, last commit and failing tests.

## 5. Voice

- **Wake v1 (M3):** WinRT `SpeechSynthesizer` via the `windows` crate. About 150 lines; works on ARM64.
- **Full voice (M6):** `sherpa-onnx` (official Rust crate, prebuilt static libs for win-x64 and win-arm64). Kokoro TTS with British male voices (`bm_george`, `bm_lewis`, `bm_daniel`); Moonshine v2 STT on CPU (Whisper-small optional); push-to-talk on `^space` over `cpal` WASAPI.
- **Sidecar.** It runs as `marlowe-voice.exe`, spawned lazily by the daemon over stdio JSON lines. This avoids ORT symbol clashes with the memory engine, isolates crashes and keeps the daemon light. Because the daemon owns it, alerts can speak while the TUI is closed.
- Windows' built-in speech recognition is ruled out: it needs MSIX package identity.
- **Barge-in:** a key press stops TTS. Voice barge-in needs echo cancellation and comes later, with a wake word and an optional cloud TTS slot.
- **Presence:** the braille meter in the title bar is driven by real mic and TTS amplitude.

## 6. The Look

The palette cannot change. `theme.rs` is kept verbatim:

- accent `#9B7EDE`, with structure, hover and dim borders derived by scaling;
- Marlowe's voice `#b39ae8`;
- green `#8fd4a8`, amber `#d6a95f`, red `#dd7f85`, blue `#7fa8dd`;
- foreground weights `#cdc9d6`, `#6a6480`, `#4a4460`;
- the terminal's own background (`#0f0e14` in the launched WT profile) and no background fills;
- braille glyphs, plus the markdown and LaTeX renderers.

The layout is Conversation + Situation Board (at least 120x30, mouse-enabled). Every box is a
region with its label on the top border and its hotkey on the bottom; the board stacks its four
sections when the window is tall enough, else shows them as numbered tabs:

```
 * marlowe — what changed overnight?                                 3 runs · 1 needs you
╭ Conversation · what changed overnight? ──── ⣀⣤⣶⣤⣀ idle ╮╭ Model ───────────────────────╮
│  you  what changed overnight?                          ││ Sonnet 4.5 ▾    medium · 200k│
│  ⋯ recall · 3 memories · valiance gate threshold       │╰──────────────────────── (m) ─╯
│  The valiance run finished at 04:12…                   │╭ Workspace ───────────────────╮
│                                                        ││ ~/valiance ▾                 │
│                                                        │╰──────────────────────── (w) ─╯
│                                                        │╭ Crew ─────────── 1 needs you ╮
│                                                        ││ ◐ engineer · gate-refactor   │
│                                                        ││   ready to merge             │
│                                                        ││ ● researcher · attention-sin…│
│                                                        ││   step 14 · $0.41 · explore  │
│                                                        │╰──────────────────────── (1) ─╯
│                                                        │╭ Watch ────────────── 1 alive ╮
│                                                        ││ ● valiance-ft  layer 9 · 40s │
│                                                        │╰──────────────────────── (2) ─╯
│                                                        │╭ Threads ─────────────────────╮
│                                                        ││ no open threads              │
╰────────────────────────────────────────────────── (c) ─╯╰──────────────────────── (3) ─╯
╭ Message ───────────────────────────────────────────────╮╭ Memory ──────────────────────╮
│ › message Marlowe…            ⇧↵ newline · esc boxes   ││ 390 memories · 41 episodes   │
╰────────────────────────────────────────────────── (i) ─╯╰──────────────────────── (4) ─╯
 ^n New  ^o Sessions  ^d Dispatch  ^r Crew  ^g Memory  ^k palette          Friday · 21:40
```

- **Header:** one borderless row: `* marlowe — <session>` and the live summary (crew and watched
  runs, what needs the user, new notices). The window title reads `marlowe — <session>` (OSC 2),
  and the terminal's default background is set to `#0f0e14` (OSC 11, reset with OSC 111 on exit).
- **Pickers:** the conductor model and the workspace, as boxes with dropdowns.
- **Board:** Crew, Watch, Threads, Memory; each run, watched item and section opens its full view.
- **Footer:** the chords and the day (date, quiet hours, clock). Every chord is also a button.
- **Pointer:** the box, row, tab or chord under the pointer takes the hover tone or brightens its
  foreground, never a fill; a click acts on it.
- **Full-screen views:** Run (live transcript, steer, pause/cancel, merge/discard, the research
  stage strip), Library (models, slots, effort; "no models, add one"), Memory, Watch. Each shows its
  keys on its bottom border; Esc, and Backspace where nothing is typed, go back one level.
- **Overlays:** the approval modal, the ^k palette, sessions, dispatch, onboarding and the Wake.

## 7. Persona v3

Persona v2 is kept intact: anti-sycophancy, answer first, no emoji, never invents familiarity. v3 adds a JARVIS layer: loyal and anticipatory; dry wit allowed in low-stakes moments; "sir" in greetings and alerts; familiarity earned through retrieved memory.

## 8. Budgets

| Measure | Budget |
|---|---|
| Distribution | Single `marlowe.exe` (plus lazy `marlowe-voice.exe` and `marlowe-typeset.exe`, the maths typesetter) |
| Cold start to first frame | < 100 ms |
| Idle daemon RAM, ORT unloaded | < 80 MB |
| Harness overhead per turn, excluding model and retrieval | < 50 ms |
| Retrieval P95 | < 300 ms |
| Boot time, at any journal size | Must not grow with history (snapshots + watermark) |

## Why this beats Dots and Grok Bot

- **Memory you own.** Theirs is opaque and vendor-side. Ours is a signed, hash-chained journal on your disk, with provenance on every recalled fact, supersession history and a forget control.
- **Recall that abstains.** Calibrated gating means Marlowe says nothing rather than inventing a memory.
- **Briefings and ambient awareness.** Both competitors are reactive chat endpoints. The Sentinel watches training runs, repos and arXiv, and the Wake turns that into a spoken brief.
- **Local engineering and research workflows.** Training-log awareness, GPU stats, git worktrees and paper ingestion are first-class, not afterthoughts.
- **Voice that ships.** Spoken greeting in M3, full local voice in M6, in a crash-isolated sidecar.
- **Local-first.** Your workspace, your models (Ollama or any provider you add), no mandatory cloud computer.
