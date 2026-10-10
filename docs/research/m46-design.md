# M4.6 design: full-stack engineering and coding

Status: proposed 2026-10-09, revised after review the same day; awaiting Matthew's approvals
(section 9). Plan row: M4.6 in `~/.claude/plans/claude-multiple-companies-have-golden-metcalfe.md`.
**Bar (Matthew):** "not half measure, just like Claude Code at a minimum." Claude Code (CC) parity
is the floor for the interactive conductor AND the crew; memory, crew, Sentinel, voice and local
models are the edge. Parity is measured (sections 1, 6), not claimed. Review resolutions: section 11.

## 1. Acceptance (M4.6 is done when all hold)

| # | Gate | Number |
|---|---|---|
| A1 | H2H, same frontier model, same tasks (6.3), 3 seeds | pooled mean pass@1 Marlowe >= `claude -p`; per tier, paired-bootstrap 95% CI lower bound of (Marlowe - CC) >= -10 pts; gated tiers own 40 + polyglot 40 (+ tb 30 only if spike S1 passes, else unscored arm); own 40 x 1 seed re-run through the daemon path meets the same rule; interactive suite `iact` 15: Marlowe >= CC |
| A2 | Parity matrix (section 2) | every `v1`/`v1+` row's test `parity_<row>` exists and passes (`cargo test parity_`, `pytest -k parity_`); terminal-rendering rows also pass the recorded-script checklist in Windows Terminal and the VS Code terminal; 0 deferrals without Matthew's sign-off |
| A3 | Local dusk-27b-super earns its tools | own 40, 3 seeds, paired bootstrap: full tools - bash-only baseline >= +10 pts mean, CI lower bound > 0; baseline = mini-SWE-agent at a pinned version, shell = Git Bash, same model |
| A4 | Tool-call reliability (bench, 200 prompts) | dusk >= 98% valid after repair, <= 5% needing repair; frontier >= 99.5% |
| A5 | Edit reliability, two rates | first-attempt apply per edit call >= 92% dusk / 98% frontier; per-intended-edit success within 2 retries >= 98% / 99.5%; per format in `marlowe bench` |
| A6 | End to end, pre-registered (9.2-h) | 2 feature specs x {dusk, frontier} on the named repo with CI via `build_v2`: gate green vs baseline, merged after approval, frontier PR CI green; done only with sign-off by Matthew or an independent frontier reviewer outside the run; every attempt recorded; wall-clock within the run budget (3.6) |
| A7 | Durability | daemon killed mid-model-call or after tools before next call: resume loses <= 1 model call and 0 completed tool calls; `/rewind` byte-identical for tracked + untracked non-ignored files + ignored files Marlowe tools wrote; user repo refs/object count unchanged; facts only in rewound turns not recalled |
| A8 | Code intel | `rust-lang/rust-analyzer` at a sha pinned at track start, this machine (Win 11 x64): cold tags index <= 20 s per 100k LOC; incremental <= 50 ms/file; post-edit diagnostics p95 <= 5 s warm |
| A9 | Search | `grep`/`glob` match ripgrep on 40-case fixture; MarloweCowork-wide grep <= 300 ms warm |
| A10 | Headless | json/stream-json goldens; exit codes 0/1/2/3/130; `dont_ask` denial -> model recovers -> exit 0; never hangs |
| A11 | Export scrub | 0 leaks of 1,000 planted canaries |
| A12 | Memory unchanged | `eval/` reproduces R@1 0.6725 / R@5 0.8865, re-run after the `Retracted` change (3.4) |
| A13 | Builds | `cargo check/test --workspace` green on x64; `cargo check --workspace --target aarch64-pc-windows-msvc` green in every integrator gate (9.1-i precondition for merging C) |
| A14 | Cache | recorded 30-step Anthropic session: `cache_read_input_tokens/input_tokens` >= 0.8 from step 3; Ollama `prompt_eval_count` per step <= new tokens + 64 |
| A15 | Prefix budget | assembled dusk-profile prefix <= 9k tokens at 32k (3.4); every Ollama request `num_ctx` >= prompt + max output |
| A16 | Sandbox holds | in `bypass`: `rm -rf ../x`, `curl https://new.host`, `git push` Ask; `push --force-with-lease`/`+main`/`--mirror` denied; 150+ case shell corpus 100% (unparseable -> Ask) |

## 2. Parity matrix

`v1` ships at parity; `v1+` ships and exceeds CC; `SIGN-OFF` = proposed deferral in 9.3 (no row is
"later" without it). Today: `-` none, `P` partial, `Y` exists. Test column: what `parity_<row>`
asserts (written now; the integrator fails rows whose test is missing or red). `[R]` = also on the
recorded-script checklist. Tracks: section 8.

### 2.1 Tools

| # | Claude Code | Marlowe v1 | Today | Track | v1 | Test asserts |
|---|---|---|---|---|---|---|
| T1 | Read offset/limit | `read{path, offset, limit}`; `cat -n`; 2,000-line window + continue hint | Y | A | v1 | window + continue hint exact |
| T2 | Read images | `Part::Image` downscaled per provider; non-vision: caption via vision slot else refusal | - | A, G | v1 | png -> image part in request JSON |
| T3 | Read PDFs, pages | `read{path, pages:"1-5"}`; text via `marlowe-extract`; page images on vision models | - | A | v1 | pages 2-3 text only |
| T4 | Read notebooks | cells + outputs; image outputs -> image parts | - | A | v1 | fixture ipynb rendered |
| T5 | Write | atomic temp+`MoveFileExW`; read-guard; keeps BOM/EOL | P | A | v1 | crash sim leaves original |
| T6 | Edit, replace_all | exact -> EOL-normalised -> whitespace (unique only); did-you-mean | P | A | v1+ | 150-case fixture |
| T7 | Read-before-edit, stale check, external-change notice | `FileState{blake3, mtime, size}` per agent; watcher on read files -> next-step reminder with short diff | - | A, F | v1 | external modify -> reminder in next request |
| T8 | MultiEdit atomic | `multi_edit{edits:[...]}` cross-file all-or-nothing | - | A | v1+ | rollback on 3rd edit failure |
| T9 | (none) | `edit_lines{path, anchor:"line:hash", op, text}` bench arm for dusk/valiance | - | A | v1+ | stale hash rejected |
| T10 | Glob | `ignore` walker, mtime-sorted, `head_limit` | P | B | v1 | rg fixture |
| T11 | Grep (rg semantics) | `grep{pattern, path, glob, type, output_mode, A, B, C, i, n, multiline, head_limit, offset}` | P | B | v1 | 40-case rg fixture |
| T12 | Bash | Git Bash, Job Object, scrubbed env, tripwire (3.5) | Y | B, E | v1 | env has no key; tripwire fires |
| T13 | PowerShell | `-NoProfile -NonInteractive`, same | Y | B, E | v1 | same as T12 |
| T14 | Timeouts 2/10 min | same | Y | B | v1 | 10 min cap kills tree |
| T15 | Background shell | `run_in_background` -> `job_id`; `job_output{id, since, filter}` | P | B | v1 | stream since offset |
| T16 | Monitor + exit notices | `job_watch{id, until_regex}` emits events while model works; job exit -> reminder; idle conductor -> daemon starts a turn | - | B, F | v1 | `sleep 2; echo done`, turn ended -> new turn with notice |
| T17 | Kill background | `job_kill{id}` tree kill | P | B | v1 | grandchild gone |
| T18 | Ctrl+B background running cmd | detach foreground shell to a job | - | B, H | v1 | detached job keeps output [R] |
| T19 | NotebookEdit | `notebook_edit{path, cell, new_source, cell_type, mode}` | - | A | v1 | insert/replace/delete |
| T20 | WebFetch | + JS render via Edge; taint | Y | - | v1+ | existing tests renamed |
| T21 | WebSearch | + M4.5 channel registry | Y | - | v1+ | existing tests renamed |
| T22 | TodoWrite | `todo{items}`; panel; pinned; stale reminder | - | F, H | v1+ | reminder after 10 steps |
| T23 | AskUserQuestion | `ask_user{questions}` modal; headless via prompt tool or denied | - | F, H | v1 | answer round-trip [R] |
| T24 | Diagnostics after edits | new errors in touched files appended (settle window) | - | C | v1 | fake LSP error appended |
| T25 | LSP tool | name-based `symbols/definition/references/hover` | - | C | v1+ | fake LSP resolves by name |
| T26 | (none) | `rename_symbol` via LSP WorkspaceEdit -> multi_edit | - | C | v1+ | rename applied atomically |
| T27 | (none) | repo map (ranked tags, budgeted) | - | C | v1+ | budget respected |
| T28 | (none) | `project`, `service`, `http_check`, `browser` (3.8) | P | D | v1+ | fixture app served + screenshot |
| T29 | (none) | `git`, `gh`, `ci_status` (3.9) | - | J | v1+ | fake gh flow |
| T30 | (none) | `train_launch`, `experiments` (3.9) | - | J2 | v1+ | dummy trainer row |
| T31 | ToolSearch (deferred tools) | `tool_search{query}` activates when MCP tools > 20 or prefix over budget | - | K | v1 | 80 MCP tools within dusk budget |
| T32 | 30k output, middle-cut | failure-aware summary <= 30k + full log `[full output: path, N lines]`; `raw:true` | P | B | v1+ | 6 captured logs, full log readable |

