# First-try probe on the GPU (2026-10-12)

62 cases, 2 runs each (temperature 0, model default), Ollama, `--num-ctx 32768`. B = typeless schemas, C = typed.
Strict = right tool, schema-exact args, first try (a look-up call before the answer is allowed). round0 is the full
run with the specs as they were; round1/round2 re-ran only the cases of the tools whose descriptions were tightened
(3 runs each). Per-case tables are in each round's `report.md`; the raw run lines are the `.jsonl` files (local only).

| model | arm | strict | lenient |
|---|---|---|---|
| dawn:9b | B typeless | 51.6% | 52.4% |
| dawn:9b | C typed | 50.0% | 50.0% |
| dusk:27b-super | B typeless | 10.5% | 12.9% |
| dusk:27b-super | C typed | 54.8% | 54.8% |

**Decision: typed schemas are the Ollama default.** The chat template renders a property with no `type` as an object
and dusk then nests every value (`{"path": {"path": "x"}}`), so B fails 9 in 10 on dusk; dawn is level. Typed costs
about 1,000 more prompt tokens for the 62-tool conductor surface (8,041 against 7,024). `MARLOWE_OLLAMA_TYPED_TOOLS=0`
still sends typeless.

**Calibration (real tokens against chars/3 of the conductor surface):** typeless 1.09-1.10x (2.73 chars per token),
typed 1.25-1.26x (2.40 chars per token). Small subagent surfaces run 1.4-1.6x because of per-tool framing.

**Tuning (arm C, same cases before and after, strict):** dawn 2/30 to 13/45, dusk 8/30 to 19/45 in round1 (grep,
symbols, references, hover, project, powershell, monitor, bash, dispatch, agent, enter_plan descriptions). round2
(bash `background: true`, project gate, enter_worktree, dispatch vs workflow) moved one or two cases and was noisy
(monitor 3/3 to 1/3 on dawn), so tuning stopped there.

**Not passing, and why.** Neither model meets the design's 85/95% criteria. Most remaining failures are not the
spec: the model looks around first (`ls`, `glob`, `git status`, `recall`) where the case wants the tool at once
(ask_user, enter_plan, enter_worktree, project-gate, set_statusline, create_agent, handback, send_file, watch), or
asks a clarifying question (dawn: no call at all on enter_plan, notices, correct). Those need either wider `first_ok`
lists in the cases or persona guidance, not tool text. Real spec failures left: dawn `bash` background, `workflow`
scripts (dawn writes invalid Starlark, dusk calls dispatch), `service` args, `multi_edit` (renames via
rename_symbol), `read_mcp_resource` (lists first).

Grader change: the edit tools' own "you have not read it in this session" refusal is a state gate that a canned read
cannot satisfy, so it no longer counts as an argument failure.
