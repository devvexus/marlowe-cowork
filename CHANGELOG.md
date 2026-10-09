# Changelog

All notable changes are recorded here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Security
- Taint gate: after a web_fetch, web_search or a read outside the workspace, web_search and web_fetch to a host not approved this session ask (`Exposure` on `Conversation`, `ToolHost::consequence_exposed`/`taints`/`egress_host`). A reloaded session recomputes it from history.
- marlowe-net refuses loopback, private, link-local and CGNAT addresses after DNS resolution.
- A workspace that is a drive root, the home directory (or above) or a system/app-data directory frees no writes; the bin no longer turns a missing workspace into the daemon's cwd, and neither `ask` nor the TUI defaults to a too-broad cwd.
- Approval modal: Deny is the default; decision keys within 400 ms of it opening are ignored; long detail scrolls, and allow is disabled until all of it has been shown. Shell summaries show `(+N lines)`.

### Fixed
- A reconnect with a replay gap gets Welcome and the gap error only (the TUI reloads); the client queue is at least `replay_ring + 2`, so a full replay always fits.
- Two concurrent loads of one session could start two runners; registration is now one locked check-and-insert.
- Idle session runners exit after `session_idle` (30 min) and reload from the store on the next ask; finished runner handles are pruned.
- `ask` and the TUI adopt only a seq-0 `SessionOpened`, never another client's broadcast or `TurnStarted`.
- web_fetch honours interrupt and has a 60 s total deadline.
- OpenRouter: parallel tool calls streamed at one index with distinct ids stay separate calls.

## M2.5 Memory that listens — integration — 2026-10-09

### Changed
- Capture policy: a user turn that asks or requests (trailing `?` on its first sentence with a content word; a leading wh-word, auxiliary or imperative verb after greetings; "I want you to"; filler such as "ok", "got it", "I guess right") is stored as an Episode, which can be recalled but is never auto-injected. Facts are kept when a greeting sentence comes first ("Hi! I'm Matthew."), when the user asks Marlowe to remember or note something ("Please remember that ...", "Can you note that ...?"), for short first-person turns ("I'm vegan"), for `when`/`where` clauses ("When I train I use bf16") and for a noun or name head before `is`/`was` ("Will is my manager", "Plan is to ship Friday"). A declarative statement stays a Note and can be injected. Turns are stored Global. The session's workspace is kept in `profile/workspaces.json` and gives a soft boost: rank 1 is still the core's judged memory, and slots 2–3 add `WORKSPACE_BOOST` (1.0 logit, unmeasured) to same-project candidates. Explicit `scope=Project` memories stay filtered to their project. The eval adapter and the core ranking are unchanged.
- `import_legacy` also imports the old conversation from `checkpointed` `state.volatile[]` history blocks. Each distinct text is imported once, dated by the first event that carried it and filed under that event's session. User text is imported at user trust (statement → Note, request → Episode). Assistant text is imported as an Episode. Tool-result JSON, harness status lines and skill banners are skipped. Provenance is "imported from Marlowe_Harness (conversation)". `ImportReport` gains `conversation_*` counts (serde-default). The import marker is now v2, so a v1 import is offered again. `Journal::replay_meta` adds `ts` and `session_id` to replay.
- `marlowe memory import` also prints the `conversation_*` breakdown.

### Added
- `tools/admission_study.py` and `crates/marlowe-memory/artifacts/admission-floor-v1.json` (measurement only; `docs/research/admission-floor.md`). On LongMemEval fit the rank-1 logit never reaches 0.90 precision with support. The frozen floor -5.0551 rests on n = 1 (`floor_status = target_not_attained_with_support`). The band is 0.086 (17/21 on fit, 0/4 held-out). The artifact is not read by any code.
- `marlowe-mind/examples/legacy_dry_run.rs` (imports into a throwaway home) and `examples/seed_turns.rs` (ingests user turns through `Mind::ingest_turn`, so `memory probe` can be tried without a model).

### Not shipped
- The product admission rule (rank-1 floor for a one-candidate store, rank-2..3 band) is not shipped. The floor is unlicensed. The band cannot fire together with the 1.165071 margin gate, so it would cut every admission to top-1. As a result:
  - A store with one candidate still always abstains (NoRunnerUp).
  - An admission still injects all of top-3.
- In the small-store diagnostic (adapter, debug build), "what precision do I train in?" against "my training cluster is called halcyon and I use bf16" scores logit -7.80. Unrelated pairs reach -7.34 in the admission probe. No floor on this cross-encoder separates them.
- Small-store scenarios, run with `memory probe` on a debug daemon:
  - One statement: both the bf16 question and the quantum prompt abstain ("only one candidate").
  - Three statements: the bf16 question and the quantum prompt both abstain on margin. "what is my training cluster called?" injects the right memory at rank 1, and the two unrelated ones with it.

