# Marlowe Addendum B: The Terminal

**Companion to:** Marlowe — Requirements for a Memory-First Agent Harness (v1.0)
**Status:** Requirements specification. **Version 2 — supersedes v1 entirely.**
**Reference implementation:** `docs/design/marlowe-tui-mockup.html` — a clickable mockup of this
specification. Where the prose and the mockup disagree, the prose wins; where the prose is silent,
the mockup is the intent.

---

## B0. Position, and what changed from v1

Marlowe is terminal-native. The CLI is not a debug surface for a product that really lives in a
web app; it is the product. Voice, messaging, and mobile are projections of the same session and
the same commands.

**v1 of this document was wrong about one thing and it was the central thing.** It argued that
Marlowe's differentiation is *subtraction* — one header line, the conversation, one input line,
no boxes, ≤2 lines of chrome. That produced a clean chat client and a bad operating surface. A
harness that runs deep research, holds a schedule, brokers connections, tracks commitments and
earns autonomy has state the user needs to *see*, and hiding it behind slash commands they must
already know is not restraint. It is a discoverability failure wearing restraint's clothes.

**The revised position: density with discipline.** Every region of the screen is bordered,
labelled, and reachable by a named key. Nothing is hidden behind knowledge the user does not have.
The discipline is that a border must *earn* itself — see §B2 — which is a stricter rule than "no
borders," because it forbids decorative ones specifically.

**The design target is unchanged:** an engineer opens it and thinks *someone who uses terminals
every day built this*. What changed is the answer to how you get there.

**What survives from v1, unchanged and still binding:** memory is not a UI element (§B1); tool
calls render as one line (§B6); the craft targets (§B12); nothing decorative.

---

## B1. Memory Is Not a UI Element

**Binding. This survived the rewrite because it was right.**

Memory is the most important system in the harness (v1.0 §5). It gets **no special treatment in
the interface.** The user experiences memory the way they experience it in a good colleague: they
just know things.

Correct:

> This looks like the client version drift you hit in March — the pinned version got dropped when
> the Dockerfile was rebuilt.

Wrong, in every variant:

- A pane, tab or region showing retrieved memories and confidence scores
- A field for injection count, precision, or recall
- Inline citations on recalled facts
- Any default-visible indication that retrieval occurred

An agent that narrates its own recall is not demonstrating good memory. It is demonstrating that
it does not trust the user to notice. **The response is the interface.**

**A retrieval tool call is not a memory UI.** `⋯ recall  open commitments → 3 due` is a tool line
like any other and is permitted. What is forbidden is a region whose subject is the memory system.

**Corrections stay conversational.** "No, I moved off Postgres in April" is how a user fixes a
belief. It must work as plain speech and take effect immediately (v1.0 §5.3). Slash commands —
`/mem search`, `/mem forget`, `/mem pin` — exist as a power-user affordance, are reachable through
the palette, and **do not get a region of their own.**

### The one carve-out: `--dev`

Under `marlowe --dev` (or `/dev` in-session), a seventh inspector tab appears: what was injected,
what was rejected, scores, provenance, and `/why` for retroactive inspection of any turn.

**Off by default. Never shown to a user who did not ask. Not part of the product surface.**

---

## B2. The Region Contract

**This is the rule the whole design rests on. Read it before anything else.**

> **A border delineates an interactive region. Every bordered region carries a label on its top
> border and a hotkey on its bottom border. A region with no hotkey has no border.**

That is stricter than v1's "no boxes," because it forbids the failure mode v1 was actually
worried about — borders used as decoration or grouping — while permitting the thing v1 got wrong,
which is borders used as *affordance*.

The border says: this is a thing. The label says: this is what it holds. The hotkey says: this is
how you reach it. Three facts, no memorization, no discovery cost.

### Focus

**Focus is signalled by border colour and label colour. Never by a background fill.**

Three reasons, and the third is the one that matters:

