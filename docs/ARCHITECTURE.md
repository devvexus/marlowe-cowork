# MarloweCowork: Architecture

Principle: keep the implementations, replace the seams. The memory engine, journal, net stack, extractors, permission layer and TUI renderers are inherited. The model seam, agent loop, daemon, crew and sentinel are new.

## Concurrency

`std` threads and `crossbeam-channel`. No tokio. Everything inherited blocks (hand-rolled rustls HTTP, rusqlite, ort, `std::process`, crossterm), and the workload is about 50 concurrent tasks. The daemon is a set of actor threads:

- **Store**: the only sqlite writer, plus a pool of readers.
- **Memory**: loads ORT lazily, unloads it when idle, runs the embed queue.
- **Supervisor**: holds the run table and per-provider concurrency limits, wraps each run in `catch_unwind` (the build keeps `panic=unwind`).
- **Run threads**: one per conversation or crew run; parallel tools via `thread::scope`.
- **Bus**: numbers events, gives each client a bounded queue, merges deltas, replays from a sequence number on reconnect.
- **Sentinel**: `select!`s over file events and timers.
- **Voice-sidecar manager**.

When a GPU job is running, crew runs prefer cloud slots so local models do not thrash against training.

## Storage

- **Operational state** lives in WAL-mode sqlite tables: sessions, turns, runs (small delta events), notices, schedules, model library.
- **The signed journal** remains the audit spine for memory events, with content hashes and a verified-through watermark.
- **BeliefStore snapshots** keep boot time flat regardless of history.

## Memory (marlowe-mind)

- **Capture.** Every turn is stored Global. A user statement becomes a Note at user trust (live at once). A user question or request becomes an Episode, which can be recalled but is never injected. `capture::classify` makes this call lexically from the first sentence that has a content word, and keeps "please remember ..." requests as statements. Assistant prose is an Episode that is never injected. Machine payloads (tool JSON, harness status lines) are skipped. The legacy import applies the same policy to the old conversation.
- **Scope.** A session's workspace is recorded in `profile/workspaces.json` and is not journalled. At injection, slots 2–3 add `WORKSPACE_BOOST` (1.0 logit, unmeasured) to same-project candidates. Rank 1 and the admit/abstain verdict stay the core's. An explicit `scope=Project` memory is admitted only in its own project.
- **Admission.** Injection is the shipped core unchanged: rerank, the frozen gate, and the margin operating point 1.165071 (from `precision-coverage-heldout-v1.json`). It admits top-3 or abstains. Two known gaps remain. A store with one candidate always abstains (NoRunnerUp). An admitted query injects all of top-3, irrelevant memories included.
- **Proposed fixes, not shipped.** `admission-floor-v1.json` proposes a rank-1 floor and a rank-2..3 band, measured on LongMemEval fit. Neither is licensed, so neither ships:
  - The floor's 0.90 precision rests on n = 1 (`floor_status = target_not_attained_with_support`).
  - The band (0.086) cannot fire together with the 1.165 margin gate.
  - A product floor needs a pointwise (query, fact) precision curve measured on short facts, pre-registered with a fit/held-out split.

## Crates