### 2.2 Agents

| # | Claude Code | Marlowe v1 | Today | Track | v1 | Test asserts |
|---|---|---|---|---|---|---|
| A1 | Agents in md + frontmatter | `.marlowe/agents`, `<home>\agents`, `roles/`, `.claude/agents` (compat); full CC keys: `tools, disallowedTools, model, effort, permissionMode, skills, mcpServers, hooks, maxTurns, isolation`; unknown keys warn | P | F | v1 | real CC-format fixtures parse |
| A2 | Task tool | `agent{type, description, prompt, isolation, background}`; clean context | - | F | v1 | result returned |
| A3 | Parallel subagents | concurrent `agent` calls in a step, slot-bounded | P | F | v1 | 2 explore overlap in time |
| A4 | Background subagents | `background:true` -> crew run; completion wakes conductor (T16) | Y | F | v1+ | notice starts turn |
| A5 | Worktree isolation | `isolation:"worktree"` via seam `WorktreeProvider` (I implements) | P | F, I | v1 | fake provider; live at integ |
| A6 | Explore agent | read/glob/grep/symbols; cheapest capable slot | - | F | v1 | write tools absent |
| A7 | Plan agent | read-only; returns step plan + files | - | F | v1 | write tools absent |
| A8 | General agent | caller tools minus `agent` | - | F | v1 | depth 1 enforced |
| A9 | Workflows | playbooks `research_v2`, `build_v2`, `review` | P | I | v1+ | scripted-host build test |
| A10 | Per-agent model/effort, aliases | `sonnet/opus/haiku/inherit` -> library slot via settings map | P | F | v1 | alias resolves |
| A11 | SendMessage / resume subagent | `send_message{agent_id, text}` resumes a finished subagent's transcript | - | F | v1 | 2nd message sees 1st context |
| A12 | Agent teams | crew runs with run inbox + `notify_run` | P | I | v1+ | inbox message reaches step |
| A13 | Conductor in worktree (`-w`, Enter/ExitWorktree) | `marlowe -w <name>`, `enter_worktree`/`exit_worktree` | - | F | v1 | cwd + roots switch, exit keeps branch |

### 2.3 Modes and permissions

| # | Claude Code | Marlowe v1 | Today | Track | v1 | Test asserts |
|---|---|---|---|---|---|---|
| M1 | Plan mode | read-only set + `exit_plan{plan}` -> approval -> prior mode | - | E, H | v1 | every write tool blocked |
| M2 | default | shell Asks; edits Free in workspace (9.2-f) | Y | E | v1 | |
| M3 | acceptEdits | + project-profile cmds, read-only git Free | P | E | v1 | rule table |
| M4 | bypassPermissions | `bypass` (alias `auto`): Free in sandbox; tripwire hits and fixed approvals still Ask | - | E | v1 | A16 cases |
| M5 | dontAsk | `dont_ask`: not allowed -> denied, reason to model, run continues | - | E | v1 | recovery after deny |
| M6 | Classifier-gated auto | model judges risky actions | - | E | SIGN-OFF 9.3-d | |
| M7 | allow/ask/deny rules | `bash(git push*)`, `edit(src/**)`, `web_fetch(domain:x)`, `mcp__srv__tool`; deny > ask > allow | - | E | v1 | precedence table |
| M8 | Rule parsing of compound shells | per-shell parser (bash, PowerShell); `;`, `&&`, `\|`, `-and`, `& 'git'`, `iex`, `cmd /c`, `bash -c`, `pwsh -Command`, quoting, env indirection; unparseable -> Ask | P | E | v1+ | 150+ case corpus |
| M9 | Settings user/project/local | `<home>\settings.toml`, `.marlowe\settings{,.local}.toml`, CLI; read-only import of `.claude/settings.json` hooks + permissions (trust by digest) | - | E | v1 | layering + import fixture |
| M10 | /permissions UI | list/add/remove, which rule fired | - | H | v1 | key replay |
| M11 | Don't-ask-again scoped | session allow keyed by rule pattern (tool + normalised prefix or path glob), never bare tool; "always for project" -> `settings.local.toml` | P | E, H | v1 | allow `cargo test` still Asks `cargo publish`, `git push`; survives restart |
| M12 | OS sandbox | worktree + Job Object + scrub + tripwire + egress approvals; AppContainer/low-IL | P | E | SIGN-OFF 9.3-a | |

### 2.4 Context

| # | Claude Code | Marlowe v1 | Today | Track | v1 | Test asserts |
|---|---|---|---|---|---|---|
| C1 | CLAUDE.md hierarchy | `MARLOWE.md`/`AGENTS.md`/`CLAUDE.md` + `.local.md`; user level incl. `~/.claude/CLAUDE.md`; root -> cwd; subdirs lazily; no cap on frontier, warned cap on 32k | - | F | v1 | 20k-char file loads whole (frontier) |
| C2 | `@path` imports | depth 5, cycle-safe | - | F | v1 | cycle fixture |
| C3 | Path-scoped rules | `.marlowe/rules/*.md`, `.claude/rules/*.md` with `paths:`; loaded on first matching touch | - | F | v1 | fires only after matching read |
| C4 | /init | `MARLOWE.md` from repo map + profile + git log | - | F | v1 | scripted provider |
| C5 | `#` quick memory | `remember` at user trust | Y | - | v1+ | existing |
| C6 | /memory | open instruction files in `$EDITOR`, reload | - | F, H | v1 | reload after edit |
| C7 | Auto-compaction | append-only history; mask in whole chunks only at trigger; pinned working set; summary fallback | P | F | v1+ | pinned set survives |
| C8 | /compact focus | manual with focus text | - | F | v1 | focus in summary prompt |
| C9 | /context | meter + breakdown incl. truncation warnings | P | H | v1 | key replay |
| C10 | Prompt caching | Anthropic breakpoints, OpenRouter, Ollama prefix stability | P | F, G | v1 | A14 |
| C11 | /clear | new session, memory kept | - | F | v1 | |
| C12 | /add-dir, `--add-dir` | adds workspace root (rules, roots) | P | F | v1 | path allowed after add |
| C13 | Output styles | `.marlowe/output-styles/*.md` appended last in system | - | F | v1 | block present |
| C14 | Thinking toggle + display | effort/thinking per turn; thinking shown collapsed | P | G, H | v1 | budget in request [R] |
| C15 | /model mid-session | switch slot; history kept | P | F, H | v1 | next call uses new model |