1. A fill collides with the user's terminal theme.
2. A fill costs a full-cell repaint on every focus change, which fights §B12's flicker target.
3. Border-and-title is how terminal applications have signalled focus for forty years. It reads
   as native; a fill reads as a web page rendered in a terminal.

Focused region: accent border, brightened label, accent hotkey, brightened value. Unfocused:
default border, accent label, dim hotkey. Inactive or irrelevant: dim border, dim label.

**Dimming is load-bearing, not cosmetic.** A calendar event needing nothing from the user is
dimmed to near-invisible so the eye goes to the two that do. Dimming is how a dense screen tells
you where to look.

### Colour

One accent plus three state colours plus three foreground weights. **State colours encode state
only, never category.**

| role | meaning |
|---|---|
| **accent** — `#9B7EDE`, matte violet | focus, labels, hotkeys, structure |
| **amber** | needs attention, approaching a limit, degraded |
| **red** | conflict, failure, irreversible |
| **green** | healthy, live, running normally |
| bright / normal / dim foreground | emphasis, body, inactive |

`MARLOWE_ACCENT` overrides the accent. Fallbacks: ANSI 141 at 256 colours, magenta at 16.
**Never hardcode a background fill.** The terminal's own background is the background.

---

## B3. The Screen

Six regions, top to bottom. All are always present.

```
  * marlowe — thursday                              2 runs · 3 due today

  ┌ Model ──┐┌ Profile ┐┌ Session ──┐┌ Workspace ────────┐┌ Autonomy ┐
  │ opus-5 ▾││ work   ▾││ thursday ▾││ ~/projects/ingest ▾││ draft   ▾│
  └─────(m)─┘└─────(p)─┘└───────(s)─┘└──────────────(w)──┘└──────(a)─┘

  ┌ Status ───────────────────────────────────────────────────────────┐
  │  ◉  listening                              740 ms voice-to-voice  │
  │     voice · barge-in on                    opus-5 · work          │
  └───────────────────────────────────────────────────────────────(v)─┘

  ┌ Conversation ──────────────────┐  1 Runs  2 Schedule  3 Sessions
  │                                │  4 Skills  5 Trust  6 Status
  │  ...transcript, scrolls...     │
  │                                │  ┌ 11:00 · vendor call — acme ──┐
  │  turn 12 · 47 compacted        │  │ you owe them the pricing …   │
  └────────────────────────────(c)─┘  └──────────────────────────(↵)─┘

  ┌ Message ──────────────────────────────────────────────────────────┐
  │ ›                                        12%  $0.18  22m          │
  └───────────────────────────────────────────────────────────────(i)─┘

  ^v Voice  ^n New  ^r Runs  ^l Lineage  ^t Trust  ^u Undo  esc Interrupt  ^k palette
```

**Layout ratios:** the conversation holds no less than 55% of the horizontal split. The inspector
is the aside, never the peer.

**Chrome that scrolls is a bug.** Every region's label, hotkey and footer are pinned outside its
scroll area. Only content moves.

---

## B4. The Control Strip

Five regions holding the values the user changes most. Each opens a selection list in place —
a dropdown drawn by Marlowe, never an OS widget.

| region | key | holds |
|---|---|---|
| Model | `m` | the routed model; switching is one keystroke and no restart |
| Profile | `p` | work / personal — isolated agent roots per v1.0 §12 |
| Session | `s` | current session; the list is searchable by content, not only title |
| Workspace | `w` | the agent's working directory |
| Autonomy | `a` | observe / suggest / draft / confirm / act — the trust tier for this context |

**Autonomy is the one that matters and it carries state colour.** It is the single control that
changes what Marlowe is permitted to do without asking. It is amber at `confirm` and above. It is
never a value the agent can change (Addendum A §A8: self-granted promotion is structurally
impossible), and the region is read-write for the user only.

---

## B5. The Status Band

**One region, always visible, outside every scroll area, reporting exactly what Marlowe is doing
right now.** This is the single most-read part of the screen and it replaces v1's three dim
numbers on the input line.

