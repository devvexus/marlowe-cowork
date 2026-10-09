# Changelog

All notable changes are recorded here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### M3.1 Brief narrative and first-run onboarding

First-run onboarding:
- **Added.** Proto: `SetUser { name, address }` request; `User { name, address, set }` event, sent after `Welcome` and broadcast on change. `set` is false until something has named the user.
- **Added.** Daemon: `SetUser` writes `<home>\user.json` atomically, updates the live user (next turn, next brief, spoken notices), and broadcasts `User`. Names and addresses are trimmed, at most 64 chars, no control characters.
- **Changed.** `user.json`, when it exists, is the whole answer; `MARLOWE_USER_NAME`/`_ADDRESS` apply only without it, so a cleared address stays cleared.
- **Added.** TUI onboarding for an unset user, after the boot: name, then "sir" / your name / nothing (default sir), then a checklist of only what applies (import old memories, watch each suggested folder, add a model), each skippable. Esc skips all for the session. Typing during the boot, or within 600 ms of it appearing, sends the keys to the composer instead. The palette reopens it ("Your name and how I greet you"). The idle greeting uses the address.
- **Added.** CLI: `marlowe me [--name N] [--address sir|name|none]`.
- **Fixed.** "Not now" on the import offer now lasts the session. The daemon re-sends the offer as a reply after every `Welcome`, and the TUI treated that reply as an explicit ask.
- **Fixed (review).** The onboarding checklist no longer waits forever for discovery when the Sentinel is not running: it closes.

Brief narrative:
- **Changed.** The deterministic brief reads like a colleague:
  - A root with one pipeline is named for itself (`Valiance`); otherwise runs are named without the project prefix (`wnr-final`).
  - Runs are said as finished, failed (with a short reason), or stopped. Day words are `at 06:54`, `yesterday`, `on Tuesday`.
  - Raw log lines are never quoted. A written artifact plus a passed check, or llama.cpp's `quant size … BPW`, is paraphrased.
  - The newest result is compared with its previous sibling in at most two numbers. A sibling written within 24 h is preferred, and the run's own result comes first.
  - Memory is mentioned only when the profile gained entries; zero counts are never said.
  - At most one offer, as a short question; the offer's folder is spoken by name, not path.
  - At most 5 sentences and 90 words.