### 2.5 Sessions

| # | Claude Code | Marlowe v1 | Today | Track | v1 | Test asserts |
|---|---|---|---|---|---|---|
| S1 | -c / -r / picker | `marlowe -c`, `-r <id>`, `/resume` | P | F2, K | v1 | picker key replay |
| S2 | Checkpoints + /rewind | shadow git dir per repo (3.4), git and non-git alike, code/conversation/both | - | F2 | v1+ | A7 cases |
| S3 | Diff of changes | `/diff [turn\|session]`; crew diff viewer | P | F2, H | v1 | diff equals shadow tree diff |
| S4 | Tokens and cost | per call/turn/session/run; `/cost` | P | F2, H | v1 | cost = usage x price table |
| S5 | Transcript | turns appended per model call + per tool batch | P | F2, I | v1+ | A7 |
| S6 | Session name, fork | `/rename`, `/fork` (copy to new id at a turn) | - | F2 | v1 | fork independent |
| S7 | /export conversation | markdown/json of session | - | F2 | v1 | golden |

### 2.6 Extensibility

| # | Claude Code | Marlowe v1 | Today | Track | v1 | Test asserts |
|---|---|---|---|---|---|---|
| X1 | PreToolUse | full output: `permissionDecision`+reason, `updatedInput`, `additionalContext`, `continue:false`+`stopReason`, `systemMessage`, `suppressOutput`; exit 2 blocks | - | E | v1 | one test per field |
| X2 | PostToolUse, PostToolUseFailure | `additionalContext` fed back | - | E | v1 | per event |
| X3 | UserPromptSubmit | block or add context | - | E | v1 | per event |
| X4 | Stop, SubagentStart, SubagentStop | stop hook can force continuation | - | E | v1 | forced continue |
| X5 | SessionStart, SessionEnd | | - | E | v1 | per event |
| X6 | Notification | + Windows toast + voice | - | E | v1+ | per event |
| X7 | PreCompact | | - | E | v1 | per event |
| X8 | PermissionRequest | hook can allow/deny the modal | - | E | v1 | auto-allow hook |
| X9 | Matchers, env, prompt hooks | regex matcher incl. `mcp__.*`; `MARLOWE_PROJECT_DIR` + `CLAUDE_PROJECT_DIR`; `type:"prompt"` LLM-evaluated hooks | - | E | v1 | matcher table |
| X10 | Slash commands | `.marlowe/.claude/<home>` commands; `$ARGUMENTS`, `$1..$N`, `!`cmd`` pre-exec (rules apply), `@file`; frontmatter `description, allowed-tools, model, argument-hint` | - | F | v1 | CC fixtures |
| X11 | Skills | `SKILL.md`; `allowed-tools`, `context: fork`, `disable-model-invocation`, `user-invocable`, `argument-hint`, `${SKILL_DIR}` | P | F | v1 | CC fixtures |
| X12 | MCP stdio | `mcp__<srv>__<tool>`, untrusted (ADR-052), output cap 25k tokens | P | K | v1 | fake server |
| X13 | MCP HTTP + auth | streamable HTTP via `marlowe-net`; headers; OAuth 2.1 + DCR, token in Credential Manager | - | K | v1 (9.2-d) | fake OAuth server |
| X14 | MCP resources, prompts, list_changed | `@srv:uri`; `/mcp__srv__prompt`; tool set refresh | - | K | v1 | list_changed refresh |
| X15 | `.mcp.json`, `mcp add/list/remove` | same file; `marlowe mcp ...` | - | K | v1 | |
| X16 | /hooks, /agents, /mcp UIs | TUI panels (list, enable, trust) | - | H | v1 | key replay |
| X17 | Plugins (local) | local-path bundles (agents/commands/skills/hooks/mcp) | - | F | v1 | bundle loads |
| X18 | Plugin marketplaces | | - | F | SIGN-OFF 9.3-b | |
| X19 | /loop, scheduled tasks | `/loop <interval> <cmd>`; daemon `schedules` table; fires non-blocking turns | - | F | v1 | fires twice |

### 2.7 Headless and SDK

| # | Claude Code | Marlowe v1 | Today | Track | v1 | Test asserts |
|---|---|---|---|---|---|---|
| H1 | `claude -p` | `marlowe -p` (stdin appended); daemon default, `--standalone` | P | K | v1 | both paths |
| H2 | text/json/stream-json; stream-json input | `--output-format`, `--input-format stream-json` (multi-turn) | - | K | v1 | goldens |
| H3 | Exit codes | 0, 1, 2 only under `--fail-on-ask`, 3 budget, 130 cancel | - | K | v1 | matrix |
| H4 | Flags | `--allowed-tools`, `--disallowed-tools`, `--permission-mode`, `--max-steps`, `--max-budget-usd`, `--model`, `--append-system-prompt`, `--add-dir`, `--mcp-config`, `--strict-mcp-config`, `--settings`, `-c/-r`, `-w` | P | K | v1 | one test per flag |
| H5 | Permission prompt tool, control msgs | `--permission-prompt-tool mcp__s__t`; stream-json `can_use_tool`, `interrupt`, `set_permission_mode` | - | K | v1 | prompt tool consulted; interrupt -> 130 |
| H6 | Agent SDK | stream-json protocol + `eval/marlowe_client.py` | - | K | v1 | client drives a turn |
| H7 | `ANTHROPIC_API_KEY` from env | env fallback read once, scrubbed from children | - | G, K | v1 | clean-container run |

### 2.8 Git and GitHub

| # | Claude Code | Marlowe v1 | Today | Track | v1 | Test asserts |
|---|---|---|---|---|---|---|
| G1 | Commit only when asked | `git{op:"commit"}` Asks in default; style from `git log`; `persona/blocks/git.md` | - | J | v1 | Ask in default |
| G2 | Branches | `git{op:"branch"\|"switch"}` | - | J | v1 | |
| G3 | PR via gh | `gh{op:"pr_create"}`; one approval shows remote, branch, title, body | - | J | v1 | exactly one approval |
| G4 | PR review | `/review <pr\|diff>` -> 3 lenses; comments post only on approval | - | J, I | v1+ | |
| G5 | /pr-comments + address | `gh{op:"pr_comments"}`; address flow: fix commits, reply after approval | - | J | v1 | fake gh comments |
| G6 | /security-review | security lens on pending diff | - | I | v1 | planted vuln found |
| G7 | No force push / hook skip | hard deny `-f`, `--force*`, `+refspec`, `--mirror`, `--delete` of protected, `--no-verify`; no mode lifts it | P | J, E | v1 | A16 |
| G8 | CI status | `ci_status` + Sentinel `NoticeSource::Ci` | - | J | v1+ | failed run -> notice |
| G9 | Other git via bash | rebase, merge, cherry-pick, reset, tag, worktree stay allowed under rules | Y | J, E | v1 | rebase + cherry-pick via bash |
| G10 | Actions / @-mention bot | claude-code-action equivalent | - | J | SIGN-OFF 9.3-f | |

