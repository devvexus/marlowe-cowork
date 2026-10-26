# MarloweCowork

MarloweCowork is a local-first research engineer that lives on your machine. It is a single Rust binary, terminal-native and voice-capable, that remembers everything you have worked on, watches your experiments while you are away, runs a crew of agents in sandboxes, and briefs you each morning on what changed. It descends from Marlowe_Harness and inherits its look, its signed journal, its memory engine and its executors, and it replaces the agent plane that never converged. Memory is the weapon: it is owned by you, auditable, and never vendor-side.

## The six pillars

- **Wake**: a fast boot sequence and a spoken, memory-grounded greeting ("Good morning, sir.") composed from real overnight events.
- **Total Recall**: every turn is captured, retrieval is invisible and gated by calibrated abstention, and the signed journal keeps it all auditable.
- **Crew**: role-carded agents (Researcher, Engineer, Reviewer, Analyst, Scout) run as durable, non-blocking runs in sandboxes; Deep Research and Build ship as playbooks.
- **Sentinel**: ambient awareness of training runs, git repos, processes, the Ollama store and arXiv, with ranked notices and routines.
- **Voice**: spoken output first (WinRT), then full local voice (Kokoro TTS, Moonshine STT) in a sidecar process.
- **Look**: the original palette, locked, with a new Conversation + Situation Board layout.

## Status

| Milestone | Scope | Status |
|---|---|---|
| M0 | Foundation: workspace, inherited crates pruned, persona v3, docs | Done — 654 tests green |
| M1 | Walking skeleton: model seam, Ollama and OpenRouter, lean loop, daemon and bus, minimal TUI | Done |
| M2 | Memory that knows: turn ingest, embed queue, snapshots, episodes, `import-legacy` | Done — LongMemEval held-out reproduced exactly (R@1 0.6725, R@5 0.8865, R@10 0.9039, n=229) |
| M3 | The Wake v1: boot sequence, spoken greeting, brief, watchers, first-run onboarding | Done |
| M4 | Crew: Job Objects, dispatch, durable runs, sandbox, playbooks | Done — 1,297 tests green; scripted end-to-end run on a debug daemon: research, build + merge, blocked push, resume after `taskkill /F` |
| M4.5 | Deep Research v2 + CUDA: keyless channels, main-content extraction, corpus fetcher, plan → explore → compress → corroborate → verify → synthesise → remember, GPU rerank cascade | Done on CPU — 1,622 tests green; extraction F1 0.793 → 0.894 on WCXB test (p95 43 ms); live v2 run (fe527eba): citation accuracy 0.897 strict / 0.949 half-credit, self-judged, and not good enough by hand (half-blind retrieval, one paper counted twice) — fixed on CPU, live re-run pending the GPU; GPU cascade R@3 0.8515 → 0.8908 at p95 50 ms. The 10-question research eval is pending a free GPU; ARM64 is compiled, not run |
| M5 | Sentinel II and dreaming: routines, arXiv, wrap-up, overnight queue | Planned |
| M6 | Full voice: `marlowe-voice.exe` sidecar with sherpa-onnx | Planned |
| M7 | Ship: perf budgets, installer, Windows ARM64 | Planned |

The ten foundation crates compile and their 654 tests pass, including the embedder and cross-encoder reference tests against the real pinned models.

## Layout

```
crates/    active workspace members (journal, memory, net, extract, tools,
           permission, mcp, contract, view, surface); parked crates
           (provider, openrouter, exec, marlowe bin) sit here but are excluded
legacy/    marlowe-loop, marlowe-daemon, marlowe-stub: reference only,
           not buildable dependencies
persona/   persona prompts (v3 core + capability blocks; v1/v2 history)
eval/      LongMemEval scoreboard and adapter harness
models/    embedding and cross-encoder models (git-ignored)
docs/      VISION.md, ARCHITECTURE.md, legacy addenda
```

## Building (once M1 lands)

```
cargo build --release
```

Models (about 193 MB: `jina-embeddings-v2-small-en` and the fine-tuned MiniLM cross-encoder) are not in git. On a development tree `scripts/fetch-models.ps1` copies them into `models/` (from a local `Marlowe_Harness`, else from Hugging Face) and sha256-verifies them. On any other machine the first Wake offers them ("Install memory LMs?"), and `marlowe models fetch` does the same from the command line; both download into `%LOCALAPPDATA%\MarloweCowork\models`.

Build at idle priority with `powershell -NoProfile -File scripts/cargo-idle.ps1 build --release -j 4`.

## New machine

Windows x64 or ARM64:

1. Clone the repo.
2. Install Rust with rustup (the native `aarch64-pc-windows-msvc` toolchain on ARM64); `rust-toolchain.toml` pins the version and adds both Windows targets.
3. Visual Studio Build Tools with "Desktop development with C++", plus the components "MSVC v143 ARM64 build tools" and "C++ Clang tools for Windows" on ARM64 (`ring` compiles C with `clang` there; `rusqlite`, `blake3` and `psm` need the MSVC tools). The first build also downloads ONNX Runtime (sha256-checked, cached) and needs the network.
4. Install Ollama for the local models.
5. `cargo build --release`, then `marlowe`. The first Wake offers the memory LMs (about 193 MB): they let Marlowe recall past conversations by meaning, and nothing downloads without a yes.

## Palette

The colour scheme is locked: accent `#9B7EDE`, Marlowe's voice `#b39ae8`, the semantic green, amber, red and blue, the three foreground weights, and the terminal's own background with no fills. `theme.rs` is kept verbatim. Layout and categories may change; colours may not.

## Further reading

- `docs/VISION.md`: the product.
- `docs/ARCHITECTURE.md`: threads, storage, crates, seams, milestones.
- `CHANGELOG.md`: what changed.
