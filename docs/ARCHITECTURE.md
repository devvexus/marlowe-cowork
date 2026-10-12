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

**Model loads (`marlowe-brains::load`).** An unset slot runs the conductor's model (the first model added is only the conductor's until one is assigned). Every local load goes through `Brains::admit`: hosted or already loaded runs; a cold load is costed by the planner at `allowed_ctx` × the policy's agents against the GPU memory free beside the loaded runners. The user's own turn asks with that cost when it does not fit (once per model and resident set); a crew step is refused with it; background work (the brief's polish) never loads a model and runs only on a resident or hosted one, never while a turn or crew run is busy, and logs which model it used. Crew runs take `allowed_ctx` as their window and at most the policy's `slots` local runs at once.

## Storage

- **Operational state** lives in WAL-mode sqlite tables: sessions, turns, runs (small delta events), notices, schedules, model library.
- **The signed journal** remains the audit spine for memory events, with content hashes and a verified-through watermark.
- **BeliefStore snapshots** keep boot time flat regardless of history.

## Memory (marlowe-mind)

- **Capture.** Every turn is stored Global. A user statement becomes a Note at user trust (live at once). A user question or request becomes an Episode, which can be recalled but is never injected. `capture::classify` makes this call lexically from the first sentence that has a content word, and keeps "please remember ..." requests as statements. Assistant prose is an Episode that is never injected. Machine payloads (tool JSON, harness status lines) are skipped. The legacy import applies the same policy to the old conversation.
- **Episodes, no model (2026-10-10).** Memory never calls a generative model. An evicted session's episode is extractive (`marlowe-mind::episode`): the opening request as its title, the day and time span in words, the workspace, the user and assistant lines that carry most of the session's BM25 weight (session term frequency against the store's document frequency), and the top keywords. Retrieval answers "what were we doing on Tuesday?" from it. A later eviction supersedes the session's earlier episode. The Reflection slot is unused and hidden in the Library; its stored row is kept. Consolidation stays on its frozen non-LLM policy.
- **Scope.** A session's workspace is recorded in `profile/workspaces.json` and is not journalled. At injection, slots 2–3 add `WORKSPACE_BOOST` (1.0 logit, unmeasured) to same-project candidates. Rank 1 and the admit/abstain verdict stay the core's. An explicit `scope=Project` memory is admitted only in its own project.
- **Admission.** Injection is the shipped core unchanged: rerank, the frozen gate, and the margin operating point 1.165071 (from `precision-coverage-heldout-v1.json`). It admits top-3 or abstains. Two known gaps remain. A store with one candidate always abstains (NoRunnerUp). An admitted query injects all of top-3, irrelevant memories included.
- **GPU (M4.5, `--features cuda`, off by default).** The embedder is always CPU (belief vectors persist). The cross-encoder picks its provider at every load (`marlowe-memory::ep`): CUDA, then DirectML, then CPU, and only while the GPU is idle per the Sentinel (no other compute-only process, util < 10%, no live watched run). It re-checks every 30 s and moves back to CPU when the GPU gets busy. On a GPU it brings the six-graph cascade (`cascade.rs`: depth 30 narrowed to 10 by the shipped graph, then RRF over six Session-J-recipe fine-tunes); `rerank_score` stays the shipped logit, so the operating point is unchanged. x64 links pyke's ORT 1.28 CUDA 13 build. ARM64 has no prebuilt ORT CUDA build, so it loads the official ORT 1.31.0 at run time and registers Microsoft's CUDA plugin EP 0.3.0. `scripts/fetch-gpu-runtime.ps1` fetches the sha256-pinned runtime into `gpu-runtime\`. TF32 is off.
- **Proposed fixes, not shipped.** `admission-floor-v1.json` proposes a rank-1 floor and a rank-2..3 band, measured on LongMemEval fit. Neither is licensed, so neither ships:
  - The floor's 0.90 precision rests on n = 1 (`floor_status = target_not_attained_with_support`).
  - The band (0.086) cannot fire together with the 1.165 margin gate.
  - A product floor needs a pointwise (query, fact) precision curve measured on short facts, pre-registered with a fit/held-out split.
- **Profile (M2.6).** The fix for the young store is structural, not a threshold. `Mind::profile` renders every live Preference, Decision, Fact and Procedure that was written deliberately (remember, a correction, the harness), is not UntrustedContent, and is Global or in the current project. It is one line each, preference and decision first, newest first within a kind, capped at 1,500 tokens with an omitted count. The block is deterministic.
  - `MindMemory` sends it as its own cached block in the context tier, ahead of the retrieved block. Retrieval runs on the pool without the profile's ids, so the gate judges only what it can admit.
  - What is durable is the conductor's call when it uses `remember` (`persona/blocks/memory.md`). An untainted deliberate write is live at once (`MaturationPolicy::Deliberate`). A tainted write stays UntrustedContent, keeps the window and never enters the profile. Turns, imports and episodes stay with calibrated retrieval.

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

Workspace dependencies: `ort = "=2.0.0-rc.13"` (ONNX Runtime 1.28.0, static, DirectML linked on Windows; it was rc.10 until M4.5) with `["download-binaries"]`; GPU features come only through `marlowe-memory/cuda` and the `marlowe-ort-gpu` shim. Add `crossbeam-channel`, `notify`, `windows`. Release profile gets `lto = "thin"`.

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

## The M4.6 seam

Parallel tracks (design `docs/research/m46-design.md` sections 7-8) plug in without editing a shared `match`:

- **Tools** are rows in `marlowe-exec/src/specs/<group>.rs` (`ToolDef`: spec, manifest, call, consequence, crew consequence, describe, taint, egress, mutation). Each group file also declares `middleware`, `persona_block`, `configure` and `detect`. `BuiltinTools` only concatenates `specs::GROUPS` and looks calls up by name; per-host tool state lives in `BuiltinTools::state::<T>()`. A host's tool set is fixed per session (`with_model_exposure`: every group for frontier models, core plus detected for local ones); calls outside it are refused. `marlowe-tools` holds the exposure vocabulary (`ToolGroup`, `Profiles`, `select_groups`).
- **Loop hooks** (`marlowe-agent/src/middleware.rs`): `ToolMiddleware{before_turn, before_approval, before_call, pre_mutation, after_call, after_step, before_final, before_compact}` and `TurnObserver{on_session_start, on_notification, on_model_call, on_tools_done}`. Only `before_approval` may rewrite a call's arguments, so the gate, taint and egress judge the call that runs. Calls get a `CallCtx` (the `ToolCtx` plus a `SessionScope`: `Conversation::ext` lent for the batch, and daemon `Services`). Middleware comes from the tool host (`ToolHost::middleware`, seeded by `Agent::new`) and from `marlowe-daemon/src/stack.rs`, which concatenates one constructor per track; daemon-lifetime state goes in `Core::ext`, start hooks in `boot.rs`. Tool groups' persona blocks reach the prompt through `ToolHost::persona_blocks`, only while exposed.
- **Children** start only through `marlowe_os::child` (scrub list `SCRUBBED_ENV`, per-launch `env_allow`, Job Object); `marlowe-os/tests/spawn_audit.rs` allowlists the few remaining direct spawns with their owners. `marlowe_os::workspace` has the `Workspace` port (`LocalFs`; `DockerExec` later), which the shell tools run through (`exec`; `spawn` for background jobs), and `WorktreeProvider`. The Sentinel ticks registered probes (`marlowe-sentinel/src/probe.rs`: CI, experiments).
- **Wire**: new `Request`/`Event` variants (types in `marlowe-proto/src/coding.rs`) are routed in `marlowe-daemon/src/client.rs` to stub `*_ops.rs` files, one per owning track. `Part::Image`, `ToolResult::parts` (`ResultPart`) and `ModelInfo::supports_vision` are additive; pre-M4.6 turns and library rows load unchanged (fixtures in `marlowe-model/tests/fixtures`).
- Skeleton crates `marlowe-code`, `marlowe-verify`, `marlowe-settings`, `marlowe-anthropic`; `marlowe bench|export|mcp|eval` stubs in the binary.
- **Wave 2 (seam2).** Same rules again. Three more tool groups (`workflow`, `tasks`, `notify`, files `specs/{workflow,tasks,notify}.rs`) and wire types in `marlowe-proto/src/background.rs`, routed to `task_ops.rs`/`notify_ops.rs` (N), `workflow_ops.rs` (W) and `f3_ops.rs` (F3). The background **`TaskRegistry`** (`marlowe-agent/src/tasks.rs`) is one table for jobs, agents, workflows, monitors and crons (typed ids `job_`, `agent_`, `wf_`, `mon_`, `cron_`): owners `register` a task with a stop/output handle, or add a `TaskSource` that lists tasks living elsewhere (B's job table, the crew's runs) so nothing is mirrored. News goes in with `inject` and is drained at a step boundary (at most one batch per step; the waker starts a turn on an idle session); injections from other sessions are `untrusted` with lineage, never user trust. The daemon holds one registry, installs it as the `Tasks` service of every session and broadcasts `TaskUpdated`. Workflow runs are crew runs carrying `Run.workflow`; the engine crate `marlowe-workflow` (Starlark, W) is a member and a dependency of the crew and the daemon already, so W edits no shared manifest.
- **Workflows (W).** Starlark scripts, no coroutines: an unfinished `agent()` unwinds the pass and the script is rerun from the top on a fresh heap when agents finish (calls are identified by their position in the script, `wf:<pos>:<hash>`). That one mechanism gives concurrency (`parallel`/`pipeline` catch the unwind per thunk/item), deterministic control flow and resume (journal = seeded results; `resume_from` keeps the longest unchanged prefix in script order). Host readings (`budget.*`) are memoised per position so passes agree. The engine never runs an agent: `AgentHost::start` hands one to the crew (`playbook/script/`), one crew step per agent, its own cancel token, `structured_output` added to a schema agent's tool host. Scripts, approved digests (`<home>\workflows\trusted.json`), journals and findings live under the profile home. Deviations from 3.11: Starlark instead of JS (9.0); trust is per user, not in `settings.local.toml`; foreground (`background:false`) waits inside the tool call with Esc detaching. Bounds: nothing runs before approval (the check only resolves names); statements, comprehension iterations and lambda calls share one count per pass, with a heap ceiling and a time limit checked at each; `*`/`+`/`<<`/`range` and prompts are size-checked. Not bounded: one built-in call over deliberately aliased data (`str` of a list holding one 16 MB string a million times); that needs an approved, hostile script. A workflow run's worktree is a snapshot of the working tree and merges back into it uncommitted.
- **Wave 2 (N): background work and the daemon's lifetime.** Tool results are never the only way news reaches the model: `Tasks::inject` queues news per session, `DrainMiddleware` (`after_step`, `before_final`) hands it over as one user-role message at a step boundary, and `Waker` starts a relay turn (`[crew · run background · <sources>]`, never ingested at user trust) on an idle session. Schedules are rows of `schedules` (durable) or memory (session) fired by one thread (`tick_at(now)` for a fake clock). Everything outward (toast, tray, voice line, file opener, model unload) sits behind a trait with a fake; no test shows anything. **Closing the window means everything stops:** `WindowClosed` or the last `tui` client vanishing without a `Detach` begins a close, which `Daemon::wait` carries out in order: cancel in-flight work, unload models and the voice sidecar, save (crew runs left `paused` with an `interrupted` note, offered for resume at the next launch), exit. Running on is an explicit opt-in (quit dialog or `/keep-running`) and is never invisible: tray icon plus a toast with `[Stop now]`.

## Coding eval (track L)

`eval/coding/` is the harness (Python, standard library, `marlowe eval ...`); `marlowe bench` is the model report card (`crates/marlowe/src/bench/`). Both harnesses speak Claude Code's stream-json, so one reader scores a run (steps, tokens, tool errors, repairs from `MARLOWE_EVAL_STATS`) and one driver runs the interactive suite. The head-to-head runs the VS Code extension's `claude.exe` isolated on the same local model (`mcoding/cc.py`, `cc-dry-run` shows the flags). Tasks are graded by hidden tests installed after the agent finishes; `own` tasks are past fixes of this repository put back. Nothing here loads a model unless Matthew runs it in a GPU window.
- **Wave 2 (H3): the terminal client.** `marlowe-tui/src/bg.rs` holds what the daemon's wave-2 events say (tasks, workflow runs, findings, sent files, plans, `/fast`, message threads); panels are `Overlay::Panel` plus a `Panel` variant (`bg_panels.rs`) and draw in `render/bg.rs`. Background news is told from the user's words by its first line (`bg::news_label`), so a monitor line never becomes `you`. The quit modal (`closing.rs`) and the daemon's `keep running` switch are one choice: the setting `quit.choice` is the client's half, the switch is the window-close handler's. Reports are read from disk by `Effect`s (`report.rs`).

## M4.6 coding surface (wave 1)

- **Permissions.** One `Permissions` service per session decides every call: hard denies, deny rules, plan mode, tripwires (shell asks, new hosts, taint, a tool's `AskEvery`), ask rules, allow rules, then the mode. A project widens permissions only by trusted digest. Without the service (crew, tests) the tool's consequence decides and a session allow is keyed on the tool name, so it never frees an `AskEvery`.
- **Running calls.** Reads in one message run in parallel; calls that write (by manifest) run on one worker in release order.
- **Turn inputs.** An `Ask` carries images (joined to the user's message from `<home>/blobs`, the store `read`, `PutBlob` and the providers share) and `marlowe -p`'s options (session allow/deny rules, mode, step and cost caps, an appended system prompt). The code group's middleware puts the repo map into the prompt sections once per session.
- **Files.** `marlowe-code` holds the edit engine, FileState, notebooks and `edit_lines`; `specs/files.rs` is the tool surface, its schemas pinned in `specs/files/specs_m46_files.json`.
- **Jobs.** Every shell call is a job with a log under `<home>\jobs`; `marlowe_os::job::bg_hub` is the process-wide registry. Job notices reach the model between steps; an idle session is woken by a relay turn (`job_ops`, started at boot). A relay never loads a model: one that cannot run is deferred (crew runs) or held by the session until the user's next turn.
- **Code intel.** `marlowe-code` tags index (`<home>\index`, per root) plus an LSP manager shared per profile home (2 live, 10 min idle, `MARLOWE_LSP=off`). Code tools are name-based and offered only with a workspace; post-edit diagnostics are `specs/code` `DiagnosticsMiddleware`. Servers start trusted only for the free-write workspace in an untainted session; a tainted session gets none, and server text from outside the user's folders is shown as location and signature only. Crew hosts get the index but no servers.
- **Verify.** `marlowe-verify` detects project profiles and runs gates, services, `http_check` and the Edge `browser`; the browser's reach is enforced on every request. Logs, screenshots and blobs live under the profile home.
- **Git.** `marlowe-exec/src/git`: the PR plan is written into the call's args by `git::guard` before the gate; `<home>\ci_watch.json` is the push ledger (exec writes it, the Sentinel's CI probe reads it); `Request::OpenPr` asks once and reuses `gh` through the tool factory, so `marlowe-daemon` has no git code.
- **Sessions.** Transcript rows per model call and tool batch (`turns`, `call_results`, `rewound_turns`). Checkpoints are a shadow bare repo per workspace (`<home>\checkpoints\<name>-<hash>.git`, refs `pre`, `post`, `undo`, `rewound`, `top.txt`); a rewind leaves foreign edits alone and retracts the rewound turn's beliefs (`Retracted` journal event).
- **Headless and MCP.** `marlowe -p` drives the daemon (or `--standalone`, a private library copy with no memory); `marlowe-mcp` is client (stdio, streamable HTTP, OAuth 2.1) and server (`marlowe mcp`); MCP tools beyond the window's share defer behind `tool_search`.
- **Editor.** `ide/vscode` serves MCP on 127.0.0.1 behind a bearer token and writes `<home>\ide\<port>.lock`; `marlowe-mcp::ide` turns the lock of the window that has the workspace open (the workspace is its folder or inside it, never the reverse) into the session server `ide` (`mcp__ide__*`); the server's HTTP read timeout outlasts its tool timeout, and its in-flight calls are keyed by `Mcp-Session-Id` plus request id. `/ide` and `marlowe doctor` report the connection. The lock cannot name another host or lower a tool's consequence; `execute_code` asks every call. MCP images become blobs and `Part::Image`.
- **Export.** `marlowe export` (opt-in per project) writes SFT and step-DPO JSONL through a layered scrubber; nothing is uploaded.

## M4.6 conductor completions (F3)

- **Subagents.** `agent` runs in the background by default and takes `model`, `effort` (`xhigh` is `Max`) and `name`; `send_message` addresses a name or id, steering a running agent at its next step or resuming a finished one. Every subagent has `handback{message}` (its result verbatim; it ends the run; a final text without it also works and is counted). `type:"fork"` starts from the parent's last request byte for byte: the loop leaves a `ForkBase` in the conversation's extensions for a step that forks, the child's `ForkPrefix` goes first in `assemble_with`, and the tool list stays the parent's (nested agents are refused by `ForkGuard`, not by omission) so the prefix cache holds. `guide`, `statusline` and `agent-creator` are built-in types from `roles/*.md`; their extra tools (`set_statusline`, `create_agent`) belong to the subagent's tool view (`subagent_tools.rs`), not the tool table.
- **Plan file.** `enter_plan` (asks) switches to plan mode; `exit_plan` writes `<home>\plans\<session>-<slug>.md`, publishes `PlanReady` and waits for `AnswerPlan`; the file is read again after the answer, so the user's edit is what is pinned. Approval names `accept_edits` or `default`, never wider.
- **Task list.** `TaskLists` (a service over the `daemon_kv` key `tasklist.<session>`) is the one list of a conversation, shared by the conductor, subagents and workflow agents; ids are never reused and an item cannot start while a blocker is open. The conductor's `TodoState` mirrors it (`TaskListSync`); a subagent's own `todo{items}` stays private.
- **Directives.** `ultrathink` and `+500k` are parsed by `DirectiveMiddleware`. The `TokenPool` service counts the output tokens of the loop, its subagents and workflows, and the conductor is kept going until the target is within 10%.
- **`/fast`** wraps the `/model` switch: the `Quick` slot's model while on, the session's own after.
- **Tool deferral (T31) and the 32k prefix (A15).** On a window of 40k or less the host lists the core tools (`read write edit glob grep bash project todo ask_user skill agent exit_plan`) and `tool_search`; the rest of the built-ins (`marlowe-exec/src/deferral.rs`: `git`/`gh`, the code-intelligence tools, `multi_edit`, `powershell`, `python`, job tools, web and research, services, background work, workflows) load by `select:name` or keywords and are appended at the tail of the tool list, so the bytes before them stay fixed. A group whose tools are all deferred keeps its persona block out of the prefix; the block arrives with the first load. The MCP host merges its own `tool_search` with this one. With the persona condensed (v3 about 1.6k tokens) the dusk prefix in a git/Cargo repository is persona+blocks about 2.3k, tools 3.0k (core, memory, dispatch), 8.5k fitted of 9k; `marlowe-daemon`'s `the_wired_dusk_prefix_fits_its_slices_in_a_repository` pins it.

## M4.6 integration (wave 2)

- **Bash.** `marlowe_os::bash::git_bash` is the one lookup for every bash the harness runs (tool, jobs, monitors, hooks, `!cmd`, doctor): `MARLOWE_BASH`, the Git for Windows roots, a Git found through `git.exe` on PATH; never the WSL launcher.
- **Web.** `web_fetch` stops at a redirect to another host (`CorpusConfig::same_host_redirects_only`), keeps a 15-minute process-wide page cache, and answers `prompt` through the session's `WebDigest` service (`web_ops.rs`: a hosted or loaded model only). `web_search` filters by domain after the backend.
- **`marlowe mcp serve`.** Builtin tools for one workspace behind `Permissions` in `dont_ask`; refusals carry the reason; calls are logged at agent trust without arguments. Nothing reaches memory.
- **One `+500k` pool.** `marlowe_workflow::Budget` can be linked (`PoolLink`) to the conductor's `TokenPool`; the daemon links each session's pool on creation.
- **Inventory.** `crates/marlowe/tests/parity_inventory.rs` maps every name in the recorded Claude Code init (`tools/record_cc_init.py`) and diffs the documented hook events; re-record when the pinned version moves.

## Crew and sandbox model

An agent is a role card (`roles/*.md`, overridden by `<home>\roles\*.md`: slot, effort, tool allowlist, budget, sandbox kind, instructions), and that is all. Depth 1: crew runs never get `dispatch`.

- **Dispatch.** The conductor's `dispatch` tool (exposed with `persona/blocks/crew.md` only while the supervisor runs) returns `{run_id, role, playbook, estimate}` at once. After untrusted content it asks, and the run starts tainted. The `Supervisor` queues the run and runs it on its own thread behind `catch_unwind`. Limits are 2 runs per cloud provider and 1 per local one.
  - Every step resolves its own slot. A busy GPU moves a local step to a cloud slot or holds it at "waiting for the GPU". Busy means a watched run is alive, another process computes on the GPU (NVML compute list minus graphics list, not Ollama), or utilisation is at least 60% with no Ollama runner (a game). Desktop graphics load alone (about 20% here) is not busy. At most one local model stream runs at a time across all runs.
  - Each model call is admitted against the run budget, counting calls still in flight, so parallel scouts cannot spend past it before the approval. A user's cancel outranks pause and shutdown.
- **Durable runs.** `crew_runs` and `crew_run_steps` live in `cowork.db`, with one checkpoint row per finished playbook step (its messages and output). At boot, queued and running runs are requeued and skip completed step keys. A step that was cut off mid-way reruns from its start.
- **Research sources** (`marlowe-research`). There are 11 keyless API channels plus web search (SearXNG on loopback, then DuckDuckGo lite, then a hosted search the daemon supplies), behind one `ChannelRegistry` that has no credential field.
  - Every request goes through a process-wide per-channel limiter (`Retry-After` holds it) and the on-disk cache. robots.txt applies to page fetches, feeds and DuckDuckGo. A CAPTCHA is a refusal: the backend rests and nothing tries to get around it.
  - Dedup merges an identifier only on its authority's word. Identifiers that only indexes agree on are confirmed with `ChannelRegistry::lookup`.
- **Research in the product (M4.5).** The binary builds deep research's I/O in `src/research_wire.rs`: the keyless registry, `NetHttp`, `<home>\cache\research` and the corpus fetcher (`marlowe-exec::corpus`: robots.txt, per-host spacing, main-content markdown, Edge for client-rendered pages). `<home>\research.json` sets SearXNG URL, DuckDuckGo, hosted search, auto-approve threshold (off), fetch concurrency, feeds, Wikipedia language and Stack Exchange site; it has no credential field. A plan waiting for approval is re-sent on `WatchRun`. The TUI's Run view shows it (`a` approves, `x` rejects) and `marlowe crew plan <run>` shows, narrows (`approve --breadth/--depth/--sources`), edits or rejects it. A finished run writes `report.md`, `research.json` and `bundle.json` (the research eval's input). Extraction picks rs-trafilatura, then rust-trafilatura, then html.rs per page (`main_content::Selector`), mapped back onto html.rs blocks so the markdown keeps links for citation anchors.
- **Playbooks** are deterministic Rust (`playbook/`); every step is one agent turn with a role. `deep_research` v2 (with `CrewSetup.research`) runs plan (approved by the user, with a cost quote) → explore by selection → compress to quoted claims → corroborate over independent roots → verify (required; else a refusal) → synthesise with every cited sentence re-checked → remember (a v2 step that fails or is cut off at the output limit runs once more as `<key>:again` with thinking off); without channels it runs v1 (plan, parallel scouts, a verify pass, synthesis into `report.md`, one Result memory). `build` runs plan, implement, a test loop, then review, and ends with a `DiffSummary` in NeedsYou. Other roles get a single step.
- **Sandbox.** A git workspace gets `git worktree add <home>\worktrees\<id> -b crew/<id>`; anything else gets `<home>\scratch\<id>`. File-tool writes are free inside and ask outside. Reads are allowed anywhere, and reads inside the sandbox or the original workspace do not taint. The sandbox is trusted explicitly (`BuiltinTools::new_sandbox`), since the profile sits under app data.
  - Children run in `marlowe-os` kill-on-close Job Objects with API keys scrubbed. `GIT_CONFIG_*` clears and replaces every remote's `pushurl` with an invalid URL (git ≥ 2.46), adds an empty-prefix `pushInsteadOf`, and empties `credential.helper`.
  - Crew shells are not OS-confined. The cwd is pinned to the sandbox, but a child can write wherever the user can. A child that clears its environment, or runs from a script, reaches the user's remotes and credential helper. **The push block is best-effort.** Real confinement (AppContainer or a restricted token) is not built.
  - The shell policy (`shell_policy.rs`) is a lexical tripwire that over-asks. Approval is needed for `git push`, for git writes outside the worktree, and for changes to what a worktree shares with the checkout: config, `update-ref`, branch delete/rename/force, `tag -d`, `stash drop|clear`, `remote` and `worktree` changes.
  - It also covers deletes, copies, moves and redirects outside the sandbox; commands that mention git's environment; python code and interpreter scripts that mention `git … push`; and any shell command after an untrusted read. A budget overrun asks too.
  - A worktree shares `.git/config` and hooks with the checkout. The sandbox stores a sha256 of the repo config (minus `branch.*` and `user.*`) and the hooks at creation. Diff and merge refuse once that digest changes. The daemon's own git calls run with `core.hooksPath=/dev/null`, `core.fsmonitor=false`, `--no-ext-diff --no-textconv` and the no-push environment.
- **Results.** A finished run's `Dispatch` is relayed into its parent session: as a relay turn when the session is idle, at the next step when it is busy and the result is untrusted-free, otherwise after the current turn. A run that read untrusted content relays tainted. `Run.relay_due` stays set until the relay is in the session's saved history, and the daemon relays every run still due at boot. Only the daemon merges (fast-forward, else a merge commit), and only after `crew_merge` is approved. It never pushes. For a research run, "merge" copies the report to `<workspace>\research\<slug>.md`.

## Sentinel and the Wake (M3)

- **Read-only by construction.** `marlowe-sentinel` never writes, locks, signals or renices anything under a watched root. State (roots, per-file offsets, last metrics, family bests, run status, git heads, notices) lives only in `cowork.db`. Reads are short, use full sharing, and are capped at 256 KiB per tick. Git uses `status --porcelain`/`log -n` with `GIT_OPTIONAL_LOCKS=0` and a 5 s timeout. Ollama is `/api/tags` only. NVML is a query, and absent NVML is "unavailable", not an error. A test snapshots a fixture root (including `.git`) before and after and requires it to be identical.
- **Threads.** The engine thread owns file and run state, driven by `notify` (750 ms debounce) plus a 15 s poll. Probes (git 60 s, Ollama 120 s, GPU 15 s) run on their own thread so the engine never blocks. The first scan of a root is a silent baseline.
- **Notices.** Deduped by (source, subject, kind), re-broadcast only on escalation or a changed detail after 30 min, ranked by severity then age, with seen marks per client. Quiet hours (23:00–07:00) suppress speech, not recording. Roots are added only by consent (`marlowe watch add`, or a key press in the TUI Watch view). Discovery returns candidates and never adds them.
- **Brief.** `brief::compose` is pure and deterministic: a greeting, then finished, needs-you, running, models, git and memory, capped at 5 sentences. The tui client gets it on Hello after 2 h away or on the first session of the day, and it is stored as the day session's first assistant message. One optional polish call (Quick, falling back to Conductor; 6 s) is accepted only if it adds no new number and stays in the persona's register; otherwise the deterministic text is used.
- **Voice.** An in-daemon WinRT actor synthesizes to a buffer, computes a 30 Hz RMS envelope for the TUI presence meter, and plays through MediaPlayer. Any key barges in. It speaks the brief and urgent notices outside quiet hours, at most one every 20 s.

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
| M4.5 | Deep Research v2 + CUDA | `marlowe-research` keyless channels, identity dedup, independent-root corroboration; main-content extraction (rs-trafilatura, rust-trafilatura, html.rs); corpus fetcher with robots.txt and Edge escalation; seven-stage v2 playbook with plan approval and required verification; `cuda` feature with the GPU rerank cascade | Extraction passes the WCXB bar (met); a live v2 run reaches citation accuracy >= 0.95 (**not met**: fe527eba read 0.897 strict, 0.949 half-credit, self-judged; re-run pending GPU); the 10-question research eval is scored (pending GPU) |
| M5 | Sentinel II + dreaming | Routines/scheduler; arXiv; notices with quiet hours; end-of-day wrap; overnight queue; consolidation + reflection into Threads | — |
| M6 | Full voice | `marlowe-voice.exe`: Kokoro British voice, Moonshine push-to-talk, key barge-in, amplitude-driven presence | — |
| M7 | Ship | Perf budgets verified; installer script; Windows ARM64 (`aarch64-pc-windows-msvc`, VS clang + ARM64 tools, sherpa win-arm64 libs) | — |

## Verification

- Every milestone: `cargo check --workspace` and `cargo test --workspace`. Kept crates keep their tests (journal; memory claim/ingest/embedding_reference/cross_encoder_reference; provider no_tls; markdown/palette).
- Memory: run `eval/` LongMemEval through `--eval-adapter` for exact reproduction.
- End to end: fresh profile, "no models", add Ollama `marlowe-dusk` and an OpenRouter model, run the probe; chat; `taskkill /F` the daemon and check the session survives and an in-flight crew run resumes; ask "what did I tell you about X" and check provenance; run `import-legacy`.
- Crew: Deep Research yields a cited report and a memory write; Build yields a diff, a merge approval, and asks before a crew `git push` it can see (the block is best-effort; see Crew and sandbox model).
- Sentinel: append to a fake `trainer_state.json` and check NaN, plateau and finished notices; fast-forward the clock and check the brief.
- Voice: spoken greeting plays (WinRT, then Kokoro); push-to-talk round-trips.
- ARM64: `cargo check --target aarch64-pc-windows-msvc` early in M7, then a release build.

## Compute policy

No cargo, tests or inference until Matthew says the machine is free. After that, cargo runs with `-j2` to `-j4` at idle priority, one build at a time. Live model tests use OpenRouter while another job holds the GPU. The old Marlowe_Harness repo and profile are read-only.
