# Changelog

All notable changes are recorded here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

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
