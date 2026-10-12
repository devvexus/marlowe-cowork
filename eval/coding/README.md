# Coding eval (M4.6 track L)

Design: `docs/research/m46-design.md` section 6. Python on the standard library; run it as
`marlowe eval <command>` or `python -I eval/coding/run.py <command>`. Tests: `pytest eval/coding`.

| Command | What |
|---|---|
| `run --suite micro\|own\|polyglot\|iact --arm A[,B] --seeds N` | the matrix; records in `results/<run>/records.jsonl`, resumable |
| `report DIR...` | tables and the A1 / A3 gates |
| `merge-gate --baseline DIR --rerun DIR --rerun DIR` | no task passing at baseline fails 2 of 2 reruns |
| `arms`, `toolchains`, `tasks --suite S` | what exists and what can run here |
| `cc-dry-run`, `cc-check` | the isolated Claude Code command and its checks; `cc-check` also asks Ollama's tag list (loads nothing) |
| `own-mine`, `own-validate` | draft and prove the own-repo tasks (`own-validate` runs cargo) |

`marlowe bench <model>` (Rust) is the report card; its coding line calls `run --suite micro`.

## Suites

- **micro** (10): a tiny Python repo, a bug or a missing function, hidden pytest tests, a reference solution in `solution/` that the tests use to prove the task can be solved.
- **own** (`own/candidates.json`): **a draft of 40 bug fixes mined from this repository's own history, not Matthew's list (9.2-h).** Each starts at the fix commit's parent with history dropped, the crate's fix-time tests hidden, graded by `cargo test` through `scripts/cargo-idle.ps1`. `status: draft` and `needs_review: true` stay until he edits the statements, drops tasks and marks the rest `approved` (`--approved-only`). Four statements are thin (`statement_weak`). Run `own-validate` before using a task: with the bug the hidden tests must fail, with only that crate's source taken from the fix they must pass. Mined with `own-mine --repo <main checkout>`; the commits must stay reachable (they do once the track branches merge).
- **polyglot**: `--polyglot-dir` is a checkout of `Aider-AI/polyglot-benchmark` (nothing is downloaded). Languages without a toolchain are reported, not failed.
- **iact** (`iact/scenarios.json`, 15): scripted conversations; both harnesses speak stream-json control messages.

## Arms and the Claude Code side

See `mcoding/arms.py` and `mcoding/cc.py` (docstrings). `marlowe-shell` is the A3 baseline (the same binary with only the shell group). Thinking is off for both harnesses unless `--thinking`; budget and timeout are the same. Claude Code's prompt and tool list are large: at 32k context on the local model it may truncate, which `cc-check` cannot show; the first live run will.

`settings.toml` is the eval profile (network allowlist, design 9.2-l); each Marlowe arm gets it as `<home>/settings.toml` in its private profile home (`marlowe -p --settings` takes permission keys only).