Each state carries: an animated indicator, the state name in its colour, what it is doing in
specifics, and the numbers that matter *for that state*.

| state | colour | detail it carries |
|---|---|---|
| **listening** | green | voice-to-voice latency, barge-in armed, wake phrase |
| **thinking** | accent | what it is reading, how much is pending, spend against ceiling |
| **speaking** | green | interruptible, latency |
| **writing** | accent | target artefact, words so far, cost this turn |
| **running** | amber | which command, elapsed, results so far |
| **waiting** | amber | **indicator stops moving** — what needs approval, and the keys |
| **idle** | dim | daemon uptime, spend today, background runs elsewhere |

**Motion means Marlowe is working. Stillness means the ball is in the user's court.** That is why
`waiting` freezes the indicator rather than changing only its colour — it is the one state where
nothing will happen until the user acts, and it must be legible from across a room.

**The indicator must report real amplitude or real progress**, or it is decoration and violates
§B12. In voice states it tracks the microphone level. In `running` it tracks elapsed against the
expected duration. **The terminal-native form is a braille or block-character amplitude meter, not
a circle** — a circle is not drawable in a character grid and the mockup's rings are indicative,
not literal. Decide the glyph form before implementation and record it.

**Degradation lives here**, not in the corner of the input line. `dense retrieval offline ·
lexical only` in amber, with the reason in the Status tab.

---

## B6. The Conversation

The hero region. Scrolls; label, pager and hotkey pinned.

**Tool calls render as one line** — this survived v1 unchanged and is still the single largest
difference between a clean agent CLI and a noisy one:

```
    ⋯ read      Dockerfile                              48 lines
    ⋯ bash      git log --oneline -- Dockerfile         6 commits
    ⋯ recall    open commitments                        3 due
    ⋯ edit      drafts/acme-pricing.md                  +3 −0
    ⋯ bash      kubectl diff -n staging                 exit 1
```

- **Expand on demand** — cursor to a line, `Enter` or `Tab` for full output in place.
- **Failures auto-expand.** The one case where the user always wants detail.
- **Live lines animate in place** with elapsed time. Never scroll output in and then clear it.
- **Consecutive same-verb calls collapse** — six reads become `⋯ read  6 files`.
- **Summaries are typed, never generic.** Each tool declares its own summary shape. `done` is not
  acceptable.

**Compaction announces itself inline and does not interrupt:**
`─ compacted · 47 turns → summary ─`

**The pager is pinned at the bottom** and carries what no other region has:
`turn 12 · 47 compacted · lineage 3 deep`. The lineage depth is how many compaction generations
this session goes back, and it is what `^l` walks.

**A visible scroll position is required.** A thin scrollbar on the right edge of the transcript,
not an overlay hint.

---

## B7. The Inspector

Six tabs, one visible at a time, each reachable by its number. Tabs are pinned; content scrolls.
Each item inside is itself a bordered region with a label and a key.

| tab | key | holds |
|---|---|---|
| **Runs** | `1` | active background work — status, elapsed, spend against ceiling, subagent depth, and **a Steer field that injects guidance into a running child without restarting it** (v1.0 §10.1) |
| **Schedule** | `2` | today's events with what Marlowe noticed about each; commitments due; conflicts with one-key resolutions |
| **Sessions** | `3` | history, searchable by content; turns, artefacts produced, compaction depth |
| **Skills** | `4` | grouped by domain, not a flat list; installed count and currently-exposed tool count against the ≤12 budget |
| **Trust** | `5` | the trust ledger (Addendum A §A8) — action classes, tier, agreement rate, trend, pending promotion proposals with their evidence, and ceilings that no evidence lifts |
| **Status** | `6` | model, provider, context, spend, connection health, degradation reasons, memory size, daemon uptime |

### The rule that makes the inspector worth having

> **When the user asks for something the inspector can render, the inspector renders it and the
> conversation says only what a colleague would say out loud.**

