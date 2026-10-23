# Crew watch: design

Status: design only, nothing built. Palette locked, no fills, glyphs and foreground ink only. No new colours.
Numbers marked `~` or "target" are estimates or unmeasured goals. Section 9 lists what the real code cannot do yet.

---

## 1. Summary (8 bullets)

1. **One object.** A **task** (rail/window name stays **Crew**). Three tags: `team` (Starlark script), `solo` (a dispatch: one specialist, own worktree), `helpers` (the `agent` tool's quick helpers, one synthetic row). Task -> phase -> agent -> step. The words "run", "dispatch", "subagent", "one-specialist team task" leave the UI.
2. **One store, four zooms.** Rail row, task list, task view, agent view are the same data at four densities, from one client `TaskStore` built from `CrewRunInfo` + `WorkflowInfo` + `TaskInfo` + new samples. Every fact appears once per zoom. The duplicate strip row, `/teams` and the crew half of `/tasks` go away.
3. **The slot strip is the crew's context meter.** Six braille cells, one per llama.cpp slot, in the rail header and window header. One click opens a planner-style **Slots & crew** popup that links to Context & hardware.
4. **Urgency order everywhere:** needs you, failed, working, queued, done. "N working" is never shown without the blocked count. Blocked-on-you shows in three places: count, row, toast (plus terminal title).
5. **Every agent row answers "what is it doing, how full, how fast".** Glyph, name, one now-line (derived from events, no LLM), context bar with the `┊` 70% mark, cache %, tok/s, slot, one fixed-width clock.
6. **Drill is all clickable, no dead ends.** Rail -> task -> agent in 2 clicks. Esc goes up one level. Every footer hint is a button. The composer stays at the bottom of every crew view, so you keep talking (zero-wait).
7. **Steer anything, see it land.** One steer box at every level, scope defaults to the selection. Delivery ladder: `queued` -> `read at step N` -> (or) `not delivered`. Needs a per-agent inbox. Approvals happen inline on the agent row/card, naming the agent.
8. **Alive between events.** Running crew work makes the cadence Live (crew regions at 80 ms spinner frame, focused stream at refresh rate). Wire: 4 Hz ephemeral per-agent samples, ordered lifecycle events, text only for the opened agent. About 5 KB/s instead of 15-30 KB/s.

---

## 2. The model

```
Task (row)  tag: team | solo | helpers
 └ Phase*      from the script; solo has one hidden phase; helpers none
    └ Agent*   stable id  t7.a3  (solo: t9.a0, helper: h.a12)
       └ Step* one model turn -> tool chips, text, a question, a steer received
```

| Today | Becomes | Notes |
|---|---|---|
| Starlark team task | `team` task | phases, N agents |
| Dispatch (crew run) | `solo` task | no phase strip; Enter lands straight on the agent view. Breadcrumb still `Crew › fix-flaky-test` |
| `agent`-tool helper | agent in the `helpers` task | one row, steps down to dim weight 30 s after the last helper ends. After commit `7c30d58` helpers are ordinary pool agents, so same rows. Foreground helpers become visible (today invisible) |
| Conductor | slot 0 in the strip only | never in the list |

**Identity.** `AgentId` rides on every sample, lifecycle event, approval, steer, message, toast. Script agents: `AgentCall.index` + `StepSpec.key`. This replaces the broken join between `WorkflowAgentInfo.index` and `RunStep.step`.

**Consolidation plan.**
- `TaskStore` (client) replaces three feeds as the single reader. `TaskUpdated` for crew rows is dropped client-side, then server-side.
- Rail section "Crew" owns crew rows. The background strip stops listing crew items.
- `/teams` retired; `/tasks` keeps jobs, monitors, cron and a "Crew ->" link. Saved scripts move to the Crew window's `^d` new-task prompt.
- Header summary becomes crew-only (`1 needs you`). Watch experiments stop being counted as "runs".
- The research stage strip and build DAG strip fold into the one phase strip (both playbooks emit phases).

### The whole workflow (every step is a click or one key, no dead end)

| Step | You see | You do | If it goes wrong |
|---|---|---|---|
| 1 Deploy | Conductor says one line with a `[open]` card. Rail row appears at once: `○ spinning up: creating worktree…` (real Spinup stages) | nothing; keep typing | Script error: row `✗` with error line, `e` shows log |
| 2 Keep talking | Composer is never covered, in the conversation or in any Crew view | type | A queued agent says what it waits for (section 6) |
| 3 Glance | Rail: slot strip, one row per task, amber count if blocked | nothing | Amber count appears in header, rail, title bar, toast |
| 4 Drill | Click row -> task view -> click agent -> agent view (solo task: one click) | `esc` goes up | Breadcrumb is also clickable |
| 5 Steer | Steer box at every level, scope defaults to selection | type, `enter` | Ladder: queued -> read at step N -> or `not delivered` + resend |
| 6 Approve | Amber `◐` row, `g`/toast jumps to it, card in place | `y` / `n` | Deny one call (`esc`) or stop (`x`) |
| 7 Results | One clickable card in the conversation, Result tab with hunks | `m` merge, `D` discard | Merge conflict: agent `✗`, open diff or steer to fix |
| 8 Follow-up | Card has `[continue]`: fills the composer with `@t7 ` so the conductor has the result in hand | type | Failed task: `[retry]` reruns from the journal (cached calls skipped) |
| 9 Recover | `↺ interrupted` after a window close or crash; resume lists what reruns | `enter` | Replay gap reloads snapshot, keeps your place |

---

## 3. The widget (rail)

Rail ~44 columns. Question it must answer without a click: *is everything fine, who is doing what, who needs me.*

**Collapsed (default, 6 rows):**
```
╭ Crew ────────────────────── 1 needs you ╮
│ ⣿⣷⣷⣷⠶⠂  5/6 slots · cache 91% · 14m     │
│ ◐ api-refactor  4/6 ━━━╺── ask: merge?   │
│ ⠹ evals-sweep   2/2 ━━━━━╺ edit run7.py  │
│ ✓ lint-pass     done 2m ago             │
│ + helpers 2 working                     │
╰─────────────────────────────────── alt-r ╯
```
**Expanded** (click the `Crew` title; auto when exactly one task needs you): agents indent 2 cells under the task.
```
│ ◐ api-refactor  review 3/4  ━━━━━╺───   │
│   ⠹ #1 types     ▸ edit lib.rs      14s  │
│   ⠴ #2 tests     thinking           4s   │
│   ◐ #3 merge     merge into main?  41s   │
│   ○ #4 docs      after tests         –   │
```

**Slot strip cells** (position = slot number, so spatial memory):

| Cell | Ink | Meaning |
|---|---|---|
| `⣿` first | speech `#b39ae8` | slot 0, conductor, pinned (dim when idle) |
| braille level 1-4 | accent | agent decoding; level follows smoothed tok/s |
| prefill fills `⠁⠃⠇⡇⣇⣧⣷⣿` | accent new, blue cached share | prefill progress |
| `⠶` | structure | thinking / tool (steady) |
| `◐` | amber, no motion | holder needs approval (calm = waiting on you) |
| `⠂` | dimmer | idle but cache-warm |
| `·` | dimmer | free |
| `?` | dim | Ollama fallback: label reads `slots n/a (Ollama)`; never a fake map |

Failed cell: red for 2 s then free. Done: one green flash 400 ms. Click a cell opens that holder; click the label opens the Slots popup.

**Row anatomy:** glyph (worst-case agent state) · name (12) · `done/total` · 6-cell phase bar (`━` done, accent `╺` leading edge, `─` rest) · now-line (flex). Now-line: the question (amber) if blocked, freshest tool/output if working, result if done.

**Rules.**
- Sort needs you, failed, working, queued, done. Done folds after 30 s (stays in the window list for the session). Failed stays until opened. Max 3 task rows plus helpers; overflow `+2 tasks · alt-r` keeps the amber count if hidden ones need you.
- Priority when narrow (pinned by a test like `the_meter_drops_the_bar_first`): bar, then `done/total`, then cache %, then clock, then the word `slots`. Never drops: glyph, name, needs-you count/question. Under 28 columns: glyph + name + one-letter state.
- Hover = `lit()` one weight up, never a fill. Row click opens task (or agent if solo); now-line click opens the asking agent; strip click opens popup.
- A reorder gets one 300 ms one-weight lift on the moved row. No sliding.
- `5/6 slots` repeats the strip as a number on purpose: the strip is for the eye, the number survives when the strip is dropped at narrow widths.
- **Scale (20 agents).** Expanded shows at most 6 agent rows, worst state first (needs you, failed, working), then `+14 · 3 done`. The task row never loses its phase bar and `done/total`. Done agents fold into one `✓ 9 done` row.

---

## 4. The window

`View::Crew` becomes a drill stack with breadcrumb: `Crew › task › agent`. Esc up one level, alt-r toggles to the conversation and back to exactly where you were (`CrewNav {task, agent, tab, scroll, draft}` kept per task). A one-line conductor composer stays pinned at the bottom (`you ›`), above it the steer box. At 150+ columns it is master/detail (left tree, right selected thing); below, a stack.

Ink key for all mockups: `NEEDS YOU` headings amber; other headings dimmer; accent = selection marker/name, working glyph/bar, hotkey letters, fact-grid labels; numbers body, units dim, separators dimmer; green `✓`, red `✗`, amber `◐`, blue = cache only (one meaning). Amber is state only, never an ordinary number (planner dialect, not the strip's amber numbers).

**Scale (20 agents in 30 rows).** The agent table is virtualised and scrolls (wheel, `j`/`k` is not used, arrows). Over 12 agents, done agents fold into one `✓ 9 done` row per phase (Enter expands it) and the phase strip becomes the filter. Header, strip, events, steer, hints and composer take 9 rows, so 20 agents never push the steer box off screen.

### 4a. Task list (120x30)
```
 Crew   3 tasks · 8 agents     ⠹ 5 working  ◐ 1 needs you  ✗ 0  ✓ 2 done              local · 41k tok
 ⣿⣷⣷⣷⠶⠂  5/6 slots · cache 91% · 2 queued                                s steer  g next needing me  / filter
 ──────────────────────────────────────────────────────────────────────────────────────────────────────────
 NEEDS YOU
›◐ api-refactor    team   review  3/4  ━━━━━━╺───  2m41   merge api-refactor into main?    [y]es [n]o [v]iew
 WORKING
 ⠹ evals-sweep     team   run     2/2  ━━━━━╺────  9m12   edit eval/run7.py · #2 thinking
 ⠴ fix-flaky-test  solo   step 6        ━━╺───────  0m48   ▸ run cargo test -p marlowe-net
 ⠙ helpers         ×2                               0m12   #h1 explore: grep "slot" · #h2 plan
 QUEUED
 ○ bench-sweep     team   –       0/4                0m41   waits: all 5 agent slots busy · longest holder #2 evals 9m
 DONE (folds in 30 s)
 ✓ lint-pass       team   done    3/3  ━━━━━━━━━━  4m02   +3 −1 files · ready to merge      [m]erge
 ──────────────────────────────────────────────────────────────────────────────────────────────────────────
 ↵ open  space peek  s steer  p pause  x stop  m merge  f filter  ^d new  g next  esc back     (all buttons)
 › you: type to the conductor…
```
Peek (`space`): rounded accent popover under the row: full goal (3 lines), whole now-line, phase counts, budget bar, last 3 output lines, how long it has waited on you. On a needs-you row `y`/`n` answer the approval, digits answer numbered questions. A real approval always goes through the permission path, never a steer.

### 4b. Task view (120x30)
```
 Crew › api-refactor   team · 4 agents · branch marlowe/api-refactor                  ‖ pause   ■ stop   esc
 ◐ review · 2m41 · tokens ━━━━━━━┊───  41k / 120k      slots ⣷⣷⠶◐⠂·   cache 92%   local
 research ✓ 2 ─ build ✓ 3 ─ ▸ review ◐1 ⠹2 ─ publish ○ 1                (click a phase to filter)
 ───────────────────────────────────────────────────────────────────────────────────────────────────────
    #   agent        role      slot now                              ctx          cache  speed   time   tokens
›⠹ #1  types-core    engineer  s2   ▸ edit crates/api/src/lib.rs    ━━━┊───  74%  c 92%  31 t/s  2m41  18.4k
 ⠴ #2  tests         engineer  s3   thinking 4s                     ━━┊────  38%  c 88%    --    1m52   9.1k
 ◐ #3  merge         engineer  s4   merge into main?   [y] [n]      ━━━┊───  66%  c 95%    --    0m41  11.0k
 ○ #4  docs          writer    –    waits for #2 (tests)             –          –      --      –      –
 ✓ #5  survey        explorer  s5   3 callers of old ApiError       ━━┊────  29%  c 90%    --    0m58   2.6k
 ───────────────────────────────────────────────────────────────────────────────────────────────────────
 events  2m41 #3 asks merge · 2m38 #1 edit lib.rs ok · 2m20 #2 steer read at step 5 · 2m02 phase build done
 steer › to: all ▾  tell the whole task…                                      ↵ send · tab target
 ↵ agent  space peek  s steer  p pause  x stop  r retry task  f filter  e events  [ ] flip  esc back
 › you: type to the conductor…
```
- Columns drop right to left when narrow: tokens, speed, cache, ctx, slot. Never: glyph, name, now-line, clock.
- ctx bar: accent under 70%, amber from 70%, red from 90% (same `used_ink` rule). `c 92%` is blue when over 80%, amber under 40% with the reason on peek (`first request` / `prefix changed`). `~` prefix while an estimate. `$` only on cloud models, local says `local`. `--` speed means not decoding right now.
- The `events` line is the only log (needs P3 lifecycle events; absent in P2). The stream is the only place output text appears.
- Inline approval: Enter or click on a `◐` row expands a rounded card in place (see section 6).

### 4c. Agent view (160x45, master/detail)
```
 Crew › api-refactor › #1 types-core                      engineer · qwen3-coder:30b · medium     ‖ pause  ■ stop  ↺ retry
 ⠹ tool edit crates/api/src/lib.rs   3.1s   step 7                                                slot 2 of 6 · warm
 TASKS                        │ Context  ━━━━━━━┊────  24.3k / 32k  74%                │ INSPECTOR (150+ cols only)
 ◐ api-refactor  review 3/4   │ Cache    22.4k cached · 1.9k new · 92% · read 0.4s     │ FILES  M lib.rs   +41 −9
  ✓ #5 survey                 │ Speed    ⣀⣤⣶⣿⣷⣶⣤⣶ 31 t/s decode · read 5.9k t/s      │        M error.rs +14 −3
 ▸⠹ #1 types-core             │ Tokens   in 41.2k · out 6.8k of task 120k · 38%        │        A retry.rs  +7 −2
  ⠴ #2 tests                  │ Sandbox  wt/api-refactor/types-core · +62 −14 · 4 files│ HISTORY
  ◐ #3 merge                  │ Steer    none pending                                  │ 2m20 you › use anyhow   read st 5
 ⠹ evals-sweep   run 2/2      │ ──────────────────────────────────────────────────────│ 1m05 ◐ run cargo test  you y ok
 ✓ lint-pass     done         │ STEPS ✓1 read ✓2 plan ✓3 edit ✓4 edit ✓5 test ⠹7 edit │
                              │ 6 ✓ tool edit  error.rs   ok 0.8s  +14 −3               │
                              │ 5 ✓ tool run   cargo test -p api  ok 12.4s · 24 passed  │
                              │ 2 ✓ think 4.2s  plan: split error type, keep public API │
                              │ OUTPUT (follow ▾)                                       │
                              │ I'll replace the bespoke ApiError with a thiserror enum │
                              │ ▸ edit lib.rs  @@ -12,7 +12,11 @@                        │
                              │   - pub use error::ApiError;   + pub use error::{ApiError, Result};
 steer #1 › say something to types-core…            applies after its current tool call · ↵ send · tab to: task
 ↵ expand  s steer  p pause  x stop  r retry  M model ◂▸  d diff  t tool detail  [ ] flip  g next  esc back
 › you: type to the conductor…
```
- Fact grid is planner dialect: 7-wide accent labels, body values, dim units, dimmer separators.
- The header line is the one now-line with the one clock (current call). The running step is not repeated below it; the STEPS list shows finished steps. Sparkline lives in the Speed row only. Inspector holds only what the grid lacks (files, history).
- Agent prose is body ink (speech colour is the conductor's voice). Diffs use the conversation's `diff.rs` renderer; `+`/`−` counts green/red.
- At 120x30: tree and inspector fold away; Context/Cache/Speed/Tokens stay in the grid, steps become one line, files move to a `d` popup. Nothing lost, one key further.
- `[` `]` flip to neighbouring agents without going up.

### 4d. Slots & crew popup (planner dialect, modal, for a decision)
```
╭─ Slots & crew · marlowe-dusk:27b-super ─────────────────────────────── esc ─╮
│ Engine llama.cpp · 6 slots · ctx 32k each (q8) · weights 9.8 GiB · cache ≤8.0 GiB
│
│  slot  holder              state       ctx            cache   idle-warm
│  0     you (conductor)     writing     ━━┊────  38%    94%    –
│  1     #h1 explore         tool        ━━┊────  29%    90%    –
│ ›2     api-refactor #1     tool        ━━━┊───  74%    92%    –
│  3     api-refactor #2     thinking    ━━┊────  38%    88%    –
│  4     api-refactor #3     ◐ approval  ━━━┊───  66%    95%    holding slot 41s
│  5     –                   idle         –          81%    warm 3m
│
│ Queue       1 waiting: docs #4 (after tests)
│ Read-ahead  prefilling slot 5 with the shared team prompt (2.1k tokens)
│ ↵ open holder   p planner (context & hardware)
╰─────────────────────────────────────────────────────────────────────────────╯
```
Rules from the planner: rounded accent border, dim `esc` top right, `›` accent selection, `–` for zeros, amber/red notes appended per row, scrim behind (dimmer, no fill).

---

## 5. Steering, approvals, results

**Steer.** One box per level, same widget, always visible. Scope defaults to the selection; `tab` cycles agent / phase / task. Nothing selected means the conductor.
- Semantics: appended at the agent's next step boundary ("applies after its current tool call"). Never rewrites sent history, so no prompt cache breaks. Not-yet-started agents get it as a prefix to their start prompt.
- Ladder shown on the row and in History: `› queued` (body) -> `› read at step 7` (body, steps to dim after 5 s) -> or `› not delivered · agent finished` (amber, offers resend to the task). A steer that lands cannot be recalled; the box says so. Unread over 60 s turns amber: `in a long tool call`.
- Task-wide steer fans out as N per-agent steers with one shared id: `all · read by 3/4`.
- Quick chips (one key each): `wrap up`, `redo step`, `skip` (script `agent()` returns null), `+budget` (goes through approval), `model ◂ ▸` / effort for the next call (shown pending), `retry`.
- Pause per agent takes effect at a step boundary. Default: releases the slot; the cache stays warm only if no other agent reuses that slot, so resume shows `cache warm` or `cache cold, rereading ~Nk` (D4).
- Retry in v1 is whole-task resume from the journal (finished `agent()` calls are cached, the rest rerun, with a preview). Retrying one agent inside a live script needs host support that does not exist (section 9, #7).
- Steer text is user-trust guidance. It can never act as an approval, and History lines for approvals are distinct and name who approved.
- Persisted in the db; the morning brief can say what you told whom.

**Approvals inline.** An approval attaches to an agent. The row shows `◐` and the exact ask; Enter/click expands a card in place:
```
 ◐ #3 merge  engineer  s4  st 9
   ╭ asks to merge wt/api-refactor into main ───────────────── 41s ╮
   │ +62 −14 · 4 files · tests ok (24 passed) · ⚑ not tainted       │
   │ [y] approve  [n] deny  [d] diff  [s] steer first                │
   ╰─────────────────────────────────────────────────────────────────╯
```
- Card facts: diff summary, taint flag (finally shown; amber `⚑ untrusted content entered the run`), host name for new network, bar and `+20k tokens` for budget overrun.
- Esc denies that one call without stopping the agent; `x` denies and stops. No agent text is ever approval.
- The global modal remains for the conductor's own approvals; for crew approvals it names the agent and gains `o open`. Merge/push use the same card (D2).

**Results.**
- One relay per task: the conversation gets a clickable card `✓ api-refactor · 4 agents · 4m02 · +62 −14 · 4 files · [open] [merge]`. The conductor still receives the full text; the card is the only copy you see.
- Result tab: summary, per-file diffstat, `d` for real hunks (conversation diff renderer), sources, and clickable merge/discard. A pending merge shows as `merge waiting for you` in the same tab, closing the m -> flash -> modal -> back loop.
- Failures relay one line with the first error and `[open]`. Restart shows what reruns before it starts (`#1-#4 cached, #5 #6 rerun`).
- Follow-up: `c` / `[continue]` seeds the composer with `@t7 ` so the conductor works from the result (P5). Later: compare up to 4 agents, `@a3` references to single agents.

---

## 6. States and attention

One glyph + ink + now-line + clock per state. Waits always say "waits for X", never bare "queued".

| State | Glyph (ink) | Now-line | Clock = |
|---|---|---|---|
| queued, phase | `○` dimmer | `waits for phase tests (2 of 3 done)` | time waiting |
| queued, slot | `○` dim | `waits: 6/6 slots busy · longest holder #2 9m` (no ETA: none is measured) | time waiting |
| queued, model load | `○` dim | `model loading 4.1 / ~9.0 GB` | load clock |
| spin-up | braille accent | `creating worktree…` (real Spinup stages) | spin-up |
| prefill | braille accent | `reading 3.2k new · 9.1k cached 63%` | time in prefill |
| thinking | braille accent | `thinking 4.1s` (reasoning stream forwarded) | time in call |
| tool | braille accent | `▸ edit lib.rs (+3 −1)` | time in tool |
| writing | braille accent | last output line | time in step |
| needs approval / question | `◐` amber | the exact ask | time waiting on you |
| steered | blue `›` chip | `queued` / `read at step 7` | time since send |
| paused | `‖` dim | `paused at step 7 · slot released` | frozen |
| stalled (>30 s silent) | spinner stops, amber `…` | `silent 34s · last: bash cargo bench` | time silent |
| failed | `✗` red | first error line + `r retry` | frozen at duration |
| done | `✓` green | result first line or `+3 −1 files` | frozen at duration |
| cancelled / skipped | `–` dimmer | `stopped by you at step 7` | frozen |
| interrupted | `↺` amber | `window closed · resume (reruns #5 #6)` | since interruption |

**Spinner:** existing 8-frame braille `SPIN`, 80 ms, phase-offset per agent so the board shimmers. **One operation, one line, one clock:** clocks derive from the operation start, not `updated_ms` (fixes today's "time since last update"). Fixed 5 columns (`4.1s`, `12.3s`, `1m05s`).

**Attention rule.**
1. **Counts:** header, rail right slot, window header: `N needs you` amber. "Working" never appears without it.
2. **Row:** `◐` amber with the exact question.
3. **Toast:** at most 2, newest replaces oldest, hover pauses the timer, click jumps to the agent/card, `×` closes. Needs-you toasts collapse into the rail badge after 20 s; done 6 s; failed until opened. Roll up by task: 8 agents never make 8 toasts (`api-refactor · 2 need you`).
4. Terminal title: `(1) marlowe · 3 working`. `g` / alt-g jumps to the oldest waiter from anywhere. Needs-you never decays silently; its age keeps counting.

**Failure and recovery.**
| Case | Looks like | Action |
|---|---|---|
| agent error | `✗`, error line, `retry 1 of 2` | `r` retry that call, with a "will rerun" preview |
| engine reload | rows `◌ loading model…`, one load clock | none |
| disconnect | header `offline · samples stale`, `~`, rows dim | auto reconnect; replay gap reloads snapshot, keeps `CrewNav` |
| window closed mid-run | `↺ interrupted` | resume lists exactly which agents rerun |
| budget overrun | `◐` card with bar | approve extension or stop |
| merge conflict | agent `✗ merge conflict in 2 files` | open diff or steer to resolve |
| Ollama fallback | slot cells `?`, sample fields blank | label `slots n/a (Ollama)` |
| script bug | task `✗`, script error and line in header | `e` shows log |

---

## 7. Interaction map

One verb per key at every level (fixes `x`/`s`/`t` collisions). One code path serves key and click, like the planner.

| Key | Meaning |
|---|---|
| `↵` / click | open (drill down) / expand card |
| `esc` | up one level (then close) |
| `space` | peek |
| `s` | steer selected row |
| `p` | pause / resume agent or task |
| `x` | stop selected agent or task by focus (y/n) |
| `r` | retry task from journal (after a rerun preview) |
| `k` | skip agent (script gets null) |
| `m` / `D` | merge / discard (with confirm). `c` continue (follow-up). `M` model ◂▸ |
| `y` / `n` / `v` | approve / deny / view on an ask (card shows the same letters) |
| `[` `]`, `1`-`9` | flip sibling agent, jump to agent n |
| `f` or `/` | filter by state, phase, text |
| `g`/alt-g | next needing me |
| `e` | events pane. `t` tool detail (not messages; messages become History) |
| `tab` | cycle steer scope / pane |
| `alt-r` | open/toggle Crew. `^d` new task |

**Mouse.** New targets: `Task`, `Agent`, `Phase`, `Step`, `Slot`, `Approve`, `Deny`, `Steer`, `Pause`, `Stop`, `Merge`, `Toast`, `Hint(chord)`, `ScopeChip`, `ResultCard`. Click selects, second click opens, right-click opens the row's action menu (pause, stop, steer, retry, skip, copy id). Wheel scrolls every list and stream. Hover is `lit()` only. Every footer hint is a real button (the app's own "every chord is also a button" rule).

---

## 8. Live and fast

**Cadence.** `animating()` becomes true whenever any agent is in a live state (prefill, thinking, tool, writing, loading). Crew regions then redraw at the 80 ms spinner frame (12.5 Hz); the focused stream redraws at refresh rate while it streams. Nothing live: back to the 1 s idle. A needs-you row alone holds amber with a 1 Hz clock.

**Animates:** spinner, slot cells, clocks, leading `╺`, tok/s sparkline. **Never:** colour fades, sliding, flashing. A changed row gets one 600 ms one-weight lift (a step in weight). Samples older than 3 s show `~`; older than 10 s dim.

**Frame budget targets, unmeasured (at 144 Hz = 6.9 ms):** quiet rail < 0.5 ms; 20-row table < 1.5 ms; agent stream < 2.5 ms with wrap cache.

**Nothing shown twice.** Each fact once per zoom. Slot strip in rail and window header but never both on screen. `events` is the only log; stream the only output text. Tokens once per level. Duplicate `TaskUpdated` removed.

**Event plan.**
| Stream | Rate | Size | Replay ring |
|---|---|---|---|
| `AgentSample` (id, slot, state, tok/s, ctx fill, prefill done/total/cached, tool, out tokens, cache %, wait reason + since) | 4 Hz per running agent, on change | ~150 B | no (ephemeral, seq 0) |
| `SlotMap` (6 slots: owner, busy, cached tokens, queue) | 1-2 Hz on change | ~300 B | no |
| `AgentLifecycle` (queued, started, tool chip, step done, steer sent/read, approval, skip, finished) | 0.2-2/s | 100-300 B | yes, ordered, with its own allowance |
| `AgentText` via `WatchAgent`, coalesced ~30 ms | only the opened agent | as today | no |
| `TaskTree` once, then `TaskDelta` | structural changes | small | snapshot on subscribe / replay gap |

Server: buffered transcript writer (flush per step or 250 ms) or drop it; stop resending goal/result on every `RunUpdated`; drop the duplicate `TaskUpdated`; lifecycle gets a ring allowance so ~120 deltas/s can never evict an approval. Client: slab keyed by `AgentId` with dirty flags, virtualised rows, wrap cache keyed `(agent, width, len)` extended incrementally, same-ink cells merged into spans (the meter trick), tok/s smoothed by EMA, only the opened agent streams.

---

## 9. Data and events to add

| # | Piece | Status today | Work |
|---|---|---|---|
| 1 | Stable `AgentId` on events/approvals/steers; join `AgentCall.id`+`StepSpec.key` to transcript | missing (key never leaves crew) | add to wire |
| 2 | Task hierarchy (phases, parallel groups from `AgentCall.id`, role/model/effort/branch) | exists, not sent | send once, then deltas |
| 3 | Slot index, owner, queue wait reason + holders | exists in engine (`Lease.slot`), not sent | extend `EngineObserver`, publish `SlotMap` |
| 4 | Prefill progress, cached vs new, read time, `Usage`, `CallStarted`, `FirstToken`, `ReasoningDelta` | measured, dropped in `Hooks::event` for crew | forward into per-agent state |
| 5 | tok/s | missing | derive from decode_ms and out tokens, live in decode |
| 6 | Context fill per agent | conductor only | compute from assembly, add to sample |
| 7 | Per-agent steer inbox, ack states, pause, cancel | per-run inbox only; skip exists; single-agent pause/cancel/retry inside a live script has no host support (v1: whole-task resume from journal) | per-agent inbox, `Steer{target}`, `SteerAck` |
| 8 | Structured tool events (name, short args, status, duration, +/−) | clipped text line | new event, results on request |
| 9 | Live sandbox diff (count, +/−, touched files) | `diff_summary()` at finish only | poll ~3 s |
| 10 | One budget unit | workflow pool = output tokens, run = in+out | tokens in+out everywhere (D3) |
| 11 | Foreground helper visibility + kept transcript | invisible, not kept | after `7c30d58` merge, treat as pool agents |
| 12 | `ApprovalRequested.agent`, taint on card | run id only; taint unshown | add field, render |
| 13 | Persist tree, model, slot, durations, cache ratios, tool chips | script journal lacks them | for replay and morning brief |
| 14 | Diff hunks for Result tab | file names and counts at finish only | fetch hunks on request |
| 15 | Slimming (dup `TaskUpdated`, 30 KB snapshots, per-delta file open) | wasteful | see section 8 |

Already exists and just needs wiring in the UI: `workflow` on `CrewRunInfo` (never read), `tainted`, `spent`/`budget` (no bars today), `Spinup` stages, `SkipWorkflowAgent`, `ResumeInterrupted`, `Pause/Resume/Cancel/Merge/Discard`.

---

## 10. Build plan

Each phase ships alone and leaves the conversation usable. All snapshots pass `assert_palette_only`. Cargo through `scripts/cargo-idle.ps1`, one build at a time, when the machine is free.

**P0. One store, one vocabulary (client only, no wire change; smallest valuable).** `TaskStore` from the three feeds; rename to Crew/tasks; unified rail row with phase, agent counts, worst-case glyph, urgency sort; `r.workflow` links the old Run view to its agents; strip stops listing crew; header counts crew-only.
- Accept: a team task appears once across rail, strip and list; snapshots `crew_rail_collapsed/expanded.txt`; test `rail_drops_bar_first_and_never_the_needs_you_count`; palette test.

**P1. Alive and clickable (client only).** `animating()` includes live crew work; spinners and true operation clocks; footer hints, toasts, phase chips, merge/pause become targets; toast click lands on the agent; wheel in lists; 2-toast limit with hover pause.
- Accept: with only crew running, cadence is Live; idle with nothing live is 1 s; every footer hint has a hit rect and sends the same action as its key; hover never fills.

**P2. Task list and task view from existing data.** Urgency grouping, phase strip with filter, agent table (state, label, tokens, elapsed), inline approval card (agent from run for now), fixed key map, persistent conductor line, drill stack with `CrewNav`, `g` next needing me.
- Accept: snapshots at 120 and 160; rail -> any agent in 2 clicks; esc/alt-r stack test; `g` jumps in under one frame; draft survives esc.

**P3. Wire identity and samples.** `AgentId`, tree, `AgentSample`, `AgentLifecycle`, `Hooks::event` forwards, structured tool events, per-agent steer inbox and ladder, `ApprovalRequested.agent`, ring allowance, buffered transcript.
- Accept: reconnect mid-run rebuilds identical rows; 150 deltas/s do not evict an approval; a steer to #4 is read by #4 only; task-wide shows `read by 3/4`; sample rate < 5 KB/s with 6 agents.

**P4. Agent view and slot popup.** Fact grid, steps strip, output stream via `WatchAgent`, History, inspector at 160, `SlotMap`, real slot strip motion, Slots & crew popup, read-ahead line.
- Accept: streaming 10k tokens never re-wraps old lines; stand-in engine: cell fill matches prefill fraction, a queued agent names its holder, Ollama shows `slots n/a`; synthetic 20-agent frame budget; snapshots `crew_agent_160.txt`, `crew_slots_popup.txt`.

**P5. Results, recovery, persistence.** Result tab with hunks, clickable merge/discard, relay card,  `[continue]` follow-up, retry preview, interrupted-resume, foreground helpers, persisted tree for overnight replay, large-task banner.
- Accept: kill-and-restart shows `↺` and resume lists exactly the agents that rerun; finished overnight task opens with agents, durations, tool chips; 20-agent scale test.

**Later:** compare, `@` agent references, OS notification, split view at 180+, voice (one `Attention{kind, who, line}` struct feeds toast, title and speech).

---

## 11. Decisions for Matthew

**Real choices (6). Reply with letters, e.g. "1a 2a 3a 4a 5a 6a" to accept all.**

1. **The noun.** (a) "task" with `team`/`solo`/`helpers` tags, window still named Crew; (b) "team" for everything. **Recommend (a)**: a dispatch is a one-specialist task, not a team.
2. **Crew approvals.** (a) inline cards for all, including merge/push; (b) card for most, modal for merge/push. **Recommend (a)**: a modal blocks "keep talking"; same permission path either way.
3. **Budget unit.** (a) tokens in+out everywhere, USD first only on cloud; (b) keep two units. **Recommend (a)**; the team pool must move off output-only.
4. **Pause releases the slot?** (a) yes, resume may be cold (shown honestly); (b) hold it, starves others. **Recommend (a)** with a "hold slot" toggle.
5. **Green on `✓` in the Crew window** (`RUN_WINDOW` omits green). (a) green; (b) dim. **Recommend (a)**; (b) only if Crew becomes a pop-out window.
6. **Retry.** (a) v1 whole-task resume from journal with preview, per-agent retry later; (b) block on per-agent retry in the script host. **Recommend (a)**: (b) is a host rewrite.

**Defaults I took (veto any):**
- Helpers are one synthetic `helpers` task; alt-r opens the one task needing you, else the list; persistent one-line conductor composer in every Crew view.
- Done rows stay 30 s in the rail and for the session in the list; failed and overnight results stay until acknowledged.
- Crew regions animate at 12.5 Hz, the opened stream at refresh rate; numbers are body ink, amber is state only.
- Task-wide steer reaches every agent including not-yet-started (as a prefix); reasoning shows as collapsed `think` lines.
- Research stage strip and build DAG fold into the one phase strip; `space` peek may answer questions (real approvals need the card).
- OS notification opt-in, needs-you and failed only. Split view at 180+ columns later, opt-in.

---

## Remaining concerns

- Per-agent pause/cancel/retry inside a running script is not supported by the host today; v1 offers skip, steer, and whole-task pause/stop/resume.
- Slot index, queue holders and per-agent cache/tok/s exist only inside the engine and provider; P3/P4 depend on extending `EngineObserver`. Until then the table shows `--`, never invented values.
- Frame budgets, event rates and the 5 KB/s total are estimates from the data map, not measurements.
- Ollama fallback has no slots (`slots n/a`); pause there cannot release a slot.

## Provenance

Built on the glance-first design (one row at four zooms, slot strip as the context meter), plus workflow-first drill/state rules and alive-first slot motion. Rejected: speech ink for agent prose, per-cell motion before the engine observer exists, split panes, LLM-written summaries, rerun-everything resume.