### 2.9 UX

| # | Claude Code | Marlowe v1 | Today | Track | v1 | Test asserts |
|---|---|---|---|---|---|---|
| U1 | Inline diffs | hunks in modal + results, palette tokens only | - | H | v1 | snapshot [R] |
| U2 | Clickable file:line | OSC 8; `editor_url` setting | - | H | v1 | escape bytes [R] |
| U3 | Image paste/attach | Ctrl+V CF_DIBV5/PNG, dropped path, `@img.png` | - | H, G | v1 | key replay -> `Part::Image` [R] |
| U4 | Status line + command | built-in + `statusline.command` | P | H | v1 | command output shown |
| U5 | Esc interrupt, steer | `CancelToken` + `Steer` | Y | - | v1 | existing |
| U6 | Queued messages while busy | input queue, sent at step boundary | - | H | v1 | key replay |
| U7 | `@` file mention | fuzzy over `ignore` walk | - | H | v1 | key replay |
| U8 | `!` shell | runs as user, output into context | - | H | v1 | key replay |
| U9 | Shift+Tab modes | cycle + indicator | - | H | v1 | key replay -> `SetMode` |
| U10 | Prompt history | Up/Down history at buffer edge, Ctrl+R search (today Up only moves cursor, `input.rs:838`) | - | H | v1 | key replay |
| U11 | Transcript/verbose view | Ctrl+O full tool I/O | - | H | v1 | key replay |
| U12 | /config | settings panel writing the right layer | - | H | v1 | key replay |
| U13 | /doctor | install, keys present (not values), LSPs, gh, Docker, Ollama | - | K | v1 | report fields |
| U14 | VS Code integration | extension `ide/vscode`: diffs in editor, selection/open-file context, IDE diagnostics, over 127.0.0.1 MCP with lock-file token | - | H2 | v1 | extension test host |
| U15 | JetBrains integration | | - | H2 | SIGN-OFF 9.3-e | |
| U16 | /vim, custom keybindings | | - | H | SIGN-OFF 9.3-c | |

### 2.10 Agent behaviour

| # | Claude Code | Marlowe v1 | Today | Track | v1 | Test asserts |
|---|---|---|---|---|---|---|
| B1 | Read before edit | T7 | - | A | v1 | T7 |
| B2 | Verify by running | stop check: files changed, no gate since last edit -> reminder | - | F | v1+ | reminder fires |
| B3 | Keep going | open todos + budget -> continue; 400-step cap, token/cost/time budget | P | F | v1 | 41st step runs |
| B4 | Track todos | T22 | - | F | v1 | T22 |
| B5 | Ask only when blocked | `persona/blocks/coding.md` + `ask_user` | - | F | v1 | iact suite |
| B6 | Secret hygiene | scrub; known secrets masked in results; secret-shaped files need approval to write/commit | P | A, J | v1+ | canary masked |

## 3. Architecture decisions

| Decision | Choice | Reason |
|---|---|---|
| New crates | `marlowe-code` (edit, FileState, tags, repo map, LSP), `marlowe-verify`, `marlowe-settings`, `marlowe-anthropic` | disjoint ownership; `marlowe-exec` stops growing |
| Registration tables | seam turns every central match into concatenation: `specs/<group>.rs::tools() -> Vec<ToolDef{spec, manifest, group, profiles, call, consequence, crew_consequence, describe}>`; `marlowe-exec/lib.rs` (`call` :330, `consequence` :364, `describe` :451), `marlowe-tools/builtin.rs::builtin_registry` + `exposure.rs`, `marlowe-crew/tools.rs` only concatenate | no track edits a shared match |
| Request routing | seam adds every new `Request` arm in `marlowe-daemon/client.rs::dispatch` (:96) routed to a stub `*_ops.rs` the owning track fills (section 7) | workspace compiles at wave 0 |
| Middleware | seam adds `ToolMiddleware{before_call, pre_mutation, after_call, after_step, before_final}` + registry in `run_tools.rs`; `TurnObserver{on_model_call, on_tools_done}` in `agent.rs`; `Convo.ext` typed map | A, B, C, F, F2 plug in without editing E's or F's files |
| Workspace | seam trait `Workspace{read, write, rename, walk, metadata, exec}` in `marlowe-os`; `LocalFs` (seam), `DockerExec` (K); A, B, C tools go through it | tb/vmini get the full tool surface in-container |
| Child spawn | seam moves `with_child_env`/`scrub` (`marlowe-exec/lib.rs:127,164`) to `marlowe-os::child`; every spawn (shell, hooks, LSP, services, project cmds, gh, train) uses it; per-launch `env_allow:[names]` shown in the approval | non-negotiable 5 by construction |
| `marlowe-permission` | rules engine + `shell/` parser and `classify_shell`/`classify_code` moved from `marlowe-crew/shell_policy.rs`; applied to conductor and crew shells in every mode | one adjudication point |
| Concurrency | std threads + crossbeam; LSP reader thread per server; Job Objects | CLAUDE.md |
| Tool schemas | flat, stable order, `additionalProperties:false`; lenient repair (trailing commas, raw newlines, Qwen XML calls) counted | local tool-call errors are the known failure |
| Tool exposure | frontier all groups; local core + detected groups, fixed per session; MCP behind `tool_search` over threshold | small models, warm cache |
| Step budget | per task; conductor 400 steps (`agent.rs:25` today 40) + token/cost/wall-clock | real refactors |

### 3.1 Tool surface (all args optional unless *)

| Tool | Arguments | Default consequence |
|---|---|---|
| `read` | `path*, offset, limit, pages` | Free (taints outside roots) |
| `write` / `edit` / `multi_edit` / `edit_lines` / `notebook_edit` | per 2.1 | Free in workspace, Ask outside |
| `glob` / `grep` | per 2.1 | Free |
| `bash` / `powershell` / `python` | `command*\|code*, cwd, timeout_ms, run_in_background, description, raw` | Ask; rules/modes may Free; tripwire always Ask |
| `job_output` / `job_watch` / `job_kill` | `id*, since, filter` / `id*, until_regex` / `id*` | Free (own jobs) |
| `todo` / `ask_user` / `skill` / `tool_search` | per 2.1, 2.6 | Free |
| `agent` / `send_message` | `type*, description*, prompt*, isolation, background` / `agent_id*, text*` | Free (budget-checked) |
| `exit_plan` / `enter_worktree` / `exit_worktree` | `plan*` / `name` / `keep` | Ask / Free / Free |
| `symbols`, `definition`, `references`, `hover`, `diagnostics` | `symbol\|query*, path, kind, limit, severity` | Free |
| `rename_symbol` | `symbol*, new_name*, path` | as multi_edit |
| `project`, `service`, `http_check`, `browser` | 3.8 | service start Asks (Free in crew); localhost Free |
| `git` | `op*(status\|diff\|log\|show\|blame\|branch\|switch\|add\|commit\|stash), args` | reads Free; writes Ask in default, Free in crew worktree |
| `gh` | `op*(pr_create\|pr_view\|pr_diff\|pr_checks\|pr_comments\|pr_comment\|issue_view), args` | Ask create/comment |
| `ci_status`, `experiments` | 3.9 | Free |
| `train_launch` | `command*, cwd*, run_dir*, gpu, metrics_glob, wall_clock_h, env_allow` | Ask always |