Ask *"what does my day look like"* and the Schedule tab activates while the transcript says
*"Three things need you — the vendor call at eleven is the one to look at."* The transcript
carries judgment; the region carries data. **This is the whole argument for a TUI over a chat
log**, and without it the tabs are just a menu.

### Schedule is where the secretary layer becomes visible

Not a calendar dump. Each event is a region carrying what Marlowe noticed — an unfulfilled
commitment to that person, contact-cadence drift, a conflict with a flight arrival, and one-key
resolutions inline (`▸ move to 16:00 · ▸ send regrets`). **Events needing nothing are dimmed to
near-invisible.** Commitments due are a separate region from events, because a commitment has a
deadline and no time slot.

---

## B8. Message Field and Footer

**The message field is a region** with the ambient numbers on its right: context percentage,
session spend, elapsed. Context pressure is a colour, not a bar — dim until 60%, amber at 60%,
accent at the 70% compaction trigger (v1.0 §6).

**The field's placeholder tracks the status band**: `listening — type to take over`,
`thinking — esc to interrupt`, `waiting on you`, `ask me something`. That is how barge-in is made
visible without a second indicator.

**The footer is one line of global keys**, always the same, never context-dependent. Region
hotkeys live on their own borders; the footer is for actions.

---

## B9. Approvals

Where the security model meets the user (v1.0 §8), and where most agent terminals degrade into
reflexive clicking.

**In a design where every region is bordered, a border no longer signals modality.** The approval
overlay must therefore: dim the entire frame behind it, use a doubled border in the state colour
of its risk tier, and centre itself. It is the only element permitted to dim the rest of the
screen.

- **States blast radius, not the command.** Not `Run: rm -rf ./build?` but
  `Delete 1,204 files in ./build · not recoverable`.
- **Risk-tiered** per v1.0 §8.2 — silent for reversible reads, batched for routine writes,
  blocking for irreversible ones.
- **Novelty gating is explained in one line**: `unusual · first send to this recipient`.
- **Ceilings are stated**: `this class sits at its ceiling and cannot be promoted`.
- **Offers the delegation escape hatch**: `↵ send · e edit first · s send as marlowe · esc deny`.
  Sending *as Marlowe* is the path that avoids impersonation entirely (Addendum A §A3).
- **Rubber-stamping is measured** — approval latency and approve-without-expand rate per class.
  When the user is clicking through, say so and propose promoting or tightening the class
  (v1.0 §14.8).

---

## B10. Interaction

**Keyboard first. The mockup is mouse-driven; the implementation must not be.** Build every path
by key and add mouse as a bonus. Mouse-first retrofitted with keys produces a bad TUI.

**Amended 2026-08-08 — the mouse is captured.** An earlier version of this section left mouse
reporting off and recorded that as a deliberate choice, so that the terminal's own selection and
copy kept working. That reasoning was sound in isolation and wrong in aggregate: **being able to
drag-select the frame is the single thing that makes a running application read as a printout**,
and that product judgment outranks the earlier line.

So: `EnableMouseCapture` on start, `DisableMouseCapture` in the same teardown as raw mode and the
alternate screen — **including the panic hook**, because a panic that leaves capture on hands the
user a terminal whose mouse has stopped working with no application left to explain why.

**This does not weaken keyboard-first, and the two were never in tension.** Every action remains
reachable by key; the mouse adds no capability the keyboard lacks and no second state path. Click
focuses a region and the wheel scrolls one — both by routing into the same dispatch the arrow keys
use. Hit-testing deliberately stops at the region: individual inspector items are chosen by key,
because item-level hit-testing would be the first place the mouse grew a path of its own. The cost
is named rather than absorbed: terminal-native selection is gone, and copying a line now needs the
terminal's own override (`Shift`-drag in most emulators).

