# Third-party material

Code ported from other projects, test cases taken from them, and the crates.io dependencies added
for M4.6. Each ported file also carries its own licence header. Nothing comes from claurst or from
the original (leaked) Claude Code TypeScript source.

## Ported code

| Source | Licence | What | Where |
|---|---|---|---|
| openai/codex @ `6c0c6759d0617d7670826b61aca0a931bbb6db1f` | Apache-2.0 | `is_dangerous_command` and the Windows dangerous-command list; strict literal-word lowering; the PowerShell AST parser protocol, with its `.ps1` script unchanged | `crates/marlowe-permission/src/shell/{danger,words,pwsh_ast}.rs`, `shell/fixtures/codex_powershell_parser.ps1` |

## Test cases (fixtures only, no code)

| Source | Licence | Cases | Where |
|---|---|---|---|
| openai/codex @ `6c0c6759d0617d7670826b61aca0a931bbb6db1f` | Apache-2.0 | apply-patch scenarios, `seek_sequence` and `text_file` cases | `crates/marlowe-exec/src/specs/files/tests.rs`, `crates/marlowe-code/src/edit/donor_tests.rs` |
| openai/codex (same commit) | Apache-2.0 | dangerous-command (47) and word-lowering (23 bash) tests, 68 PowerShell lowering cases, 6 parser-child tests, 23 hook JSON schemas | `marlowe-permission/src/shell/danger/tests.rs`, `shell/words.rs`, `shell/fixtures/codex_powershell_lowering.json`, `marlowe-agent/src/approval/tests.rs`, `marlowe-settings/tests/fixtures/codex-hooks/` |
| openai/codex (same commit), `codex-rs/secrets/src/sanitizer.rs` tests | Apache-2.0 | secret-scrubbing cases | `crates/marlowe/src/export/scrub/` tests |
| flashback7766/flashagent @ `0b08afcd6042615d38d87e80c3e5edda2a49da71` | MIT | `fs_tools.rs` cases; `permissions.rs` cases; repetition and loop-nudge cases | `marlowe-exec/src/specs/files/tests.rs`, `marlowe-code/src/edit/donor_tests.rs`, `marlowe-permission/src/shell/tests_flash.rs`, `marlowe-agent/src/agent/guard.rs`, `marlowe-agent/tests/parity_f.rs` |
| ultraworkers/claw-code @ `08106b0c3771ef5b4a5aa176acccd460e88b7325` | MIT | `permissions.rs` and `hooks.rs` cases; the compaction tool-pair walk-back as a behaviour case; the reasoning-model temperature note (knowledge only) | `marlowe-permission/src/rules/policy/tests.rs`, `marlowe-settings` tests, `marlowe-agent/src/hooks/tests.rs`, `parity_c7_compaction_never_splits_a_tool_call_from_its_result`, `marlowe-openrouter` |
| agent-code @ `82e0edfa7372c256d893d0854f954d9099254f88`, `crates/lib/src/services/secret_masker.rs` tests | MIT | secret-masking cases | `crates/marlowe/src/export/scrub/` tests |
| codewhale @ `406d17ee5f38b643d9f703cfdf8a6906920835dc`, `crates/sanitize/src/redact.rs` tests | MIT | redaction cases | `crates/marlowe/src/export/scrub/` tests |
| anthropics/claude-plugins-official, marketplace snapshot (lastUpdated 2026-10-10T17:50Z, no git sha) | Apache-2.0 | agent, command, skill and plugin files in Claude Code's format | `crates/marlowe-agent/tests/fixtures/cc/` (files listed in its `SOURCE.md`) |

ripgrep 14.1.1 (Unlicense/MIT) is used only as a test-time oracle for the search fixture; nothing
is vendored.

## Crates added in M4.6

| Crate | Licence | Used by |
|---|---|---|
| `ignore`, `grep-searcher`, `grep-regex`, `grep-matcher` | Unlicense/MIT | `marlowe-exec` search, `marlowe-code`, `marlowe-tui` |
| `tree-sitter`, `tree-sitter-tags` and the rust, python, javascript, typescript, go, c and cpp grammars | MIT | `marlowe-code` tags index (not on aarch64) |
| `lsp-server`, `lsp-types` | MIT/Apache-2.0 | `marlowe-code` LSP client |
| `pdf-extract` (already used by `marlowe-extract`) | MIT | `marlowe-code` PDF reads |
| `toml_edit` | MIT/Apache-2.0 | `marlowe-settings` |