### 3.2 Edit formats per model class

| Model class | Formats | Reason |
|---|---|---|
| Claude | `edit`, `multi_edit`, `write` | str_replace post-training; CC parity |
| Local Qwen-family (dusk) | `edit` (whitespace tier), `multi_edit`, `write`; `edit_lines` bench arm | hashline-style anchors beat replace on 14/16 models in cited evidence; bench decides |
| valiance | bench decides (`edit` vs `edit_lines`) once Matthew allows | never run yet |
| GPT/Codex | `edit`, `write`; V4A `apply_patch` deferred (9.3-g) | no reachable GPT provider; A5 could not measure it |
| Unknown | `edit`, `write` | safest |

`edit_formats` per model in the model library (`marlowe-brains/src/library.rs`, L). Unified-diff
parse kept only inside `marlowe-code` for `rename_symbol`/WorkspaceEdit. Fuzzy tiers never pick
among >1 candidate; every result names its tier.

### 3.3 Code intelligence

| Layer | What | Notes |
|---|---|---|
| L0 | `ignore 0.4`, `grep-searcher 0.1`, `grep-regex 0.1` | pure Rust |
| L1 | `tree-sitter 0.27`, `tree-sitter-tags`, grammars rust, python, ts/tsx, js, go, c, cpp | per-repo sqlite `<home>\index\<repo-hash>.db` keyed (path, blake3); incremental |
| Repo map | PageRank over def/ref, 1.2k tok (32k) / 4k (frontier) | eval arm |
| L2 LSP | `lsp-server 0.10` + `lsp-types 0.97`; rust-analyzer, pyright, typescript-language-server | lazy; Job Object; idle stop 10 min; max 2 live; integrated branch only in crew, children use `cargo check --message-format=json`; none under `DockerExec` (L0/L1 fallback) |
| Diagnostics | push always, pull when registered; settle 800 ms quiet, max 5 s | |

tree-sitter is the one new C compile, and it is the reason for 9.1-i. Until 9.1-i lands, tree-sitter
deps sit under `[target.'cfg(not(target_arch = "aarch64"))'.dependencies]` (ARM64 builds without
L1); 9.1-i is a precondition for merging C, after which the aarch64 check runs in every gate.

### 3.4 Conductor context and sessions

| Area | Decision |
|---|---|
| Prompt order | stable prefix: persona v3 -> live `persona/blocks/*.md` (`coding`, `git`, `verify`, `ml`) -> tools -> instruction files -> project profile -> repo map -> skills index -> output style; history append-only; per-turn memory + env as a reminder on the latest user message |
| Caching | Anthropic breakpoints: tools, end of system, last stable message, latest; masking only at compaction (0.70), all eligible results in one chunk, so the prefix breaks once per compaction, not per step (`context.rs:19,181` today masks one result per step); eval arm rolling vs append-only |
| Prefix budget, 32k | persona+blocks 2.0k, tool schemas 3.0k, instructions 1.5k, profile 0.3k, repo map 1.2k, skills 0.4k, style 0.2k, slack 0.4k = 9k; tail reminder <= 1k. 64-128k: 16k; >= 200k: 30k, instructions uncapped |
| Over budget | drop skills index -> repo map tail -> MCP behind `tool_search` -> truncate instructions with a visible warning (`/context`, session-start notice); never silent |
| `num_ctx` | Ollama request sets `num_ctx` >= assembled prompt + max output, else error before send |
| Compaction | pinned working set (todos, plan, edit ledger, last failing output, user instructions) never masked; summary fallback; `/compact focus` |
| Turn persistence | `turns` appended per model call (`model, provider, usage, latency_ms`) and per tool batch (results); via `TurnObserver` (F2) |
| Checkpoints | shadow repo `<home>\checkpoints\<repo-hash>.git`, `--work-tree` = workspace, alternates-linked to the user repo's objects (read-only), persistent shadow index (stat cache, no re-hash), honours `.gitignore` + `info/exclude`; pre-images for ignored files Marlowe tools write; non-git dirs the same; size cap 2 GB with warning; user repo never written; prune 30 days |
| Rewind memory | journal `Retracted{session, turns}`; BeliefStore excludes those turns from retrieval (A12 re-run) |
| Subagents | `agent` on the turn thread's scope; `background:true` -> crew `dispatch` (never blocks) |
| Notices | job exit, `until_regex` match, background agent done -> `AgentEvent` injected next step; idle -> daemon starts a turn |

### 3.5 Permissions, modes, hooks, settings

| Area | Decision |
|---|---|
| Modes | `plan`, `default`, `accept_edits`, `bypass` (alias `auto`), `dont_ask`; headless Asks go to `--permission-prompt-tool` if set, else denied with reason (exit 2 only under `--fail-on-ask`) |
| Tripwire | conductor shells classified in every mode; hit -> Ask even in bypass; unparseable/indirect -> Ask |
| Fixed approvals | merge, push, deletes outside workspace, new network hosts, budget overrun, `train_launch` |
| Egress | first contact per host Asks unless in `network.allow_hosts`; install commands (npm/pnpm/pip/uv/cargo/go) map to their registry host; arbitrary binaries' egress is not enforceable without M12 (stated, 9.3-a) |
| Eval profile | `eval/coding/settings.toml`: allow pypi.org, files.pythonhosted.org, registry.npmjs.org, crates.io, static.crates.io, index.crates.io, github.com, raw/objects.githubusercontent.com, proxy.golang.org (9.2-l) |
| Hard denies | force push variants (G7), `--no-verify`, shared `git config` writes from crew, writes to `Marlowe_Harness` and the default profile |
| Hooks | all CC events + output schema (2.6); Job Object, scrubbed env, 60 s; project hooks trusted once by digest; `.claude/settings.json` import read-only |
| Taint | unchanged: after untrusted read, side effects outside the sandbox Ask in every mode |

### 3.6 build_v2 playbook

`BuildHost{progress, propose_plan, plan_approved, load, save, spawn_worktree, integrate, ci_status, profile, recall, remember}`.

| Stage | Who | Output (checkpointed) | Gate |
|---|---|---|---|
| 0 scope | lead + runner | repo map, profile, conventions; full gate run -> `baseline.json` (failing ids; each failure retried once -> `flaky`) | ask only if blocked |
| 1 plan | lead | task DAG `{id, goal, files_owned, interfaces, depends_on, accept_cmd}` + hub list + cost/time quote | plan approval; overlap in non-hub `files_owned` -> serial |
| 2 contract | lead | every module decl, dependency entry, migration slot, route stub, shared type, failing accept tests on `crew/<run>`; hub files frozen after | compiles; new tests fail for the right reason |
| 3 implement | engineers | child worktree per task; hub edits denied (child rule); need -> `need_hub_change` to lead, who amends and children rebase | accept tests pass, no new failures vs baseline, <= 4 rounds |
| 4 integrate | runner + integrator | `merge --no-ff` in topo order; lockfiles never by agent: take either side, regenerate (`cargo update -w`, `npm install --package-lock-only`, `uv lock`); textual conflicts -> integrator | build; on failure integrator gets errors, <= 2 rounds, then serial re-implementation of the task on the merged branch |
| 5 verify | runner + verifier | build, test, lint, format, typecheck; web smoke + screenshots | no new failures vs baseline (flaky retried) + accept tests |
| 6 review | 3 lens reviewers (correctness, security, performance), clean context | findings `{lens, severity, file, line, claim, evidence, fix}` | |
| 7 triage + fix | lead, engineer | fix commits | 0 blocker/major and stage-5 gate; failure set unchanged 2 rounds -> lead replan or `ask_user`; <= 3 rounds then budget ask |
| 8 deliver | - | diff, gate evidence vs baseline, review table, screenshots | merge approval; optional PR |