### Verified
- `cargo check --workspace --all-targets` is clean (only the pre-existing `marlowe-memory` test-name lints).
- `cargo test --workspace --no-fail-fast`: 1051 passed, 0 failed, 3 ignored (network/live). `embedding_reference` and `cross_encoder_reference` ran against the models and passed.
- The old profile, imported from a copy into a temp home (debug build), gave: read 410, imported 390, conversation read 408 / imported 388 / yours 176 (requests 148) / assistant 212 / skipped 20. Re-importing it imported 0. The old directory's 12 files are sha256-identical before and after.

## M2 Total Recall — integration — 2026-10-09

### Added
- `marlowe-mind`: the memory service. One actor owns the profile (journal, beliefs, vectors). The embedder and reranker load lazily and unload after 10 min idle. A coalescing embed queue fills vectors in the background. A sealed BeliefStore snapshot means boot derives only the journal tail. With models missing it runs degraded (lexical recall only, no auto-injection) and says so. `import_legacy` copies the old profile, verifies its chain under the old key, and replays memories with their original dates and trust. It is idempotent and never writes to the old directory.
- Injection: `select_for_injection_filtered` with the dense query vector, cross-encoder rerank at `RERANK_BUDGET`, the frozen gate and the declared operating point, filtered to global plus current-workspace beliefs. Every injection is journalled (`MemoryInjected`).
- Daemon: a `MindMemory` hook (inject before a turn, emitting `Recalled`; user and assistant turns ingested). Memory tools `remember`/`recall`/`forget`/`correct` are composed with the session tools. Memory requests, the `LegacyAvailable` offer after Welcome, and an episode summary when an idle session is evicted.
- Proto: `MemorySearch|Forget|Correct|Stats|Probe`, `ImportLegacy`, `LegacyStatus`. Matching events, including `Recalled` and `MemoryProbed`.
- CLI: `marlowe memory search|stats|probe|forget|correct|import|bench`. `ask` prints the dim `⋯ recall · N memories` line. `marlowe --eval-adapter` is restored, with `tools/score_longmemeval.py` and `tools/recall_at_k.py`.
- TUI: the Memory view (search, forget and correct with confirmation), the recall line, the first-run import offer, and palette entries.
- `marlowe-mind/examples/synthetic_legacy.rs`: builds an old-format profile for import and latency tests.

### Changed
- `marlowe-memory` (retrieval numerics unchanged): maturation by trust class on the product path. User-asserted beliefs are live at once; agent, web and untrusted lineage keep the 6 h window, and the eval `ingest()` path stays `Uniform`. BM25 term statistics are cached, bit-identical to the old path (tested). Added kind, scope, speaker and source tags, and a provenance accessor. Assistant prose is never auto-injected. Tombstoned memories drop their vectors.

### Security
- An auto-injected memory with untrusted lineage now taints the run, the same way an untrusted `recall` already did. Before this, a later `remember` or `correct` was written as inferred, and the post-untrusted-read approvals were skipped. `MemoryHook::before_turn` returns a `Recollection` (blocks plus an `untrusted` flag), and `Injection.floor` holds the lowest trust among the injected memories.
- `forget` follows the same trust rule as `correct`. `Mind::forget` takes a `Provenance` and refuses a caller whose trust is below the memory's. When the conductor tries to forget a memory that outranks the run, it asks the user first, and runs with the user's authority if allowed. `MemoryForget` from a client counts as the user.
- An episode summary of a tainted session is stored as untrusted (`Mind::write_episode(.., tainted)`).
- Session taint is stored in `sessions.tainted`, migrated in place, and set again on reload. Taint from an untrusted `recall` or an injected memory no longer disappears after idle eviction or a daemon restart.
- Forgetting writes the snapshot and vector file again straight away, so neither file keeps the forgotten text. The pre-release embedding cache (`<home>\cache\embeddings`), which stored query vectors and vectors of forgotten text, is no longer used and is deleted on open.

### Fixed
- Auto-injection no longer scores candidates that have no vector as dense 0. Up to 64 missing vectors are embedded before an injection. With more missing, injection abstains with `memory index warming (k of n embedded)`.
- Belief vectors persist in a sealed `beliefs.vectors` file next to the snapshot, tied to the embedder's pinned digests and each belief's text. Boot no longer re-embeds the whole store.
- A failed background embed step puts its work back in the queue and retries after 30 s. Before, it cleared the queue for the rest of the process.
- Only a missing or digest-mismatched model puts memory into degraded mode for the rest of the process. Other load failures, such as a file locked by a scanner, are retried the next time the model is needed.
- Boot cost follows the journal tail, not the whole history. The snapshot records the journal signature at its watermark (`Journal::open_from_checkpoint` verifies only from there on), and the tail is folded through a ranged, streaming `Journal::replay_after`.

### Verified
- `cargo test --workspace`: all pass, including `embedding_reference` and `cross_encoder_reference`.
- Live, against the real models on CPU:
  - A user statement in one session is recalled in another after a daemon restart.
  - The old profile imports (2 of 2 memories), and its files are byte-identical before and after.
  - Inject latency over 2,000 imported memories with models warm: p50 198 ms, p95 215 ms.
  - Daemon memory: 251 MB working set with both models loaded, 51 MB once they unload, 25 MB on an empty profile.