- **Region hotkeys jump focus directly.** `m` to Model, `2` to Schedule, `c` to the conversation.
- **The documented navigation must work on the first keystroke of a fresh session.** The default
  focus is a region where letters are hotkeys — **never a text input**. This is a rule and not a
  detail: an implementation that starts focused in the message field makes every hotkey printed on
  every border inert on arrival, and the borders are then lying. It is stated here because it was
  got wrong once and the failure is invisible to any test that presses `Esc` first — *"reachable
  after one extra key that no border mentions"* is still reachable, so the test passes while the
  design fails. **A design whose documented navigation fails on the first keystroke will be judged
  in the first five seconds**, and no amount of correctness behind that first impression recovers
  it. The acceptance suite asserts reachability from the default focus, pressing the advertised
  hotkey and nothing else.
- **`Tab` / `Shift-Tab` cycle regions** in reading order.
- **Arrows move within a focused region**; `Enter` acts; `Esc` backs out one level.
- **Copy is a first-class interaction, not an afterthought.** *A conversation the user cannot get
  out of the application is a conversation trapped in it.* Mouse capture took the terminal's own
  selection away, so the replacement has to be real rather than "use the keyboard":
  - **`Shift`-drag falls through to the terminal's native selection.** Windows Terminal, iTerm2 and
    GNOME Terminal all bypass mouse reporting while `Shift` is held — the escape hatch `lazygit` and
    `htop` rely on. **Verified working under capture** against Windows Terminal 1.24 (2026-08-08):
    121 characters copied out of a live session. It is documented in `/help`, because an escape
    hatch nobody knows about is not an escape hatch.
  - **`y` copies the focused turn or tool result**; **`Y` copies the whole transcript as markdown.**
    Both are reachable from the palette.
  - **Copied text is built from the model, never from the screen** — no borders, no scrollbar
    column, no inspector content that happened to share a row. This is not a refinement of native
    selection but a different thing: cell-rectangle selection *cannot* know that column 62 belongs
    to another region, which is exactly what the measured `Shift`-drag output shows.
  - **A tool line copies its full expanded output, not its one-line summary.** §B6 collapses six
    reads into `⋯ read 6 files` and hides a failure's detail; that is right on screen and wrong on
    a clipboard, where the user copying a failure wants the error.
- **`Ctrl-K` opens one fuzzy palette** over everything — sessions, models, skills, commands,
  recent files, memory search. Editor-grade type-ahead. This is where anything without a region
  lives.
- **Slash commands autocomplete inline** with descriptions.
- **Multiline by default.** Shift-Enter for newline, Enter to send. Pasting multiple lines never
  fires a premature send.
- **`Esc` interrupts.** Partial output is kept. Mid-tool-call interrupt follows v1.0 §9 —
  idempotent reads complete, mutations cancel.
- **Type while it thinks.** Input is never blocked; queued messages send when the turn completes.
- **`!cmd`** runs a shell command in the agent's working directory, through the same approval path.
- **`/undo N`** soft-deletes the last N turns, identically across TUI, CLI and messaging.
- **Installed skills become slash commands automatically.**
- **User-defined commands that skip the model entirely** — the cheapest latency win available.

---

## B11. Width, and the Classic CLI

**v1 required the TUI to work at 80×24. That requirement is withdrawn.**

This layout needs approximately 120 columns. A narrow variant was designed and rejected: it cost
the borders, which cost the region contract, which is the entire design. Half a good interface is
worse than one honest fallback.

**The TUI requires ≥120 columns and ≥30 rows.** On start it requests a resize
(`CSI 8 ; rows ; cols t`); terminals that honour it comply, and those that do not are handled by
the rule below.

**Below the minimum, the TUI does not render a degraded layout.** It prints one line naming the
current and required size, and offers the classic CLI. **A broken grid is worse than an honest
refusal.**

**The classic CLI is the narrow, SSH, piped-stdin and no-TTY path.** It is a readline REPL with
**command parity, not layout parity** — every slash command, every session, every piece of data
is reachable, rendered linearly. `/runs`, `/schedule`, `/trust`, `/status` print what the
corresponding tab shows. It is not a lesser product; it is the same product without a grid.