Build cost: one `CARGO_TARGET_DIR=<home>\targets\<repo-hash>` per repo shared by all children
(Cargo's lock serialises); shared pnpm store/npm cache, `UV_CACHE_DIR`. Run budget for A6:
5-task feature <= 3 h frontier, <= 12 h dusk (estimate; engineers serialise on one local slot).

### 3.7 Crew mechanics

| Area | Decision |
|---|---|
| Per-call checkpoints | `after_call` row (model output) + `after_tools` row (results); resume seeds both; synthetic "interrupted, re-check state" only for calls with no result row; never re-executed |
| Worktrees | `Sandbox::create_child` implements seam `WorktreeProvider`; `StepSpec.sandbox`; resume re-attaches |
| Integrator | thin, mechanical first (3.6 stage 4); origin merge stays `Sandbox::merge` after approval |
| Review | structured findings replace the "no blocking issues" string match |
| Roles | `lead`, `integrator`, `review-correctness`, `review-security`, `review-performance` |
| Inbox | `Supervisor::notify_run(run, notice)` (seam-crew stub, I implements) |

### 3.8 Verification harness (`marlowe-verify`)

| Piece | Decision |
|---|---|
| Profile | cargo, uv/pip/poetry + pytest/ruff/mypy, npm/pnpm/yarn/bun, go, cmake/ctest, make, compose, migrations; hub file list; `.marlowe\project.toml` override |
| Services | own Job Object, readiness by URL/port/log regex, killed at run/session end; `env_allow` |
| HTTP / browser | `http_check` on `marlowe-net`, localhost exempt; `EdgeRenderer` snapshot/screenshot/console, last 3 images kept |
| Migrations | throwaway DB only; user DB URL Asks |

### 3.9 Git/GitHub, ML engineering

| Area | Decision |
|---|---|
| Push/PR | never from crew shells; daemon `Request::OpenPr{run}` after merge approval, one approval |
| Credentials | `gh auth` token in Credential Manager; `GH_TOKEN` only via `env_allow` to gh/push child |
| CI | Sentinel polls `gh run list` for pushed branches, 60 s while running; failure -> notice + optional fix run |
| Training | `train_launch`: detached Job Object; `run_dir` watch root (9.2-g); GPU-busy check; `env_allow` (e.g. `HF_TOKEN`, `WANDB_API_KEY`) shown in approval; experiments row holds pid, create-time, deadline; Sentinel re-arms the cap on daemon boot (kills on deadline after pid+create-time match) |
| Tracking | `experiments` table: sha, diff hash, command, config, run_dir, status, metrics, notes |

### 3.10 Vision, bench, export

| Item | Decision |
|---|---|
| Model seam | `Part::Image{mime, blob, w, h}` (blake3 blob store); `ToolResult.parts`; `ModelInfo.supports_vision` |
| Providers | Ollama `images` (tool images on a following user message); OpenRouter `image_url`; Anthropic image blocks; long edge 1024/1568/2048 |
| Anthropic extras | thinking signatures round-trip across tool steps and steers; 529 backoff; `count_tokens` for the meter; 1M-context option |
| Bench | `marlowe bench <model> [--suite ...] [--tier ...] [--vs ...]`; writes `edit_formats` only with `--apply`; valiance refused until enabled |
| Export | `marlowe export trajectories`; opt-in per project; keep rule (crew merged / conductor `/good`); HF JSONL + step DPO; layered scrub + dry-run + manifest |

## 4. Frontier-model path (9.2-a, 9.2-k)

CC speaks only the Anthropic API, so the shared H2H model is a Claude model. **Recommended:** native
`marlowe-anthropic` (`ProviderKind::Anthropic` is stubbed), key in Credential Manager with an
`ANTHROPIC_API_KEY` env fallback for CI (read once, scrubbed); CC runs the same full model id on the
same key. OpenRouter is blocked (HTTP 403, over limit). Max-plan OAuth is never reused by Marlowe,
so Matthew's daily frontier coding in Marlowe is metered: 9.2-k decides a daily budget vs local-first
with per-task escalation.

## 5. What runs where

| Suite | Windows native | Container |
|---|---|---|
| own (worktree + hidden tests), polyglot (Py, Rust, JS, C++, Go) | yes | Java only |
| micro-suites, vision, memory, `iact` | yes | no |
| vmini 50 | no | yes, via `DockerExec`; nightly regression only, dropped if `DockerExec` slips |
| tb (Harbor) | no | Harbor on Python 3.12 (`uv python install 3.12`); Marlowe as external agent via `DockerExec`, CC installed in-container; **spike S1 (L0, wave 1)**: Docker Desktop started (or Ubuntu WSL2, 9.1-e) + one tb task end to end |

## 6. Coding eval

| Set | Size | Notes |
|---|---|---|
| own | 40 | Matthew's repos, tasks after 2026-06: 15 bugfix, 15 feature, 5 refactor, 5 ML; hidden tests; Matthew approves (9.2-h) |
| polyglot | 225 | Aider polyglot, 2 attempts |
| vmini | 50 | contaminated; regression only |
| tb | 89 | Terminal-Bench 2.1 |
| iact | 15 | scripted multi-turn (steer, ask_user answers via control messages) on both harnesses; rewind steps Marlowe-only, unscored vs CC |

| Tier | Content | dusk | frontier |
|---|---|---|---|
| Baseline (L0) | smoke at the pre-M4.6 commit | GPU window | once G lands |
| Smoke | own 10 + polyglot 20 + tools 200 + edit 120 | ~3-4 h | ~25 min |
| Nightly | own 40 + polyglot 60 + vmini 50 + tb 30 | overnight | ~1-2 h |
| Milestone | own 40 x3 + polyglot 225 + vmini 50 x3 + tb 89 | days | ~4-6 h |

Wave merge gate: no smoke task passing at baseline fails in 2 of 2 reruns.

| Arm | Purpose |
|---|---|
| H2H: Marlowe `-p --standalone --permission-mode bypass --settings eval` vs `claude -p --permission-mode bypassPermissions` | A1. CC: isolated `CLAUDE_CONFIG_DIR`, `--setting-sources` empty (or pinned version's equivalent), `--strict-mcp-config`, pinned version + full model id; thinking/effort set explicitly and equal; same `--max-budget-usd` and timeout per task; memory off |
| daemon path | own 40 x1 through the daemon (A1) |
| memory on/off | the edge, not gated |
| bash-only vs full (dusk) | A3 |
| core vs full exposure; repo map; LSP; rolling vs append-only masking; `edit` vs `edit_lines` | defaults by number |
| single agent vs `build_v2` | multi-agent only where it wins |

Reported per task: pass, steps, tokens, cost, wall-clock, prefill/decode, tool errors, repairs.

## 7. File ownership

| Path | Owner |
|---|---|
| `marlowe-model/src/{message,info}.rs`, `marlowe-proto/src/`, `marlowe-agent/src/tools.rs`, `marlowe-exec/src/lib.rs` + `specs/mod.rs` + `specs/web.rs`, `marlowe-tools/src/{builtin,exposure}.rs`, `marlowe-daemon/src/client.rs`, `marlowe-os/src/{child,workspace/mod,workspace/local}.rs`, stub crates, `Cargo.toml` members, `main.rs` clap stubs (`bench`, `export`, `mcp`, `doctor`, `eval`) | seam |
| `marlowe-crew/src/{tools,shell_policy}.rs` (table + re-export), `playbook/mod.rs` (`StepSpec.sandbox`), `supervisor.rs` (`notify_run` stub) | seam-crew |
| `marlowe-code/src/{edit,filestate,notebook,editlines}/`, `specs/files.rs`, `marlowe-exec/src/text.rs` | A |
| `marlowe-exec/src/{search,jobs,summarize}/`, `specs/{search,shell}.rs`, `marlowe-os/src/job.rs` | B |
| `marlowe-code/src/{index,repomap,lsp,udiff}/`, `specs/code.rs` | C |
| `marlowe-verify/`, `marlowe-exec/src/render.rs`, `specs/verify.rs`, `persona/blocks/verify.md` | D |
| `marlowe-settings/`, `marlowe-permission/src/{rules,shell/}`, `marlowe-agent/src/{run_tools,approval,hooks,convo}.rs`, `marlowe-daemon/src/mode_ops.rs` | E |
| `marlowe-agent/src/{agent,budget,memory,context,system,todo,subagent,instructions,commands,worktree}.rs`, `specs/agent.rs`, `marlowe-tools/src/skill.rs`, `marlowe-daemon/src/{conductor_ops,schedule_ops}.rs`, `persona/blocks/coding.md` | F |
| `marlowe-daemon/src/{checkpoint,session,store,session_ops}.rs`, `marlowe-agent/src/checkpoint.rs` | F2 |
| `marlowe-anthropic/`, `marlowe-provider/src/ollama/`, `marlowe-openrouter/src/`, `marlowe-model/src/image.rs` | G |
| `marlowe-tui/` | H |
| `ide/vscode/` | H2 |
| `marlowe-crew/src/{playbook/build_v2/,sandbox.rs,runner.rs,store.rs,supervisor.rs (after seam-crew)}`, `roles/` | I |
| `marlowe-exec/src/git/`, `specs/git.rs`, `marlowe-daemon/src/pr_ops.rs`, `marlowe-sentinel/src/ci.rs`, `persona/blocks/git.md` | J |
| `marlowe-crew/src/jobs.rs`, `marlowe-sentinel/src/experiments.rs`, `specs/ml.rs`, `marlowe-daemon/src/exp_ops.rs`, `persona/blocks/ml.md` | J2 |
| `marlowe/src/{main.rs (after seam), headless.rs}`, `marlowe-mcp/`, `specs/mcp.rs`, `marlowe-os/src/workspace/docker.rs`, `marlowe-daemon/src/mcp_ops.rs` | K |
| `eval/coding/`, `marlowe/src/{eval_entry,bench/}`, `marlowe-brains/src/library.rs` (`edit_formats`) | L0, L |
| `marlowe/src/export/` | M |

## 8. Tracks

Own worktree each; shared types only via seam. Cargo via `scripts/cargo-idle.ps1 -j2`, one build at a
time (shared target dir). Waves: 0 = seam; 0b = seam-crew (right after M4.5 merges); 1 = A, B, C, E,
G, K, L0; 2 = D, F, F2, H, H2, I, J, J2, L, M; 3 = integ. Each wave merges only on the smoke gate (6).

| Key | Scope | Model/effort | Depends | Acceptance |
|---|---|---|---|---|
| seam | registration tables, client.rs arms + stub ops, middleware/observer traits, `Workspace`, `marlowe-os::child`, `WorktreeProvider`, proto variants, `Part::Image`, main.rs stubs | opus/high | - | workspace check green; every existing tool reachable via tables (existing tests pass); serde round-trips; old turns JSON loads |
| seam-crew | crew tool table, `shell_policy` re-export, `StepSpec.sandbox`, `notify_run` stub | opus/high | seam, M4.5 merged | crew tests unchanged and green |
| A | T1-T9, T19, B1, B6 writes; FileState watcher middleware | opus/high | seam | `parity_` T1-T9, T19; 150-case fixture; A5 harness |
| B | T10-T18, T32; job notices; Git Bash/PowerShell via `marlowe-os::child` | sonnet/high | seam | `parity_` T10-T18, T32 |
| C | T24-T27; LSP; tags; diagnostics middleware; cfg-gated tree-sitter | opus/high | seam; merge needs 9.1-i | `parity_` T24-T27; A8; aarch64 check green |
| E | M1-M12, X1-X9, G7/G9 rules; shell parser + corpus; session allows by pattern | opus/high | seam | `parity_` rows; A16; one test per hook event and output field |
| F | C1-C15, A1-A13 (exc. A9, A12), X10, X11, X17, X19, B2-B5, T16/T22/T23 logic; prefix budget; append-only context | opus/high | seam | `parity_` rows; A14 (with G), A15; real CC-format fixtures |
| F2 | S1-S7; per-call + per-tool-batch rows; shadow-repo checkpoints; `Retracted` | sonnet/high | seam | `parity_` S-rows; A7 matrix |
| G | Anthropic provider (3.10 extras), vision in all providers, caching, repair, env key fallback | sonnet/high | seam | recorded SSE incl. thinking signatures over 5 tool steps + steer, 529 retry; live smoke behind 9.2-a (`cache_read > 0` on call 2); repair corpus 50 |
| H | U1-U13, U16 surfaces, M10, X16, C6/C9/C14/C15, S1/S3 pickers | sonnet/high | seam | key-event replay tests (scripted crossterm stream -> expected Requests + screens); palette theme test; `[R]` checklist script |
| H2 | VS Code extension (U14) | sonnet/high | seam, K | extension test host: diff opened, selection sent, diagnostics read |
| I | build_v2 (3.6), crew mechanics (3.7), A9, A12, G6 | opus/high | seam-crew | scripted host: 3-task DAG; hub frozen; lockfile regen; stage-4 build failure -> serial fallback; pre-existing failing test does not block; flaky retried; stall -> replan; security blocker fixed; killed after tools resumes without re-run |
| J | G1-G10 (exc. G6, G10) | sonnet/high | seam | fake `gh`: one approval PR; pr_comments flow; force variants denied all modes; rebase/cherry-pick via bash |
| J2 | `train_launch`, experiments, Sentinel bridge, boot re-arm | sonnet/medium | seam-crew, I | CPU dummy trainer: metrics read, row written, plateau notice reaches inbox; restart re-arms deadline; `env_allow` only names approved |
| K | H1-H7, X12-X15, T31, U13; `DockerExec` | sonnet/high | seam | goldens; exit matrix; prompt tool + interrupt; fake MCP stdio/HTTP/OAuth; edit a file inside a container via `DockerExec` |
| L0 | `eval` entry, own + polyglot runners, baseline runs, spike S1 | sonnet/high | seam | baseline report at the pre-M4.6 commit; S1 verdict recorded |
| L | full harness, tiers, arms, `claude -p` adapter (isolation flags), Harbor agent, `marlowe bench` | sonnet/high | L0, K | `pytest eval/coding`; smoke end to end on scripted provider; CC adapter dry-run shows isolation flags |
| M | export + scrubber | opus/high | seam | A11; dry-run report; off by default |
| integ | merge waves, gates incl. aarch64 check, LongMemEval, parity walk, H2H, A6 | opus/high | all | A1-A16 recorded in CHANGELOG + this doc |

## 9. Approvals Matthew must give

### 9.0 Matthew's decisions (2026-10-09) — these override the recommendations below

- **Local only, $0.** No paid API anywhere in M4.6 (9.2-a/b/c/k/o lapse). Track G keeps vision for
  Ollama; the native Anthropic provider waits until Matthew funds a frontier path.
- **H2H (A1) on the same local model:** Claude Code pointed at Ollama's Anthropic-compatible
  endpoint (Ollama 0.32.5 serves `/v1/messages`) vs Marlowe, both on `marlowe-dusk:27b-super`, CC
  config isolated from Matthew's own. A pure harness comparison; no frontier arm.
- **Installs approved:** 9.1-a (`gh`; Matthew runs `gh auth login`), 9.1-b, 9.1-c, 9.1-i (after
  M4.5 merges, never during a build). **Not approved:** 9.1-d (uv, Go), 9.1-e (Docker/WSL), 9.1-f
  (eval venv). So: the A3 bash-only baseline is Marlowe's own shell-only ablation arm (no
  mini-SWE-agent); polyglot uses only the toolchains already installed; tb/vmini are unscored.
  9.1-g (crates) and 9.1-j (project-local npm dev deps) are build dependencies of their tracks.
- **Every 9.3 item is deferred** until the load-bearing parity work is in ("Claude Code is the
  goal right now"); they return after M4.6's core.
- Still open: 9.2-h (Matthew picks the own-repo tasks and the A6 specs/repo before eval runs).

### 9.1 Installs and system changes

| # | Item | Why |
|---|---|---|
| a | `winget install GitHub.cli` + `gh auth login` | PR/CI |
| b | `rustup component add rust-analyzer` | Rust LSP |
| c | `npm i -g pyright typescript typescript-language-server` | Py/TS LSP |
| d | `uv`, Go (winget); `corepack enable`; `uv python install 3.12` | stacks, polyglot, Harbor |
| e | Start Docker Desktop's WSL2 backend, ~60 GB; fallback: install Ubuntu WSL2 distro | tb, vmini |
| f | Python 3.12 venv: `harbor`, mini-SWE-agent (pinned) | eval runners |
| g | Rust deps: tree-sitter + `tree-sitter-tags` + 7 grammars (C compile), `ignore`, `grep-*`, `lsp-server`, `lsp-types` | code intel |
| h | GPU windows (overnight) for dusk baseline/smoke/nightly | A3-A6 |
| i | VS Installer `VC.Tools.ARM64` + `VC.Llvm.Clang`; `rustup target add aarch64-pc-windows-msvc` | ARM64 gate; precondition for merging C |
| j | `ide/vscode` dev deps (`@vscode/vsce`, `@types/vscode`) and installing the local `.vsix` | U14 |

### 9.2 Decisions

| # | Decision | Recommendation |
|---|---|---|
| a | Frontier path (section 4) | native Anthropic provider; key in Credential Manager; host `api.anthropic.com` |
| b | Eval spend cap | ~330 runs per harness at milestone, estimate $300-1,000 (unmeasured); cap $1,000 |
| c | CC billing for H2H | same API key, not Max |
| d | ADR-052 amend: MCP HTTP + OAuth via `marlowe-net`, host approved at add | yes |
| e | Read `.claude/` agents/commands/skills/rules, `.mcp.json`, and `.claude/settings.json` hooks + permissions as read-only compatibility | yes, hooks trusted by digest |
| f | Conductor edits Free inside workspace (CC asks) | yes; rewind is the undo |
| g | `train_launch` approval registers `run_dir` as watch root | yes |
| h | Own-repo 40 tasks; A6: 2 pre-registered feature specs + a target repo with GitHub Actions CI | Matthew picks before runs |
| i | valiance blocked in bench until Matthew says | yes |
| j | Export opt-in per project, local only | yes |
| k | Daily frontier budget for interactive Marlowe (Max OAuth never reused) | local-first, escalate per task, $X/day cap Matthew sets |
| l | Eval network allowlist (3.5) | yes, eval profile only |
| m | tb in A1 only if spike S1 passes; else tb is an unscored arm | yes |
| n | `Retracted` journal event excluded from retrieval (A12 re-run) | yes |
| o | `ANTHROPIC_API_KEY` env fallback for CI | yes, scrubbed from children |

### 9.3 Proposed deferrals (need sign-off, else they move into v1)

| # | Row | Proposal |
|---|---|---|
| a | M12 OS filesystem/egress sandbox (AppContainer/low-IL) | M7; v1 = worktree + Job Object + tripwire + policy; arbitrary-binary egress unenforced until then |
| b | X18 plugin marketplaces | local bundles in v1 |
| c | U16 /vim, custom keybindings | M7 |
| d | M6 classifier-gated auto mode | after M4.6; `bypass` + tripwire in v1 |
| e | U15 JetBrains plugin | after VS Code proves out |
| f | G10 GitHub Actions / @-mention bot | M7 |
| g | V4A `apply_patch` (beyond CC) | until a GPT provider is reachable |
| h | Declarative playbooks, best-of-N implement, UI/ML lenses (beyond CC) | only after an eval arm shows a gain |

## 10. Risks

| Risk | Mitigation |
|---|---|
| Local tool-call errors (~12% class) | repair layer, flat schemas, edit profiles; A4 |
| 32k context thrash | prefix budget A15, drop order, append-only masking arm |
| H2H noise (~9 pts spread) | 3 seeds, paired bootstrap, CI-bound gates, pinned CC |
| CC improves during M4.6 | pin at H2H start; re-run at close |
| tb/Harbor fails on Windows | spike S1 in wave 1; tb unscored if it fails (9.2-m) |
| Hub-file serialisation | contract stage freezes hubs; mechanical lockfile regen |
| Cold builds per child | shared target dir/caches; LSP on integrated branch only |
| Pre-existing failing/flaky tests | baseline.json, no-new-failures gates, stall detection |
| Build contention | idle priority, `-j2`, one build at a time |
| Repo hooks run code | digest trust; taint unchanged |
| Egress by arbitrary binaries | tripwire + registry mapping; full fix is 9.3-a |
| Scrub misses PII | local, opt-in, canaries, dry-run |
| M4.5 in flight | seam-crew and I start after it merges |
| Scope (20 tracks) | parity matrix + `parity_` tests are the checklist; integrator fails any unsigned gap |

## 11. Review resolutions (variants and rejections only; every other finding is fixed in place)

| Finding | Resolution |
|---|---|
| F exposes `on_model_call` for F2 | variant: seam defines `TurnObserver`, so F2 does not wait on F |
| `Sandbox::create_child` stub in seam | variant: seam `WorktreeProvider` trait in `marlowe-agent`; crew stays untouched until M4.5 merges |
| Cut `statusline.command`, `--input-format stream-json`, local plugin bundles | rejected: all three are CC features; parity is the floor |
| Cut declarative playbooks, best-of-N, extra lenses | accepted as cut; listed in 9.3-h for visibility |
| IDE row | VS Code in v1 (H2); JetBrains to 9.3-e |
| Classifier auto mode | renamed ours `bypass`; classifier mode to 9.3-d |
