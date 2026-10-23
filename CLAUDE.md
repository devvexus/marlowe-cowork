# User
ML researcher. Address the User as "Matthew" at the beginning of every turn.

# MarloweCowork

A local-first Rust AI coworker — a research engineer that lives on Matthew's machine, remembers
everything, watches his experiments, runs a crew of sandboxed agents, and greets him every morning
with what changed. The successor to `C:\Users\matth\Projects\Marlowe_Harness`, built by keeping its
implementations (memory engine, journal, executors, renderers, theme) and replacing its seams.

- Vision: [docs/VISION.md](docs/VISION.md) · Architecture: [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)
- Approved plan and milestones (M0–M7): `C:\Users\matth\.claude\plans\claude-multiple-companies-have-golden-metcalfe.md`
- What changed: [CHANGELOG.md](CHANGELOG.md). **There is no STATE.md and there will not be one.**

## Non-negotiables

1. **The palette is locked.** `crates/marlowe-surface/src/theme.rs` colours and derivations stay
   exactly: accent `#9B7EDE` (structure/hover derived by scaling), speech `#b39ae8`, green `#8fd4a8`,
   amber `#d6a95f`, red `#dd7f85`, blue `#7fa8dd`, weights `#cdc9d6`/`#6a6480`/`#4a4460`, terminal
   background `#0f0e14`, **no background fills**. Layout and panels may change freely.
2. **Memory is the product.** Never inject a memory that was not retrieved. Trust propagates
   worst-case over lineage. User turns are ingested at user trust with no maturation delay; web and
   agent content keeps maturation and untrusted lineage. The signed journal is the audit spine.
   Retrieval numerics on the shipped path (digest pins, tokenizer, gate weights, thresholds,
   operating point) change only with a re-measured eval — `eval/` must reproduce R@1 0.6725 /
   R@5 0.8865 on the held-out split. Reference fixtures are never regenerated to make a test pass.
3. **The prompt never leads the harness.** `persona/v3.md` is the stable core and describes no
   feature-specific tool. `persona/blocks/*.md` are appended only when that capability is live. The
   persona artifact never names its operator; the address form comes from the profile at runtime.
4. **The conductor never blocks.** Crew work runs on its own thread as a durable run; `dispatch`
   returns an id immediately.
5. **Sandbox, not ceremony.** Crew agents are free inside a git worktree / scratch dir. Approval is
   required for merge, push, deletes outside the workspace, new network hosts, and budget overrun.
   After an untrusted read, side effects outside the sandbox need approval. API keys live in Windows
   Credential Manager and are scrubbed from every child process environment.
6. **The old repo is read-only.** `Marlowe_Harness` and `%LOCALAPPDATA%\marlowe\default-profile` are
   sources to copy from, never to modify. `legacy/` in this repo is reference code outside the build.

## Working rules

- **Agent model band:** every subagent/workflow agent runs between **Sonnet 5.5 at `medium`** and
  **Opus 5.5 at `high`**. Sonnet medium for routine work; Opus high for load-bearing design and
  adversarial review. Always set `model` and `effort` explicitly.
- **Compute:** if Matthew has said the machine is busy, no cargo, tests, or local inference until
  he says it is free. Cargo runs through `powershell -NoProfile -File scripts/cargo-idle.ps1 <args>`
  (idle priority, inherited by every rustc) with `-j2`–`-j4`, one build at a time. GPU work (local Ollama inference) needs its
  own go-ahead; use OpenRouter for live model tests while the GPU is reserved.
- **Never read the old ledgers** (`Marlowe_Harness/STATE.md`, `CLAUDE.md`, `docs/design/DECISIONS.md`,
  `ROADMAP.md`, `SECURITY-AUDIT.md`, `CONTRACTS.md`). Grep source and read targeted ranges.
- **Docs stay short.** Decisions go in a few lines of `docs/ARCHITECTURE.md` or a CHANGELOG entry,
  not essays. Inherited files carry long ADR-archaeology comments; trim them when you touch the
  surrounding code, and only there.
- Windows is the primary platform; Windows ARM64 must keep building: no x86 intrinsics, no new C
  toolchains without a reason. `ring` needs the VS clang component on ARM64. **CUDA is a hard
  requirement on x64 and ARM64 (GB10)** behind the `cuda` feature, with GPU-idle awareness and a
  CPU fallback.

## Shape of the code

- **Concurrency:** `std` threads + `crossbeam-channel` actors. No tokio — everything inherited
  blocks (rustls HTTP, rusqlite, ort, `std::process`, crossterm).
- **Storage:** WAL sqlite for operational state (sessions, turns, runs, notices, schedules, model
  library); the signed hash-chained journal for memory/audit events; BeliefStore snapshots so boot
  time never grows with history.
- **Seam:** `marlowe-model` (M1) — `ChatRequest` / `Completion` / `StreamEvent` and
  `trait Provider`. Providers never interpret tool names; the loop does.

| Crate | Status |
|---|---|
| `marlowe-contract`, `-journal`, `-memory`, `-net`, `-extract`, `-tools`, `-permission`, `-mcp`, `-view`, `-surface` | workspace members (M0) |
| `marlowe-provider` (Ollama), `-openrouter`, `-exec` (BuiltinTools), `marlowe` (bin) | ported onto `marlowe-model` (M1) |
| `marlowe-model`, `-proto`, `-agent`, `-brains`, `-daemon`, `-tui` | new (M1) |
| `marlowe-os`, `-crew`, `-sentinel`, `-voice` | new, M2–M6 |
| `marlowe-project` | projects core (plan, state JSON, forms with display hints, generic view-model, store); stage A of `docs/research/projects-design.md` |
| `legacy/*` (old loop, daemon, stub, bin, exec) | reference only (`exclude = ["legacy"]`) |

## Commands

```
cargo check --workspace --all-targets
cargo test --workspace --no-fail-fast
cd eval && python -m pytest            # LongMemEval scoreboard against the shipped binary (M2+)
```

Models (git-ignored) live in `models/`: `jina-embeddings-v2-small-en` (model.onnx, vocab.txt) and
`ms-marco-MiniLM-L-2-v2-ft-session-j` (model.onnx, tokenizer.json), sha256-pinned in code.