| Crate | Fate | Notes |
|---|---|---|
| `marlowe-journal` | copy verbatim | Add a verified-through watermark |
| `marlowe-net` | copy verbatim | rustls (ring); ARM64 needs the VS clang component |
| `marlowe-extract` | copy verbatim | HTML/PDF/Office extraction, used for paper ingestion |
| `marlowe-permission` | copy, then simplify | Scope/walk (symlink-safe), egress allowlist, taint rule; tiers collapse to allow/ask/deny + sandbox root |
| `marlowe-mcp` | copy verbatim | Stdio client; upgrade later |
| `marlowe-contract` | copy-trim | Delete `escalation.rs` |
| `marlowe-tools` | copy-trim | Raise `MAX_EXPOSED_TOOLS`; keep skills/frontmatter/pin; register new tools |
| `marlowe-memory` | copy-trim + extend | Delete `cue/dense/{cuda_libs,vram,hostmem}.rs`, `probe.rs`, `examples/`, gate v1-v4. Prune `retrieve.rs` (Cascade/probe paths, not a rewrite). Maturation set per trust class. Cache BM25 term stats. New: `episodes.rs`, `scope.rs`, `snapshot.rs`, `reflect.rs` |
| `marlowe-model` (new) | seam | Serde-only; see below |
| `marlowe-provider` | copy-trim, port | Keep `sse.rs`, `think.rs`, `wire.rs`, leaked-XML recovery and the Ollama adapter, ported to `Provider` (`/api/tags`, `/api/show` capabilities, `think` levels, streamed tool calls). Delete `hybrid.rs`, `llamacpp.rs`, `routing.rs`, `control_step` |
| `marlowe-openrouter` | copy-trim, port | Port to `Provider`. Add `GET /api/v1/models` catalog (pricing, context, supported params), `reasoning:{effort}`, `reasoning_details` round-trip, mid-stream error and last-chunk usage handling. Web search via the `openrouter:web_search` server tool, citations from `annotations` |
| `marlowe-brains` (new) | — | Model library in sqlite; provider registry (live: Ollama, OpenRouter; stubbed: GGUF, Anthropic, OpenAI); slots and effort; tool-call reliability probe (inherits `capability.rs`); keys in Windows Credential Manager, scrubbed from child-process environments |
| `marlowe-agent` (new, from `marlowe-loop`) | — | Copy in `context.rs` (tiers, compaction, prefix cache), `steer.rs`, `provenance.rs`, `Budget`, tool-host types. New lean loop (~800 lines): streaming, text alongside tool calls, parallel tools, inbox, cancel, compaction. Old engine, driver, durable, run, profile, escalation and upward files stay in `legacy/` |
| `marlowe-os` (new) | — | The only `unsafe`: Windows Job Objects (kill-on-close process trees), `CancelToken` (`TcpStream::try_clone` + `shutdown`), NVML GPU stats |
| `marlowe-exec` | copy-trim | `marlowe_loop::` becomes `marlowe_agent::`. Add PowerShell, `python`, `arxiv`, `web_search`, `paper` (fetch + extract), `git`. Every spawned process gets a Job Object and a scrubbed environment |
| `marlowe-crew` (new) | — | See Crew below |
| `marlowe-sentinel` (new) | — | Watchers (`notify`, git, process, Ollama store, arXiv, training logs + NVML), scheduler/routines, notice ranking, brief/wrap composer, dreaming jobs |
| `marlowe-voice` (new) | — | WinRT TTS module (in-daemon) + `marlowe-voice.exe` sidecar bin (sherpa-onnx + cpal) |
| `marlowe-view` / `marlowe-surface` | copy-trim + new | Keep theme, region, markdown, latex, meter, chrome, keys, clipboard, overlay, doctor, plus `amplitude.rs` and `frame_clock.rs` from the stub. Delete app, window, render, escalation, commands, cli, inspector. Build the new layout, views, Wake and palette |
| `marlowe-daemon` | rewrite | Actor threads and a multi-client JSON-lines bus over loopback + token (`auth.rs` kept). Vocabulary: Ask/Steer/Cancel/Resume/Watch/Approve/Replay |
| `marlowe` (bin) | copy-trim | Keep `launcher`, `adapter` (eval; CUDA stripped), `profile`, `keyburst`. New `main.rs`: `marlowe` (TUI, auto-spawns the daemon), `--launch`, `daemon`, `ask`, `doctor`, `import-legacy`, `--eval-adapter` |

Dropped: the embed spike, the stub crate, `runs/`, `tools/` fit scripts, 38 experiment model directories, `tract-onnx`, and the STATE/DECISIONS/ADR ledgers.

Kept: the two shipped models (`jina-embeddings-v2-small-en`, `ms-marco-MiniLM-L-2-v2-ft-session-j`, about 190 MB) in git-ignored `models/` with a fetch/copy script; `eval/` (LongMemEval scoreboard) plus a dataset fetch script, since `data/` is empty; `persona/`; `docs/legacy/` (terminal and persona addenda, the mockup).

Workspace dependencies: `ort = "=2.0.0-rc.10"` with `["download-binaries"]` only (CUDA removed; rc.10 ships a static aarch64-windows build). Add `crossbeam-channel`, `notify`, `windows`. Release profile gets `lto = "thin"`.

M1 state: every crate under `crates/` is a workspace member. `marlowe-provider` is the Ollama adapter only (hybrid/llama.cpp/routing deleted); `marlowe-exec` is `BuiltinTools: ToolHost`; the bin wires concrete providers and tools into `marlowe-daemon` through `DaemonDeps`. `legacy/` (old loop, daemon, stub, bin, exec) is reference only and excluded from the workspace.

Windows notes (M1): a `TcpStream::try_clone` does not share SO_RCVTIMEO set after the clone, so set timeouts on the handle that is read. `Command::spawn` inherits every inheritable handle, so the bin clears the inherit flag on its std handles before spawning the detached daemon (the one `unsafe` call in the bin).

## The marlowe-model seam

Serde-only crate. The old seam (`marlowe-loop/src/driver.rs`) baked in the PI design: `ModelStep::ToolCall` dropped text accompanying tool calls, `parse_step` kept only the first control tool, providers interpreted tool names, args were flat, and children ran recursively inside the parent. The new types:

```
ChatRequest { model, effort, max_output_tokens, messages, tools }
Completion  { text, thinking, tool_calls: [{ id, name, args: Value }], usage, stop }
StreamEvent { Text, Reasoning{tokens}, Retract, ToolCallStart, ToolArgsDelta, Usage }

trait Provider: Send + Sync {
    list_models();
    stream(&self, &req, &CancelToken, &mut dyn FnMut(StreamEvent));
}
```