*(v1 said "no TUI-only features." That is amended: no TUI-only **capabilities**. Layout is
allowed to differ, because layout is what a grid buys.)*

---

## B12. Craft

Not from features. From four things users feel and cannot name:

1. **Nothing ever makes them wait.** First frame under 150 ms. Input accepted under 300 ms.
   Keystrokes never dropped, including mid-stream. The frame paints before initialization
   finishes.
2. **Nothing ever flickers.** Correct differential rendering. Resize reflows cleanly. Streaming
   does not repaint the screen. **This is why focus is a border and not a fill.**
3. **Nothing is ever ambiguous.** Every state legible at a glance. A user never wonders whether it
   is working, waiting, or stuck — the status band answers that without being read closely.
4. **Nothing is decorative.** Every moving element reports real state. The status indicator tracks
   real amplitude or real progress. No spinners where content could be. No mascots.

Get these four right and the interface will be described as beautiful by people who cannot point
to a single beautiful thing in it.

---

## B13. Acceptance Criteria

| Metric | Target |
|---|---|
| Time to first frame | < 150 ms |
| Time to interactive | < 300 ms |
| Dropped keystrokes during streaming | Zero |
| Repaint flicker during stream or resize | Zero, verified 120×30 through 240×60 |
| Tool call default footprint | 1 line |
| **Every bordered region has a label and a hotkey** | 100%, asserted by test |
| **Regions reachable by keyboard alone** | 100% |
| Background fills used to signal focus | Zero |
| Chrome inside a scroll area | Zero |
| Distinct colours | ≤ 1 accent + 3 state + 3 foreground weights |
| Memory-related regions in the default surface | Zero |
| **Accent legible on both dark and light terminal backgrounds** | Verified by eye on each |
| Below-minimum width behaviour | Honest refusal, never a degraded grid |
| Classic CLI command parity | 100% of commands, sessions, data |
| **§B13 suite run on native Windows Terminal AND a Linux emulator** | Pass on both |

The last row is the one most likely to be skipped. Development is on Windows (ADR-002 revised), so
**Linux is the surface at risk of CI-only verification.** A TUI verified on one platform is not
verified.

---

## B14. Anti-Requirements

- **No web UI in v1.** Terminal and messaging gateway are the surfaces.
- **No memory region outside `--dev`.** §B1.
- **No border without a hotkey.** §B2 — this replaces v1's "no boxes."
- **No background fill to signal focus or selection.** §B2.
- **No startup inventory.** Tools and skills live in the Skills tab.
- **No decorative motion.** Every animated element reports real state.
- **No degraded narrow layout.** §B11.
- **No mouse-only paths.** §B10.
- **No TUI-only capabilities.** Layout may differ; reachability may not.

---

## B15. Implementation

**Rust, `ratatui` + `crossterm`** (ADR-001). The mapping is close to one-to-one, which is why
this design is buildable rather than aspirational:

| specification | ratatui |
|---|---|
| bordered region, label on top border | `Block::bordered().title(…)` — native |
| hotkey on bottom border | `.title_bottom(Line::from("(c)").right_aligned())` |
| focus by border colour | `.border_style(…)` — one style swap, no cell repaint |
| control strip / main split | `Layout` with `Constraint` ratios |
| inspector tabs | `Tabs` |
| scrolling transcript + pinned chrome | outer `Block`, inner `Paragraph::scroll()` or stateful list |
| scrollbar | `Scrollbar` |
| dropdown, approval overlay | `Clear` + floating widget |
| status indicator | custom widget over braille or block glyphs |

**`crossterm`, not `termion`** — it is the backend that works natively on Windows Terminal, which
matters given ADR-002 puts development there.

---

## B16. Sources and Provenance

