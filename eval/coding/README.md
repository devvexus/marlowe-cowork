# Coding eval (M4.6 track L)

Design: `docs/research/m46-design.md` section 6. Python on the standard library; run it as
`marlowe eval <command>` or `python -I eval/coding/run.py <command>`. Tests: `pytest eval/coding`.

| Command | What |
|---|---|
| `run --suite micro\|own\|polyglot\|iact --arm A[,B] --seeds N` | the matrix; records in `<run>/records.jsonl`, resumable |
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

## Where runs live, and the machine guard

- **Runs go outside every checkout.** The default `--out` is `marlowe/eval/<timestamp>` under `%LOCALAPPDATA%`. `run` refuses an `--out` inside this repository or under any git checkout: the agents run with no permission prompts and a shell, so the micro tasks' `solution/` and `hidden/` folders and the `m46/*` fix commits would otherwise be one `cd ..` away. Own-repo workspaces are built from `git archive` with a fresh one-commit history, so the fix is not in their `git log`. (An agent can still read this checkout by absolute path; that is not sandboxed here.)
- **Every cargo run is behind `scripts/cargo-idle.ps1`**: grading (own and polyglot Rust) and the agents' own `cargo build`/`cargo test`. The harness puts a `cargo` shim directory (`<out>/cargo-shim`: `cargo.cmd`, a `cargo` script for Git Bash, `cargo_shim.py`) first on each agent's `PATH`; it forwards to the guard, which finds the real cargo because the shim strips itself from `PATH` first. Agents also get `CARGO_BUILD_JOBS=1`, `RUST_TEST_THREADS=1` and one shared `CARGO_TARGET_DIR`.
- **A busy machine is not a failed run.** The test clock for a guarded command starts when the command's first output arrives, not while the guard waits for the lock or for memory. A guard hold (exit 75) or a wait that exceeds six hours records the run as not graded (`meta.infra`), it is left out of every rate, and `run` picks it up again on resume.
- **Unfair tasks.** `own-validate` tells a failure to compile from a failed assertion on the pre-fix tree. Hidden tests that do not compile there call names the fix added, which the statement does not give: the task is marked `unfair`, skipped by `run`, and excluded from `--approved-only` until its statement names what the tests need and it validates again.