Providers translate wire formats only; they never interpret tool names.

## Crew and sandbox model

`marlowe-crew` holds the run table, role cards, `dispatch`, durable delta checkpoints, auto-resume, the worktree sandbox, and the Deep Research and Build playbooks.

- `dispatch` returns a run id immediately; the run executes on its own thread, depth 1 in v1. Results land in the conductor's inbox.
- Runs checkpoint small delta events into sqlite and resume after a daemon restart.
- Each run gets a git worktree. Its push URL is disabled and credential helpers are stripped; only the daemon merges or pushes. After an untrusted read, shell calls need approval.
- Every spawned process is in a Job Object (kill-on-close, no orphans) with a scrubbed environment.
- Autonomy is free inside the sandbox. Merge, push, deletes outside the workspace, new network hosts and budget overrun require approval.
- Agent = role card (editable markdown) + model/effort slot + tool allowlist + budget and sandbox.

## Voice sidecar

`marlowe-voice.exe` (sherpa-onnx, Kokoro TTS, Moonshine STT, `cpal` WASAPI) is spawned lazily by the daemon and speaks stdio JSON lines. Separate process because it avoids ORT symbol clashes with the memory engine, isolates crashes, and keeps the daemon light. The daemon owns it, so alerts speak with no TUI attached. Until M6, a WinRT `SpeechSynthesizer` module runs in-daemon. Windows built-in speech recognition is excluded (needs MSIX identity).

## Milestones

Critical path: seam, run thread + cancel, bus, TUI, memory, dispatch, Sentinel, voice. Each milestone closes at a gate once the CPU is free: `cargo check`, `cargo test`, adversarial review.

| # | Milestone | Delivers | Done when |
|---|---|---|---|
| M0 | Foundation (I/O only) | Workspace, crates copied and pruned, `ort` CUDA removed, persona v3, CLAUDE.md, docs, models copied, `git init`. Compile-checked on its own before M1 | — |
| M1 | Walking skeleton | `marlowe-model`; Ollama + OpenRouter providers; model library and slots; lean loop; actor daemon and bus; minimal TUI in new layout; persistent sessions; PowerShell/python/web_search tools | Chat works on dusk-22b and an OpenRouter model |
| M2 | Memory that knows | Turn ingest with trust-class maturation; embed queue; pruned retrieve; snapshots; episodes; project scope; provenance; `import-legacy`; Memory view | A fact from session A is auto-injected in session B after a daemon restart. LongMemEval reproduces R@1 0.6725 / R@5 0.8865 exactly, plus a Profile-scope arm with P95 latency |
| M3 | The Wake v1 | Boot sequence; WinRT spoken greeting; brief composer; git, training-log and NVML watchers; Watch and Today rail; contradiction asides; live repo context | — |
| M4 | Crew | `marlowe-os` Job Objects and cancel; role cards; `dispatch`; durable runs; worktree sandbox and approvals; Deep Research + Build; Run view and steer; dispatches to memory | — |
| M5 | Sentinel II + dreaming | Routines/scheduler; arXiv; notices with quiet hours; end-of-day wrap; overnight queue; consolidation + reflection into Threads | — |
| M6 | Full voice | `marlowe-voice.exe`: Kokoro British voice, Moonshine push-to-talk, key barge-in, amplitude-driven presence | — |
| M7 | Ship | Perf budgets verified; installer script; Windows ARM64 (`aarch64-pc-windows-msvc`, VS clang + ARM64 tools, sherpa win-arm64 libs) | — |

## Verification

- Every milestone: `cargo check --workspace` and `cargo test --workspace`. Kept crates keep their tests (journal; memory claim/ingest/embedding_reference/cross_encoder_reference; provider no_tls; markdown/palette).
- Memory: run `eval/` LongMemEval through `--eval-adapter` for exact reproduction.
- End to end: fresh profile, "no models", add Ollama `marlowe-dusk` and an OpenRouter model, run the probe; chat; `taskkill /F` the daemon and check the session survives and an in-flight crew run resumes; ask "what did I tell you about X" and check provenance; run `import-legacy`.
- Crew: Deep Research yields a cited report and a memory write; Build yields a diff, a merge approval, and blocks a crew `git push`.
- Sentinel: append to a fake `trainer_state.json` and check NaN, plateau and finished notices; fast-forward the clock and check the brief.
- Voice: spoken greeting plays (WinRT, then Kokoro); push-to-talk round-trips.
- ARM64: `cargo check --target aarch64-pc-windows-msvc` early in M7, then a release build.

## Compute policy

No cargo, tests or inference until Matthew says the machine is free. After that, cargo runs with `-j2` to `-j4` at idle priority, one build at a time. Live model tests use OpenRouter while another job holds the GPU. The old Marlowe_Harness repo and profile are read-only.