- **Changed.** Sentinel: once a run's process is gone, `X written`, `check OK`, `saved`, `exported` or `shipped` ends it as finished, and a non-zero exit code ends it as failed (`logs::ending`). A result's sibling without a counter is the newest close name, not the alphabetically first (`final` → `wnr7`, not `h3`). Added `Sentinel::result_comparison`.
- **Fixed (review).** A failing or negated last line is never reported as success. `check FAILED` / `check failed`, `3 tests failed`, `completed with errors`, `aborted` end a run as failed (`its check failed`, `errors`, `aborted`), whatever the Sentinel's status. `not written`, `wasn't saved`, `not finished` are no longer completions (`stopped` at most). The brief paraphrases a last line only when that line itself states success, and a bare "an error" is not given as a reason. `0 failed` / `no errors` still count as success.
- **Fixed (review).** A result whose primary metric did not move says `holds at`, not `is up to`; with no suite improved and not all worse it says `did not beat`, not `trailed … on all`.

### M3 The Wake v1
- **Added.** `marlowe-sentinel`, a strictly read-only watcher over consented roots.
  - Run dirs: the heartbeat pid, with protection against pid reuse, gives alive / stalled / finished / failed / ended. Stalled means the heartbeat is older than max(10 min, 5x its usual gap).
  - Logs are tailed for completion, error, traceback, NaN, OOM and Killed lines.
  - Metric JSON is diffed against its nearest sibling (wnr3 → wnr4), with best-so-far tracked only for metric names whose direction is known.
  - `trainer_state.json`, JSONL and CSV are checked for NaN, divergence, plateau and finished.
  - Also watched: git (branch, dirty count, new commits; never fetches), Ollama `/api/tags`, and NVML.
  - Notices are deduped, ranked and persisted in `cowork.db`, with seen marks per client and quiet hours. Discovery only offers candidates.
- **Added.** `marlowe-voice`: WinRT SpeechSynthesizer to a buffer, a 30 Hz RMS envelope and MediaPlayer playback, run by an actor with barge-in. The voice is the saved choice, then en-GB male, then en-GB, then the system default. On/off and the voice name persist in `voice.json`.
- **Added.** Daemon brief (`brief.rs`, `brief_ops.rs`): a deterministic brief in persona v3's register, given to the tui client after 2 h away or on the first session of the day, and stored in the day's session.
  - The optional polish is one call with a 6 s deadline, rejected if it adds a number or breaks the register.
  - Urgent notices are spoken outside quiet hours.
  - Presence (`daemon_kv`) is recorded in `cowork.db`.
- **Added.** Proto: `Brief`, `WatchList|Add|Remove|Candidates`, `ListNotices`, `NoticeSeen`, `Speak`, `StopSpeaking`, `VoiceSettings` requests; `Brief`, `Notice`, `Notices`, `WatchState`, `WatchCandidates`, `VoiceState` events.
- **Added.** CLI: `marlowe watch [list|add|remove|candidates]`, `marlowe brief`, `marlowe say`.
- **Added.** TUI wake:
  - a boot overlay that shows real systems lines (1.2 s, or 300 ms on a relaunch within 30 min; any key skips);
  - the brief as Marlowe's first message, with a presence meter driven by the voice envelope;
  - WATCH and TODAY rail sections, toasts for Attention and Alarm notices, and a Watch view (`^w`).
- **Added.** Live repo context: `Environment.repo` holds the branch, dirty count and last commit, taken from `Sentinel::git_context`.
- **Fixed (integration).** A metric notice that is best on more than 3 metrics is titled `best so far on N metrics` instead of listing every name; the live valiance audit listed 16. In the brief, a log line copied into two logs in one run dir is said once, and its `[hh:mm:ss]` is not repeated after "completed at".
- **Fixed (review).**
  - Log tails never split a line. Before, a file read with leftover per-pass budget could cut a line in two: `held-out error 0.12` became a false "logged an error", and a CSV header could be cut. Now only a newline-less run as long as the full per-tick cap counts as one line; a shorter cut waits for the next pass.
  - Run times are the times observed. A run that had already ended when its root was added, or that ended while the daemon was down, is dated by its last heartbeat or log write, not by when Marlowe noticed. Late notices say when (`wnr6 failed at 03:10`). Old runs no longer show up in the brief as "ended at" the time you added the root.
  - The brief polish no longer loads a local model (Ollama, GGUF) onto the GPU your jobs use. It is off by default for local providers. With `MARLOWE_BRIEF_LOCAL_POLISH=1` it runs only when every GPU is idle and no watched run is live.
  - The polish is held to persona v3. Rejected: any phrase from `<words_to_avoid>` (read from `persona/v3.md`), any emoji, more than five sentences, and dropping an attention item the draft states (its name and its numbers).
  - Spoken notices are said once, and again only if the severity rises. A 30-minute re-raise with a new detail shows on screen but is not spoken.
  - Spoken form: `NaN/inf` is read "NaN or inf", not "inf"; only real paths shrink to their last part. Log decoration (`===`) is no longer read aloud.
  - Brief honesty:
    - A stall or failure the run has since recovered from no longer shows as needing you.
    - A stage log's "completed" inside a live run is folded into that run's "still running" line, not reported as finished.
    - Plain "new results" stay on the board.
    - A best-so-far keeps its leading numbers when the full detail is too long; metric details now list bests first.
    - `===` no longer appears in brief text.
  - With the Sentinel not running, the brief says "The Sentinel is not running, so I have nothing on your runs." instead of "Nothing moved while you were away."
- **Verified.**
  - `cargo check --workspace --all-targets` is clean apart from the 4 existing marlowe-memory lints.
  - `cargo test --workspace --no-fail-fast`: 1170 passed, 0 failed, 7 ignored (after the review fixes; 1162 before).
  - Live on a debug build with a temp home, against `PruningResearch\valiance` while a run was going:
    - discovery offered valiance (1 live run) and engine;
    - the steady-state daemon used about 4 ms CPU per second, with a working set of about 70 MB;
    - every file added or removed under the root was the research job's own.
- **Known.** No en-GB voice is installed on the dev machine, so the voice is David (en-US). Live audio playback was not run. Polish runs on the Quick or Conductor slot. On a local model it is skipped unless `MARLOWE_BRIEF_LOCAL_POLISH=1` is set; a remote slot cannot be switched off.

### M2.6 Profile in context
- **Added.** `Mind::profile(ProfileQuery)` returns `Profile { block, ids, tokens, omitted }`. It holds live Preference, Decision, Fact and Procedure records written deliberately (remember, a correction, the harness), never untrusted, Global or in the current project. Each is one line, such as `- preference · 3 Sep: use uv, not pip`. Order is preference, decision, fact, procedure, newest first, capped at 1,500 tokens. The block is deterministic. The reason: short personal facts score poorly on the cross-encoder (admission-floor study), so what the conductor deliberately records now sits in context every turn.
- **Changed.** `MindMemory` sends the profile as its own cached block in the context tier. `InjectQuery.exclude` keeps profile ids out of the retrieval pool. `MemoryProbe` does not exclude them.
- **Changed.** Maturation: `MaturationPolicy::Deliberate` makes remember/correct from an untainted session live at once. From a tainted session the write stays UntrustedContent and keeps the 6 h window.
- **Added.** `persona/blocks/memory.md` is appended only when a Mind is attached. The remember tool now requires `kind`. Its kind enum has descriptions, decisions require a reason, and scope is `global|project`. The recall and correct descriptions are also updated.
- **Added.** `Request::MemoryProfile` / `Event::MemoryProfile`. In the TUI Memory view, `^p` toggles a profile list of what is in context now.
- **Security.** `remember` deduplicates only against deliberate records of the same kind, so a captured user turn with the same text no longer swallows the profile entry. A correction takes a decision's reason from its own text (`(reason: …)`), never from the target, so an untrusted reason cannot reach the profile through `correct`.

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