Hermes Agent's TUI (Nous Research) for the capability surface — classic REPL and TUI front-ends,
modal overlays, non-blocking input, shell mode with approval semantics, `/undo`, local session
recap, automatic slash commands from skills, TTY fallback.

The region contract — bordered, labelled, hotkeyed regions with focus signalled by border and
title — is adapted from the Jira TUI, which uses it to make a large control surface navigable
without memorization. Marlowe takes the affordance and rejects the density that comes with a
form-filling application: regions here hold state and actions, not fields of a fixed schema.

---

## B17. The Launcher — Marlowe Owns Its Window

**An application that looks wrong because the user has not configured their terminal is an
application that looks wrong.** Whose fault it is does not enter into it.

Marlowe opens a terminal it controls rather than assuming the current one is suitable. This is not
polish: it removes an entire class of report — wrong font, wrong size, wrong background, braille
rendering as tofu, a focus highlight in a colour nobody chose — by **choosing** those rather than
hoping for them. Every one of those was reported against M1 at least once, and none of them were
bugs in the frame.

**Requirements, on every platform:**

- A dedicated Marlowe profile. **The title bar stays** — an earlier draft of this section asked for
  a borderless window, and Windows Terminal's `--focus` delivers exactly that by removing the title
  bar, which also removes dragging and the close button. *A window the user cannot move or close is
  not a better window.* The title bar earns its row instead: **Marlowe writes it via OSC 0 and keeps
  it current with session state** (`marlowe — thursday · listening · 2 runs`), so the chrome is a
  live readout rather than a static label — the same argument for putting state in a native title
  bar on a platform that has one.
- **The chrome is themed to the frame.** Tab row and title bar take the terminal background, so
  there is no seam and the tab strip stops reading as a lid sitting on the design. The tab strip is
  hidden entirely while a single tab is open, which takes the `+` and `×` with it.
- Initial size **≥ 120×30**, per §B11, and **only ever raised toward it, never lowered** — a user
  who opens their terminal at 200×60 did not ask Marlowe to shrink it.
- A pinned monospace font with **braille coverage U+2800–U+28FF** (ADR-021). Cascadia Code
  qualifies and ships with Windows Terminal, so pinning it asks the user to install nothing.
- A colour scheme whose background matches §B2's ground, so there is **no seam** between the
  scheme and what Marlowe draws.
- **Zero cell padding.** A margin inside a bordered design reads as a rendering fault.

**The profile is written on first run, is idempotent, and adds exactly one entry.** It is keyed by
a fixed GUID so a re-run updates Marlowe's own entry; **no other profile is ever modified**.

**Two refusals, both deliberate:**

1. **A settings file that cannot be parsed is not written.** Windows Terminal's `settings.json` is
   JSONC and users keep comments in it; a naive round-trip would delete them. Parse failure skips
   the profile step, says so, and falls back to a direct launch that needs no profile.
   *Corrupting a terminal configuration to make an application look nicer is not a trade this gets
   to make.* The file is backed up before any write regardless.
2. **If no suitable terminal is found, say so plainly and run in the current one**, with a one-line
   note naming what is degraded. Never silently accept a terminal that cannot render the design —
   that is how "the font is wrong" arrives as a bug against the frame.

**Status.** Windows ships in M1: `wt --focus` gives the borderless window, a named profile pins the
font and scheme, and `--ground` (OSC 10/11) matches the ground. **macOS and Linux are recorded for
M2** — iTerm2 dynamic profiles, GNOME Terminal via dconf, kitty and alacritty config fragments are
each a separate implementation that needs verifying *on* the platform rather than reasoned about
from another one. On those platforms `marlowe --launch` currently reports the degraded path and
runs in the current terminal, which is the honest version of not having built it yet.

---

*Supersedes Addendum B v1 in full. v1's central claim — that differentiation is subtraction —
produced a clean chat client rather than an operating surface, and is withdrawn. v1's memory rule
(§B1), tool-line rule (§B6) and craft targets (§B12) are retained unchanged because they were
right.*