## M1 Walking skeleton — integration — 2026-10-09

### Added
- New crates: `marlowe-model` (provider seam), `marlowe-proto` (client/daemon wire), `marlowe-agent` (lean loop), `marlowe-brains` (model library, slots, credentials), `marlowe-daemon` (actor daemon, numbered bus with replay, sqlite sessions), `marlowe-tui` (Conversation + Situation Board client).
- `marlowe` bin: `daemon`, `ask [--model] [--workspace] [--session] [--yes]`, `models list|detect|catalog|add|remove|slot`, `sessions [limit]`, `status`, `shutdown`, `doctor`; auto-spawns a detached daemon.

### Changed
- `marlowe-provider`, `-openrouter`, `-exec` and the bin ported onto `marlowe-model`; workspace `exclude = ["legacy"]`, transitional legacy members removed.
- `scripts/cargo-idle.ps1` passes arguments through verbatim (`-p` and `--` work).

### Fixed
- Daemon dropped every client that sent nothing for 10 s (hello timeout left on the cloned read socket).
- `LineReader` lost a partial line when a read timed out mid-line; it now resumes.
- A daemon spawned from a pipeline held the caller's stdout open, hanging `marlowe ... | x` and redirects.
- CLI library mutations (and `ask --model`) raced the daemon's worker thread; they now wait for the broadcast `LibraryState`.
- `ask` workspaces kept the `\\?\` verbatim prefix; a new session now defaults to the current directory.
- Reasoning token counts are per delta: the TUI and CLI sum them, and a late settlement updates the thinking line.

### Verified
- `cargo check --workspace --all-targets` clean (only pre-existing test-name lints in `marlowe-memory`); `cargo test --workspace`: all pass.
- Live: OpenRouter catalog (469 models), `models add`, `ask` with a tool call, restart after `taskkill /F` with the session intact.

## M0 Foundation — 2026-10-09

### Added
- New workspace with the crates inherited from Marlowe_Harness (baseline commit `962e6fe`, a verbatim copy).
- `legacy/` holding `marlowe-loop`, `marlowe-daemon` and `marlowe-stub` as reference only — not workspace members, not usable as dependencies.
- Persona v3 (`persona/v3.md`): v2 plus a JARVIS layer — address rules ("sir" in greetings, briefings and alerts), graduate-level `<rigour>`, anticipation, dry wit in low-stakes moments, automatic memory with contradiction asides and provenance answers.
- Persona capability blocks (`persona/blocks/briefing.md`, `crew.md`, `voice.md`), appended only when the capability is live, so the prompt never leads the harness.
- `scripts/fetch-models.ps1` (copies the two pinned models from Marlowe_Harness, or the embedder from Hugging Face, and verifies sha256) and `scripts/cargo-idle.ps1` (cargo at idle priority).
- `README.md`, `CLAUDE.md`, `docs/VISION.md`, `docs/ARCHITECTURE.md`.

### Changed
- `ort` no longer enables CUDA: `download-binaries` only, pinned `=2.0.0-rc.10`. Embedder and cross-encoder are CPU-only; the shipped retrieval numerics are unchanged (reference fixture tests green).
- Workspace members: `marlowe-contract`, `-journal`, `-memory`, `-net`, `-extract`, `-tools`, `-permission`, `-mcp`, `-view`, `-surface`. Parked until M1: `marlowe-provider`, `-openrouter`, `-exec`, `marlowe` (bin).
- `marlowe-tools`: `MAX_EXPOSED_TOOLS` 14 → 64.
- CI rewritten for the new workspace; the traversal-suite accounting check is kept.

### Removed
- `marlowe-memory`: CUDA/VRAM/host-memory probing (`cue/dense/{cuda_libs,vram,hostmem}.rs`), provider-choice plumbing, the unwired GPU rerank cascade (`RerankPlan`, `Rerank::Cascade`, fusion graphs), the fit-only `Scoring::FitDump` path, gate artifacts v1–v4, examples, and device-only tests. `select_for_injection(_probed)`, the gate, the operating point and consolidation are unchanged.
- `marlowe-contract` / `marlowe-view`: the escalation vocabulary of the abandoned agent hierarchy.
- `marlowe-surface`: the old app/window/render/commands/cli/inspector/escalation modules and their tests. Kept: theme, markdown, latex, meter, region, chrome, keys, clipboard, overlay, doctor; `amplitude` and `frame_clock` moved in from the stub.
- `marlowe-permission/tests/boundary_hook.rs`, which tested the old repo's Claude Code edit-guard hook (`.claude/hooks/protect-boundaries.py`) that was not carried over. The path-scope security suites (`traversal.rs`, `toctou.rs`) are kept and green.

### Verified
- `cargo check --workspace --all-targets` clean; `cargo test --workspace`: 654 passed, 0 failed, including `embedding_reference` and `cross_encoder_reference` against the real pinned models on CPU.
