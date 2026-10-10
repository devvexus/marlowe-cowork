# Changelog

All notable changes are recorded here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Context and hardware planner

Matthew's rule: the user sets each model's maximum context up front; agents get that or less, never more; and the cost on this GPU is shown before anything loads.

- **Added.** `marlowe-hw`, pure: a header-only GGUF reader (a 10 GiB blob's header in about 150 ms), the ggml type table, `ArchProfile` and the VRAM budget.
  - `ArchProfile` gives layer roles (recurrent flags, `full_attention_interval`, per-layer `head_count_kv`, state tensors, MTP blocks), KV per token, sliding windows (the GGUF pattern or llama.cpp's per-architecture default), MLA, recurrent state (Mamba-1/2, Gated DeltaNet, RWKV, short conv), and where the weights sit (GPU, host embeddings, a tied copy, vision).
  - The budget is `free + Σ runner dedicated − 1 GiB`. `fixed(c) + n·per_agent(c, kv)` is shared with the client as `PlanModel`, so the popup and the daemon do the same arithmetic. It also gives partial-offload layer counts and the decode slowdown (`W_gpu/BW_gpu + W_cpu/BW_ram`).
  - `log` parses Ollama's `server.log`: the runner's argv, and every buffer per device per context. The planner is calibrated against it.
- **Measured** with dusk-super loaded (32k, q8_0, 1 slot, MTP draft n=3). Each buffer of the runner log matches to 0.00%: main weights 10,019.96/397.85, KV 1,088.00, state 598.50 (×4 copies with the draft), and draft 774.71/128.00 MiB. The uncalibrated whole-runner prediction is 13,529.2 MiB, which is **+0.32%** against PDH's 13,485.7 MiB. Ollama's `/api/ps` says 1,631.2 MiB (**−87.9%**), so nothing reads its sizes. The measured driver overhead is 506.2 MiB. `cargo test -p marlowe --test context_live -- --ignored --nocapture` repeats this on whatever is loaded, and never loads anything.
- **Added.** A context policy per model (`context_policy` and `hw_calibration` in `cowork.db`; removing a model removes its policy) and `Brains::context()`.
  - `allowed_ctx` = `min(requested, max_ctx)`, with a property test that it never exceeds `max_ctx`.
  - `slots` and `lease` provide per-model agent slots, released on drop, for the crew to call.
  - `set_policy` refuses anything above the model's trained context, under 2,048, or with 0 or more than 16 agents.
  - The conductor's window is the policy's. Every Ollama request for a model sends `num_ctx = max_ctx` through the shared `ContextAllocations`, a constant per model so the runner never reloads per agent, and the provider clamps anything above the model's maximum.
  - Nothing is applied silently. Without a saved policy, requests keep the 32k default, and `a` in the popup fills in the automatic choice.
- **Added.** Ollama runtime readers in `marlowe-provider::ollama_runtime`:
  - the main and DRAFT GGUF headers from the store, falling back to `/api/show`;
  - `/api/ps` for the model list and context, never sizes;
  - the runner's `/slots`;
  - the saved user environment, read with `reg query`.
- **Added.** `marlowe-sentinel::vram::SentinelGpu`: NVML totals, per-runner dedicated and shared memory from the `GPU Process Memory` PDH counters (NVML reports N/A under WDDM), peak bandwidth (bus width × max memory clock × 2: 736 GB/s here), and system RAM.
- **Added.** The server change: `OLLAMA_KV_CACHE_TYPE` / `OLLAMA_NUM_PARALLEL` / `OLLAMA_FLASH_ATTENTION` are set with `setx`, then the tray app is quit and relaunched. `ApplyOllamaServer` always raises an approval that offers only Allow and Deny. The approval says that every Ollama model is affected, and how many models and requests the restart stops. A Deny returns the manual steps. Tests use a recording fake.
- **Added.** On the wire:
  - requests `ContextPlan`, `ContextPolicies`, `SetContextPolicy` and `ApplyOllamaServer`;
  - events `ContextPlan`, `ContextPolicies`, `ContextPolicySaved`, `ServerApplied`, and `ContextUsage`: one per model call, prompt plus output, out of the agent's window.
- **Added.** The context meter, on the Model box's second row (the box is 4 rows; the board gives up one): `ctx ━━━━┊──── 12.4k / 32k · kv q8 · 1 slot`.
  - The bar is accent up to the 70% compaction mark `┊`, amber to 90%, then red.
  - It adds amber `57/65 on GPU` and `q4 pending`, and `not loaded` or `cloud`.
  - When narrow it drops the bar, then the slot count, then the KV type.
  - Hovering lights it; clicking opens the planner.
- **Added.** The "Context & hardware" popup, opened from the meter, the model picker (`c`), the Library (`c`, or the new `window` column), and ^k "Context & hardware".
  - It shows the GPU, the fixed load, the architecture with a layer strip (`┃` KV, `·` recurrent), the state per agent, KV per token for each type, and host RAM.
  - A table gives per-agent cost and agents that fit at 8k–256k for f16, q8_0 and q4_0, with partial-offload layers and slowdown, or "does not fit".
  - It shows the largest single agent, with and without the draft.
  - ←/→ change the context, the KV type and the number of agents. A usage bar (`━` fixed, `╍` agents, `─` spare, red over the budget) and a one-line verdict follow every change.
  - ↵ shows exactly what saving changes now and what waits for an Ollama restart. `s` saves; `r` saves and asks to restart.
- **Known.** Still to verify on first use, each from the runner's log or `/slots` and never assumed:
  - Ollama's `-c` value when `np > 1`;
  - whether `prompt_eval_count` counts the cached part of the prompt (the meter uses per-call `input + output`);
  - MLA V-cache and DSA indexer sizes (the MLA fixture is constructed, not log-validated);
  - the vision compute buffer (an estimate until a load is measured).

  The crew does not call `allowed_ctx`/`lease` yet; that wiring belongs to `marlowe-crew`. `--kv-unified` pooling and the local GGUF provider's per-model launch flags are v2.

### TUI polish after first use

- **Fixed.** The Library with one model said nothing about what to do: the cursor now runs down the slots, the models and a new "+ add another model" row; the border hints name what ↵ does on the row (`effort medium → high`, `keep as conductor`, `add another model`); Esc, and Backspace where nothing is typed, go back one level in every view and picker; j/k move in plain lists.
- **Added.** Hover and click everywhere. The renderer records where each target was drawn (`State::hits`); motion sets `State::hover` and the loop redraws only when it changes. A hovered box takes the theme's hover tone (focus keeps the accent); rows, tabs and footer chords brighten their foreground. Clickable: Model/Workspace boxes, board tabs and stacked sections, crew runs (open the Run view), watched runs and notices (open Watch at their root), footer chords, the header summary, Library/Watch/Memory/crew/picker/palette/session rows, approval and import buttons, the Run view's steer line.
- **Added.** The header row: `* marlowe — <session>` and a live summary (`3 runs · 1 needs you · 2 new`). The window title follows the session (OSC 2, saved and restored with XTWINOPS 22/23).
- **Added.** The terminal's default background is set to `#0f0e14` (OSC 11) at start and reset (OSC 111) on every exit: quit, error, panic hook, and console close/Ctrl-Break via `marlowe-os::console::on_interrupt`. Cells still carry no background. `MARLOWE_KEEP_TERMINAL_BG=1` leaves it alone. It is set before the maths image probe, so images blend onto it.
- **Added.** The Run view's research stage strip (Plan → Explore → Compress → Corroborate → Verify → Synthesise) with source, claim, root and citation counts from `ResearchProgress`; runs without progress show the plain transcript.

### Maths for engineers and researchers

- **Changed.** Typst runs in a helper process, `marlowe-typeset.exe` (crate `marlowe-typeset-helper`), built by default and shipped beside `marlowe.exe`. Images are on by default; the `typeset-images` feature is gone.
  - `marlowe.exe` no longer links Typst. The TUI probes for Sixel as before. Where the terminal has it and the helper is present, the helper starts on the first display formula.
  - Each formula is one JSON line each way over stdio. The request carries the Typst markup from the parse, the colour, the background, the cell size and the reserved box; the reply carries one Sixel strip per row, or an error.
  - Every strip is checked before it reaches the terminal: `ESC P … ESC \` with printable ASCII only. The helper's environment is cleared and its stderr discarded.
  - An error reply, a death, a timeout (20 s for the first formula, 10 s after) or a malformed reply leaves that formula on the grid. A dead helper is restarted once; after a second death the session stays on the grid. `MARLOWE_MATH_IMAGES=0` still turns images off.
- **Added.** Matrices and alignments on the grid:
  - Array specs are read fully: `l`/`c`/`r`, `p{…}`, `@{…}`, `*{n}{…}`, and `|` as the augmented matrix's rule. `\hline` between rows is a horizontal rule, and rules run the full height and width of the block. `pmatrix*[r]` aligns. `\substack` under a big operator stacks.
  - A blank row separates rows only where one is three or more rows tall (a fraction). A dropped script (`f_y`) no longer doubles a row's height.
  - `\tag` sets a right-aligned column after the widest row, in `align`, `gather` and `equation`. `eqnarray` columns are set tight. `T^{\mu}{}_{\nu}` and `{T^{ab}}_c` stagger their indices.
  - A matrix wider than the pane is clipped at the pane's edge, with a dim `⋯` on its middle row, instead of being wrapped as `[a, b; c, d]`. The rows reserved while it streams match. Other wide displays still go to one line per row. There is no horizontal pan yet.
- **Added.** On the image path: `cases(delim: none)` for nested `aligned`, `mat(augment: …)` for rules, alignment points for mixed column alignment, a `quad` between `align` column pairs, tags in their own column, and the `Vmatrix` delimiter `‖` (Typst refused `||` before).
- **Added.** `marlowe math [--grid | --images] [--width N]`: 31 hard equations and matrices (`math::GALLERY`), each drawn as the conversation pane draws it, grid and then image. It says why images are off when they are.
- **Added.** `tests/data/maths_engineering.rs`: 214 formulas in 20 categories, written the way engineers and ML researchers write them. Matrices of every bracket, block and augmented matrices, 8×8 to 10×2 numeric matrices, determinants, tagged derivations, `gather`, `cases`/`dcases`/`rcases`, Jacobians and Hessians, `\nabla`, mixed-index tensors, norms, `\mathbb E`/`\Pr`/`\operatorname{Var}`, argmin with `\substack`, multiple and contour integrals, attention/LayerNorm/KL/DPO/InfoNCE, fonts, units, `\left\{ … \right.`, and vector accents. `tests/maths_matrices.rs` adds 14 matrix snapshots.
- **Measured** (x64, release with thin LTO, unless marked debug):
  - `marlowe.exe` was 76.20 MB without images and 110.22 MB with `typeset-images`. It is now 76.49 MB with images on. `marlowe-typeset.exe` is 36.65 MB.
  - Corpus: 289 formulas (75 general + 214 engineering). Every category is 289/289 on the grid (strict parse, nothing raw at 160 and 60 columns) and 289/289 through Typst. All 236 display formulas fit their grid box as images.
  - Grid layout per formula: p50 0.043 ms, p95 0.13 ms, cold 0.1 ms (debug: 0.26 / 0.75 ms).
  - Through the helper process, over all 236 displays: the first formula takes 19–22 ms, including the process start (290 ms when the exe is not in the disk cache). After that, p50 is 0.8 ms, p95 1.8 ms and max 4.4 ms. A debug helper takes 460 ms for the first formula, then p50 5.8 ms.
- **Known.** Sixel on a real Windows Terminal is still not verified, and neither is the helper running beside an installed `marlowe.exe` there. Everything above was measured headless.

### Typeset maths

Matthew's rule: full LaTeX in the terminal, smooth, and never raw.

- **Changed.** `marlowe-surface::latex` is replaced by `marlowe-surface::math`, which never shows source. A span renders only after its closing delimiter arrives. A closed span that does not parse gets one repair pass (stray `$`, an unbalanced brace, `\left` without `\right`, an unclosed environment, a dangling `^`), and anything still broken is paraphrased (`\foo{x}` reads `foo(x)`).
  - Display maths is laid out in 2D on the character grid: stacked fractions with a `⎯` vinculum (not chrome's `─`), big operators with limits, radicals with indices, matrices, `cases`/`aligned`/`align`, brackets that scale, accents, over/underbraces, `\boxed`, `\xrightarrow`.
  - Inline maths stays on one line, using Unicode scripts and precomposed or combining accents.
  - A formula wider than the pane falls back to its one-line form.
- **Changed.** Streaming:
  - An unclosed span shows a dim `…` in the prose colour. It is not `⋯`, which marks tool lines.
  - A display block reserves its rows as it streams, so the rows around it do not move when it lands.
  - The streaming entry renders live (`render_prose_live`); the finished entry re-lays out identically.
- **Changed.** User messages, reasoning, and session and run titles also typeset their maths and leave other markup as typed. `$5 and $10` is still currency.
- **Fixed (review).** Paths that put source on screen, each now a case in `tests/maths_adversarial.rs`:
  - A finished `$` that never closed before a command (`Let $\alpha be…`) is maths up to where prose resumes. Before, it was shown as written.
  - A table too wide to lay out shows its lines with maths typeset, not as code. A `|` inside `$…$` no longer splits a cell, and the row still streaming holds its formula.
  - `\tag{\alpha}`, `$\$5$`, `\begin{pmatrix}…` mid-paragraph, `\begin{array}` on its own line, and bare symbol commands in prose (`the rate \eta`; paths like `C:\theta` are left alone).
  - The user's own backtick code and fences keep their `$` as typed.
  - `$$…` mid-paragraph reserves its rows while it streams. A display block's reservation is counted the way the landed block is drawn.
- **Added.** `marlowe-typeset`, off by default (`--features typeset-images` on `marlowe`). Where DA1 reports Sixel, display formulas are drawn as images:
  - Typst 0.14.2 lays out the same parse the grid uses (`Formula::typst`). Every character with Typst meaning is escaped, and text goes in strings.
  - The image is fitted into the box the grid reserved. It is sent as one transparent-background Sixel strip per cell row (`P2=1`), with edges blended onto the background reported by OSC 11, so there is never a filled box. Glyphs take the terminal's foreground (OSC 10) or the formula's colour.
  - A worker thread renders and encodes once per (markup, colour, box). The pane shows the placeholder until the image is ready, then the strips. Strips are re-sent only when moved or uncovered, never into a cell something is drawn over, and stale ones are erased.
  - `marlowe-os::console::query` reads the terminal's replies with VT input switched on for the exchange. `MARLOWE_MATH_IMAGES=0` turns images off; `=1` skips the DA1 check.
- **Measured.**
  - The corpus is 75 model-written formulas (30 inline, 45 display). The grid parses all 75 strictly (100%). The Typst path converts and typesets all 75 (100%), and all 45 display formulas fit their grid box.
  - 33 malformed sources render to 132 frames with no `\`, `$` or source brace on screen.
  - A token-by-token stream produces 329 frames with 0 raw and 0 settled rows moved. The same holds through the TUI reducer (97 frames).
  - Grid layout p95 is 0.54 ms (debug). Typst typeset plus encode in release: p50 0.30 ms, p95 0.68 ms, first render about 5 ms.
  - Binary size, release: the default build adds only the worker and encoder. `typst` adds 36.5 MB, measured as a size probe going from 0.13 MB to 36.6 MB. Most of that is Typst's library (syntax highlighting, bibliography styles, wasm, ICU data); the two vendored New Computer Modern fonts are 1.7 MB. That cost is why the feature is opt-in.
- **Known.**
  - Sixel behaviour on a real Windows Terminal is untested, on x64 and ARM64. That covers transparency (`P2=1`), whether spaces erase image cells, the `CSI 16 t` cell size, and the conpty path for query replies.
  - Inline maths is never an image.
  - Display formulas inside lists or quotes stay on the grid.
  - Typst 0.15 conflicts with `rust-trafilatura`'s `icu_properties =2.1.2` pin.

### TUI layout: the old harness's boxes

- **Changed.** The conversation screen is drawn in titled, rounded boxes — Conversation `(c)`, Message `(i)`, Model `(m)`, Workspace `(w)` and the Situation Board — with the hotkey on each bottom border and an accent border on the box that has the keys (`Theme::region_styles`, no fills). The board takes 3/10 of the width (38–72 columns). When it has the height it stacks Crew `(1)`, Watch `(2)`, Threads `(3)` and Memory `(4)` in their own boxes. Otherwise it shows one box with a numbered tab strip. Today (date, quiet hours, clock) and the most-used chords share a one-line footer; a chord that does not fit is in the ^k palette. The presence meter sits on the Conversation box's top border, with the turn's elapsed time.
- **Added.** The focus model: Esc (when nothing is running) or Tab hands the keys to the boxes. From there `c i m w 1–4` jump, ↵ acts, and any other letter goes back to the Message box and is typed. Alt-1..4 switches the board section from the Message box, and alt-v toggles the voice. A workspace picker (`w`) offers a typed path, the launch directory, recent sessions' workspaces and watched roots. Choosing a new workspace while a session is open starts a new session there. The Memory section shows the store size and this session's recalls.
- **Changed.** Tab in the Message box moves focus instead of inserting four spaces. Paste still keeps indentation.

### M4.5 Deep Research v2 and CUDA

- **Added.** `marlowe-research`: the keyless channel registry (no credential field; declared rate limits, identity authority and `indexes` per channel; routing by question type), `SourceRef`/`Identity`, the identity-key dedup cascade (an identifier asserted by fetched content stays `Unresolved`), corroboration over independent roots, link tables for navigation by selection, `<home>\cache\research`, and robots.txt/per-host politeness. The channel searches arrive under "Added (channels)" below.
- **Added.** `marlowe-extract::main_content`: the `MainContent` trait, the `Candidate` enum (baseline, rs-trafilatura, rust-trafilatura) and per-page `choose` (the candidates themselves are under *Added (extraction)* below).
- **Added.** Proto: the `ApprovePlan{run, decision}` request; the `PlanProposed{run, plan, quote}` and `ResearchProgress{run, progress}` events; `ResearchPlan`, `CostQuote`, `PlanDecision`, `ResearchStage` and `ChannelId`. These are additive, so `PROTO_VERSION` stays 1. The daemon answers `ApprovePlan` with an error until v2 runs exist.
- **Added.** `marlowe-crew::playbook::research_v2` stage types: quote, plan gate (auto-approve is off by default), explore tree, shared source budget, verification. A run with no verification gets a refusal that names what is missing, not a report.
- **Fixed.** `glob` returned 0 paths for `**/Cargo.toml` because `**` was treated as two one-segment `*`s. `**` now spans directories, and `**/` also matches zero of them. `{a,b}` works too. Patterns match relative to `path`, and the matcher runs as a DP with no backtracking.
- **Fixed.** `marlowe ask` piped to a file or another program no longer prints a thinking redraw per token. It prints a single `▸ thought for N tokens` line.
- **Changed.** Persona v3 `words_to_avoid` now also lists "to be transparent" and "just to be transparent".
- **Added (channels).** Every keyless channel searches for real: arXiv, OpenAlex, Crossref, Semantic Scholar, Wikipedia, Wikidata, GitHub (repos, and issues/PRs with `is:issue`), Hacker News, Stack Exchange (`/similar` when full-text finds nothing), PubMed (esearch + efetch) and RSS/Atom (robots.txt, autodiscovery, per-feed cache). Web search tries SearXNG on loopback, then DuckDuckGo lite, then a daemon-supplied hosted search, and names why each one failed.
  - Every HTTP request a channel makes takes a slot from a process-wide limiter. A 429 holds the channel for `Retry-After` (30 s when absent), and Stack Exchange `backoff` is honoured. Requests carry `USER_AGENT`; nothing sends a credential.
  - `Channel::lookup` and `ChannelRegistry::lookup` ask the namespace authority (Crossref DOI, arXiv, PubMed, Wikidata, GitHub, OpenAlex, S2). `Deduper::unconfirmed` lists identifiers that only indexes agree on. When the authority's answer arrives, those sources fold into one (`alias_of`, `live`, `resolve`). A retraction from any route marks the source.
  - Dedup measured on recorded output (`tests/dedup_eval.rs`): 48 routes become 36 sources, with merge precision and recall 1.000 (naive title equality: precision 0.548). The negative and independence controls hold.
  - `marlowe-research::NetHttp` sends https through marlowe-net and plain http only to loopback (marlowe-provider). `marlowe-net` gains `fetch_with(target, Headers)` and `Fetched.retry_after`.
- **Changed.** The exec `arxiv` tool's Atom parser and query encoding moved to `marlowe_research::arxiv`. The tool now shares the process-wide arXiv limiter and sends the research User-Agent.
- **Known.** DuckDuckGo lite answers our honest User-Agent with a CAPTCHA (HTTP 202). The backend takes that as a refusal and rests for 24 h, recorded in the cache, and nothing tries to get around it. Keyless general web search therefore needs a local SearXNG. Semantic Scholar's shared keyless pool answered 429 to every attempt on 2026-10-09.
- **Added (deep research v2).** `marlowe-crew::playbook::research_v2` runs the seven stages, each checkpointed and reported as `ResearchProgress`:
  - **Plan.** The plan is drafted in a clean step (no tools, nothing fetched) from the goal, the user's profile and Mind recall. Memory is a source in its own trust bucket. The plan is priced, and the run waits `needs_you` with `PlanProposed`. `Supervisor::approve_plan` takes approve, edit or reject: an invalid edit is refused and the run keeps waiting; a reject ends the run cancelled with the reason. `CrewConfig.research.auto_approve_under_tokens` exists and is off by default.
  - **Explore.** A breadth-b, depth-d tree. Channels are queried concurrently and results deduplicated by identity across the run. Scouts select by number from the results and from the link tables of pages their parent read, so the model never writes a URL. One `SourceBudget` is shared by every scout. The corpus fetcher reads a whole wave at `fetch_concurrency`, and gaps become bounded follow-ups.
  - **Compress.** The new `compressor` role (Quick slot, no tools) turns each stored source, in chunks, into claims with verbatim quotes and locators. A quote not found in the stored text is dropped and counted. A claim the source attributes to another work becomes a derivation edge.
  - **Corroborate.** Claims are grouped with stance and counted over independent roots: a blog repeating a paper is one root with it.
  - **Verify.** The new `verifier` role reviews every claim group and checks every claim against its re-read stored passage. A missing verdict is asked for once more. Incomplete verification ends the run with a refusal naming what is missing, plus `refusal.md`, and no report.
  - **Synthesise.** Sections per sub-question and the answer cite claim ids. Every cited sentence is checked again against its passages: unsupported ones are removed and listed, partial ones marked. The harness numbers references from source identities (DOI, arXiv, PMID, Wikidata, repo). Confidence comes from verification and corroboration. Output is `report.md` plus `research.json` (report, checks, corroboration, counts, effective citations).
  - **Remember.** The answer and up to five established findings are stored as Result memories with the run's provenance, once.
- **Added.** Approving a plan approves its quote: the run's token limit rises to the quote plus 25% and its steps limit to `step_estimate(plan)` (never lowered; unlimited stays unlimited). An edited plan is priced again first. Spend beyond that asks as before.
- **Fixed (live, marlowe-dusk:27b-super).** A step on a local thinking model could spend its whole output budget (`num_predict` 8,192) thinking and give no answer. One grouping prompt took 17,382 tokens and 223 s that way, and 4 s with thinking off. Every v2 step that fails now runs once more under `<key>:again`, and a refusal names the calls that failed. (The first version retried with thinking off; that was replaced, see "Fixed (fe527eba)".)
- **Fixed (live).** The claim reviewer refuted well-supported groups for being off-goal or for repeated sources, which the run already counts. The review prompt no longer shows corroboration counts, and it judges the statement against its passages only. Repeated routes no longer pile up in dedup ("also on arxiv, arxiv, arxiv"). A reply cut off mid-marker (`[C3, C6,`), a model-written `Confidence:` line and a `Section 1/5:` prefix are stripped. A partial mark goes before the citation (`… (partly supported) [3].`). Sentences only about the evidence carry no citation, and grading words (established, suggested) are not support-checked.
- **Changed.** The cost quote is calibrated on the live runs: 11 s per kilotoken locally, 6k compression and 5k verification tokens per source.
- **Measured.** `tests/research_v2_live.rs` (marlowe-dusk:27b-super, a fixed six-page web plus one memory, the goal "What LoRA rank should we use for fine-tuning 7B models?"):
  - Before the fixes: one run refused (8 of 9 groups unreviewed), and the next three reported at citation accuracy 0.83, 0.77 and 0.79.
  - After them: 0.95 (9 supported, 1 partial) in 855 s, then 1.00 (12 of 12) in 680 s with 23 steps and 82.5k tokens.
  - Confidence stayed low, because no group was established.
  - The quote for that run (204k tokens, 38 min) was 2.5 to 3.5 times the actual on these short pages.
- **Added.** `ResearchIo` / `PageFetcher` / `ChannelIo` (registry plus corpus fetcher), `CrewSetup.research` and `CrewDeps.research`. The binary passes `None` until a client can answer `PlanProposed`. With it, deep research runs v2; without it, v1. The daemon forwards `PlanProposed` and `ResearchProgress` and routes `ApprovePlan` to the supervisor. Data checkpoints are transcript rows with role `playbook`.
- **Changed.** Role cards: `researcher` and `scout` are rewritten for v2 (plan routing and sizing; selection by number). New cards `compressor` and `verifier` bring the built-ins to seven.
- **Added (extraction).** `marlowe-extract::main_content` reads HTML with three candidates: rs-trafilatura (page-type classification, predicted quality as confidence), rust-trafilatura and html.rs as the never-empty fallback. Both trafilatura ports are cargo features, on by default.
  - The first acceptable reading in policy order wins. Quality first (owner's call): rs-trafilatura is only capped at 1 s, for pathological pages (a 1 MB Wikipedia article takes 160-190 ms); from 400 ms on, rust-trafilatura races it on a worker, and at the cap whatever has answered is used, else html.rs. Pages whose URL looks like a forum are never capped. `SelectionPolicy::latency()` (40 ms cap, race from 10 ms) holds p95 under 50 ms for callers that need it.
  - The chosen text is mapped back onto html.rs blocks by word trigrams, so markdown keeps inline links, headings, tables and code. html.rs now records each block's kind and links, keeps `<pre>` line breaks, and emits MathML `alttext` as `$...$`.
  - Measured with `tools/extraction_eval.py` on the WCXB test split (511 pages), shipped default (`data/runs/extraction-eval/tracka-quality-1s`): F1 0.793 → 0.894, forum 0.720 → 0.836, WCXB dev 0.758 → 0.846, link recall 0.930 → 0.937, no page type regresses. **p95 is above the 50 ms budget**: 52.8 ms on WCXB test (48.5 ms in a 250 ms-cap run; ±5 ms of noise from other builds) and 63.6 ms over all 2,189 pages (max 467 ms). Shipped anyway (owner's call): network fetch dominates research latency. The latency preset (run `final`) reads p95 49.3 ms over all pages at F1 0.890 (dev 0.838, link recall 0.928).
  - `rust-toolchain.toml` pins Rust 1.98.1, which rust-trafilatura needs.
- **Added.** `marlowe-extract::paper` turns a paper into title, abstract, numbered sections and a parsed reference list (DOI, arXiv id, year, title guess). It reads arXiv's LaTeXML HTML, e-print LaTeX (gzip, tar or bare `.tex`; `\input` inlined; `.bbl`, `thebibliography` or `.bib`) and PDF text.
- **Changed.** The `paper` tool reads an arXiv id as HTML, then LaTeX source, then PDF. Every step passes the host gate, and arxiv.org's robots.txt disallows `/e-print`, so the LaTeX step reports the refusal and the PDF is read. `part: "references"` returns the reference list. GROBID stays an optional sidecar that is not wired up (`docs/research/grobid.md`).
- **Added.** `marlowe-exec::corpus` restores `fetch_and_extract` with a concurrency knob. Each host gets one request in flight, hosts are served round-robin, and every request and redirect hop passes robots.txt and per-host spacing (at least 1 s, or the site's `Crawl-delay`). A short `Retry-After` is honoured once. Redirect locations are sanitised before they are echoed. Pages come back as markdown, a `DocRef` and a link table.
- **Added.** `marlowe-exec::render`: a thin client-rendered page is rendered through the system Microsoft Edge over CDP (`headless_chrome`, no tokio). Edge runs headless in a fresh profile inside a kill-on-close Job Object, with a hard 30 s timeout, at most two at once, and downloads denied. Without Edge, or when rendering fails or the result is still thin, the page is reported unreadable (`client_rendered`) rather than passed off as the server HTML.
- **Changed.** `web_fetch` now reads through the corpus path: robots.txt, host spacing, main-content markdown and the Edge escalation. Every page fetch sends the research User-Agent.
- **Added (integration).** Deep research v2 is on in the product. `src/research_wire.rs` in the binary builds the keyless registry, `NetHttp`, `<home>\cache\research` and the corpus fetcher (as crew's `PageFetcher`), and reads `<home>\research.json`: SearXNG URL, DuckDuckGo, hosted search, `auto_approve_under_tokens` (off), fetch concurrency, feeds, Wikipedia language and Stack Exchange site. Unknown fields are refused, and there is no credential field. A scripted (debug) daemon stays on v1.
- **Added.** Answering a plan: `WatchRun` re-sends a pending `PlanProposed`. `marlowe crew plan <run>` shows the plan and quote (`--json` prints it); `approve [--breadth N] [--depth N] [--sources N]` narrows it, `edit <plan.json>` replaces it, and `reject [reason]` ends the run. The TUI Run view shows the plan with its quote, and `a` approves, `x` then `y` rejects. A toast says when a plan is ready.
- **Added.** A finished run writes `bundle.json` (`marlowe.research-bundle/1`) next to `report.md`. Every numbered reference carries the passages the report cited from it and the stored document text. `tools/research_produce.py` drives one question through the daemon (dispatch, approve narrowed to caps, wait, copy the bundle), and `research_eval.py run --questions` limits a run to some questions.
- **Fixed (live).** A query starting "2025–2026" panicked `arxiv::versioned_id` (a byte slice inside the en dash), and the panic in one search thread failed the whole run. The slice is now char-safe, and a channel search that panics is recorded as one failed query.
- **Fixed.** Markdown from the main-content path now keeps links in headings, and matches anchors with their whitespace collapsed. URL-level link recall goes from 0.604 to 0.641 on WCXB test (dev 0.583 to 0.614), with F1 unchanged.
- **Measured (extraction, quiet machine, run `m45-integrator`).** On WCXB test the shipped default has F1 0.894 against html.rs 0.793, forum 0.836, no page type regressing, and **p95 43.1 ms**, so it passes the switch-default bar on every check. The earlier 52.8 ms reading was noise from other builds. Over all 2,189 pages p95 is 57.6 ms. ScrapingHub token F1 is 0.951 (html.rs 0.803), shingle F1 0.956.
- **Fixed.** A v2 step whose answer was cut off at the output limit (`StepResult.truncated`, from `StopReason::MaxTokens`) now runs once more as `<key>:again`, like a step that gave no answer. A cut-off answer is kept over a retry that fails outright. Found live: a report section ended mid-sentence. What happens when the retry is cut off too is under "Fixed (fe527eba)".
- **Measured (live, run `fe527eba`, debug build, marlowe-dusk:27b-super).** The goal was 2025–2026 pruning methods for 20–30B LLMs at 20–30% sparsity. The plan had 5 sub-questions, breadth 4, depth 1 and 40 sources. It was quoted at 674k tokens and 124 min; the run used 744k tokens, 131 min and 104 steps, at $0.
  - 66 queries; 180 sources after dedup; 23 read, 5 unreadable; 29 channel queries or page reads failed (Semantic Scholar 429; no web-search backend).
  - 114 quoted claims in 75 groups over 23 independent roots: 7 established, 68 suggested, 0 contested. Review upheld 73, weakened 2, refuted 0.
  - 39 cited sentences checked: 35 supported and 4 partial. Citation accuracy is 0.949 with partial counted as half (the report printed it as 0.95) and 0.897 strict, and it was self-judged. 18 of the 39 verdicts came from retries with no reasoning, and all 18 said "supported". So the ADR-037 target of 0.95 is **not met**. There were 60 effective citations and 12 references; [11] (the HTML page of [10]) had no identifier. Confidence was printed as medium; with the abs/html pair merged it is low. 3 verification calls gave no answer.
  - Read by hand, the run is not good enough yet. It missed known 13B–70B pruning results because of thin retrieval. It counted one arXiv paper's abs and HTML pages as two roots. It spent a section on an off-topic paper. One section came out empty and one was cut off mid-sentence. Details are in `docs/research/deep-research-v2.md` §3.
- **Fixed (fe527eba).** These fixes come from the lead's read of the run and the review findings. They are verified on CPU with scripted providers and recorded fixtures (`research_v2/tests_quality.rs` and `tests_types.rs`, plus unit tests in each crate). The live re-run waits for the GPU.
  - **Empty and cut-off answers (cause).** A step got `num_predict` = window/4 = 8,192, and dusk's thinking spent that same budget. For example, `synthesise:q5` used 7,865 reasoning tokens out of 8,192. Research steps now ask for 12,288 tokens (`FIRST_OUTPUT`), lowered by `output_cap` to the room the prompt leaves in the window. Ollama caps any request at half the window: 16,384 on dusk. A retry keeps the card's effort and doubles the budget. Nothing is re-asked with thinking off any more. A failed step's transcript row records its output and reasoning tokens, its cap and whether it stopped at the limit.
  - **Verifier rubber-stamping.** A verification retry keeps its reasoning, gets a larger budget (`VERIFY_OUTPUT`) and splits its batch down to one item. Test: a verifier that answers "supported" whenever it is asked without reasoning cannot raise accuracy.
  - **Truncation.** `crew_run_steps` has a `truncated` column (migrated in place), and a resumed run replays it, so a stored cut-off step is asked again. A section or answer that is still cut off after its retry is cut back to its last full sentence and gets `CUT_NOTE`. The cut is listed in coverage and caps confidence at low.
  - **Identity and corroboration.** Landing pages and PDFs fold to one URL for ACL Anthology, OpenReview, PMLR and NeurIPS proceedings, as arXiv abs/html/pdf/versions already did. Before corroboration, a fetched page's `citation_doi`, `citation_arxiv_id`, `citation_title` and `citation_author` meta tags, plus arXiv's stamp in the first 4 KB, pair it with the work it names (`page_route`). So a journal repost of an arXiv paper is one root with it.
  - **Standing words in code.** `standing.rs` checks phrases like "two independent sources", "established" and "corroborated" against the root count of the cited claims. An overclaim is rewritten ("one source reports …") and listed in the report's checks. The audit prompt says the harness checks these words.
  - **Confidence and coverage.** `Coverage` covers sources read against the budget (below 60% is thin), the share of failed queries (above 25% is thin), blind sub-questions, sections with no evidence of their own and cut-off parts. Any of these caps `derive_confidence` at Low and leads the report and the dispatch summary. The fe527eba counts, rebuilt with the NOT pair merged, give Low. Accuracy prints both readings to three decimals ("0.897 strict, 0.949 with partial as half"), and `bundle.json` carries `run_citation_accuracy_strict`.
  - **Retrieval.**
    - The web channel is probed before planning (SearXNG `/healthz`, resting backends). A dead web channel is left out of the plan, with the reason shown in the proposal as "Not searched: …".
    - A 429 is waited out (`Retry-After`, or 30 s, honoured only up to 60 s) and retried up to twice. A node whose queries all failed is widened to two live scholarly channels.
    - The plan gives 2–4 keyword queries per channel (method and model names). Each node sends up to 3 queries per channel, arXiv uses field syntax with the goal's years as a `submittedDate` floor, and citation chasing goes through Semantic Scholar and OpenAlex from the strongest papers read.
  - **Relevance.** The reviewer marks a group `; off-goal: <why>`, and an off-goal group reaches neither a section nor memory. A follow-up that has drifted from the goal gets the goal's key terms appended.
  - **Empty sections.** Claims are grouped across the whole run in chunks, with a merge pass, so a finding read under q1 and q2 can corroborate. A section with no evidence of its own gets the closest groups from other sub-questions, labelled indirect.
  - **Remember.** A finding is tainted if the run's taint latch is set, if it rests on any network source, or if it rests on an untrusted recalled memory (`Recalled.trust`). Every recalled memory it rests on is passed as its derivation lineage (`remember_derived`), so the store caps the finding's trust at its parents' (an inferred parent caps it at inferred). Its text names the run and the source URLs. Only established, on-goal groups with two roots after page pairing are stored as findings.
  - **Egress and keys.**
    - Edge gets a cleared environment (no `scrubbed_env()` variable reaches it). Every request a rendered page makes is paused through CDP `Fetch` and continued only to https hosts whose every address is public. A new document must also pass the host gate, and the final URL is checked the same way. Host-resolver rules unmap `localhost`/`*.local`.
    - A model-written URL never becomes a request: query text drops URL tokens, a `GAP:` line carrying a URL is dropped, and RSS reads a feed URL only when the user configured it (`ChannelQuery.feed_url`).
  - **Robustness.**
    - A robots.txt fetch that was cancelled is not cached. Unreachable and 5xx deny-all entries expire after 5 min and parsed entries after 24 h (RFC 9309), with at most 4,096 hosts (least recently used dropped).
    - A non-loopback http `searxng_url` is reported on stderr and ignored rather than panicking the daemon at boot. `keyless_with` skips and records a channel that fails registration.
    - The GPU verdict is `Unknown` (so CPU) when NVML failed to open or a device's compute list could not be read; it never reads as an empty list.
- **Pending (GPU).** The 10-question research eval and any re-score of `fe527eba`'s `bundle.json`. The judge is local dusk, and the GPU belongs to a training job for the night of 2026-10-09; OpenRouter answers 403.
- **Verified (fix stage, CPU only, 2026-10-10).** `cargo check --workspace --all-targets` is clean apart from 4 old non-snake-case test names in marlowe-memory. `cargo test --workspace --no-fail-fast` (one job, one test thread, by `scripts/cargo-idle.ps1`) gives 1,622 passed, 0 failed and 30 ignored over 124 test targets, including the bin's (`context_live` is ignored, live). The embedder (8) and cross-encoder (11) reference fixtures pass. No GPU, Ollama or network step ran.
- **Fixed.** `marlowe-mcp`'s `Deadline::passed` uses `>=`, so a 0 ms deadline counts as reached when two reads of the clock are equal (a flaky test).

### M4 Crew

- **Added.** `marlowe-os`, the only crate with `unsafe`:
  - `JobObject::new_kill_on_close`, `assign`, `terminate`, `active_processes` and `release`.
  - `spawn_in_job[_with_flags]` starts the child suspended, assigns it, then resumes it, so no grandchild can escape the job.
- **Changed.** `marlowe-exec` runs every bash, powershell and python child in its own job; a timeout, flood or cancel kills the whole tree.
  - The conductor's background processes still outlive the call (`Leftovers::Release`). A crew host kills them when it is dropped (`KillWithHost`).
  - New builders: `with_child_env`, `with_shell_dir` and `with_research`.
  - `run_script` with `SpawnOpts` for callers outside a tool host.
- **Added.** `arxiv{query, max}` (export.arxiv.org Atom, 3 s global rate limit, 24 h cache in `<home>\cache\arxiv`) and `paper{url_or_arxiv_id, offset?}` (PDF/HTML through marlowe-extract, 12k-char windows). Both are untrusted content, like `web_fetch`.
- **Added.** `marlowe-crew`:
  - Role cards: `roles/{researcher,engineer,reviewer,analyst,scout}.md` are built in, and `<home>\roles\*.md` overrides them by name.
  - `Supervisor`: queue, provider limits (2 cloud, 1 local), GPU-busy fallback to a cloud slot, a thread per run behind `catch_unwind`, cancel, pause, resume and steer. A budget overrun goes to NeedsYou with an approval to extend.
  - `RunStore` (`crew_runs`, `crew_run_steps` in `cowork.db`): a run resumes after a restart from its last finished step.
  - Sandbox: worktree or scratch, a no-push git environment, and the dangerous-shell matcher. Merge and discard are done by the daemon only.
  - The `deep_research`, `build` and single-step playbooks, and the conductor-only `dispatch` tool with its estimate.
- **Added.** Proto: `Dispatch`, `ListRuns`, `WatchRun`, `Steer`, `CancelRun`, `PauseRun`, `ResumeRun`, `MergeRun`, `DiscardRun` and `ListRoles` requests; `RunUpdated`, `RunStep`, `RunFinished`, `Runs`, `RunTranscript` and `Roles` events; `ApprovalRequested.run`. All additive, so `PROTO_VERSION` stays 1.
- **Added.** Daemon:
  - Starts the supervisor with Brains, Mind, the Sentinel GPU probe and run approvals.
  - `dispatch` and `persona/blocks/crew.md` are exposed only while the crew runs.
  - Run events go out on the bus.
  - A finished or failed run is relayed into its parent session: a relay turn when the session is idle, otherwise at the next step or after the current turn when the result is untrusted. Relay text never enters memory at user trust.
  - `crew_merge` and `crew_discard` approvals.
- **Added.** CLI: `marlowe crew [list|show|roles|dispatch|steer|cancel|pause|resume|merge|discard]`.
- **Added.** TUI:
  - A live CREW rail (● running · ◐ needs you · ✓ done · ✗ failed · ○ queued).
  - The Run view (`^r`): streaming transcript, steer box, pause and cancel, a diff summary with merge and discard for build runs, the rendered report for research runs.
  - The `^d` dispatch prompt, crew toasts, and approvals that name their run.
- **Added (integration).** Debug builds only: `MARLOWE_SCRIPTED_PROVIDER=<script.json>` replaces both providers with a rule-scripted one, with in-memory credentials and no web search. It is used for the end-to-end check of the real binary. Release builds do not compile it.
- **Fixed (integration).** Live `RunStep` events numbered text by model call, while the transcript numbers rows by step. With parallel scouts, or a step that made tool calls, the TUI showed one step's text under another's label. Each step now reserves its transcript row on its first event.
- **Fixed (integration).** `marlowe ask` answered crew-run approvals raised in its session. With `--yes` it would have allowed a crew `git push`; with a closed stdin it raced the TUI and exited 2. It now answers only its own turn's approvals.
- **Security (review).** The crew push block is **best-effort**. It relies on `GIT_CONFIG_*` variables, which a child process can clear. Crew shells are pinned to the sandbox as their cwd but are not OS-confined, so a child can still write wherever the user can. ARCHITECTURE now says so; OS confinement is not built.
  - Partial mitigation: the shell policy asks for any command that mentions git's environment. Python code, and scripts passed to bash, sh, python, pwsh or node, ask when they mention `git … push`.
- **Security (review).** A worktree shares `.git/config` and hooks with the original checkout.
  - The sandbox records a digest of the repo config (minus `branch.*` and `user.*`) and the hooks. Diff and merge refuse once that digest changes.
  - The daemon's own git calls run with hooks and fsmonitor off, no external diff or textconv, API keys scrubbed and the no-push environment.
  - Crew shells ask for `git config` writes, `update-ref`, `symbolic-ref`, branch delete/rename/force, `tag -d`, `gc`, `reflog expire`, `worktree`, `remote` changes, `stash drop|clear` and `checkout -B`.
- **Security (review).** Git commands aimed outside the worktree (`-C`, `--git-dir`, `--work-tree`, or a `cd` out of it) ask unless they only read. Deletes ask whether or not they are recursive, and so do copies, moves, writers and `>`/`>>` redirects whose target is outside the sandbox.
- **Security (review).** `dispatch` asks after the conductor has read untrusted content, and the run starts tainted, so its shells ask from the first step. The model cannot set the taint itself.
- **Fixed (review).** A run that read untrusted content relayed untainted unless its role had web tools. `CrewRunInfo.tainted` now carries the run's own taint into the relay.
- **Fixed (review).** In production every crew read was marked untrusted, and from then on every shell and python call asked. The sandbox sits under `%LOCALAPPDATA%`, which the broad-root rule refuses. Crew hosts now trust the sandbox explicitly (`BuiltinTools::new_sandbox`), and reads in the original workspace (`with_read_root`) do not taint.
- **Fixed (review).** Cancel lost to pause. Cancelling a run that was waiting on a budget approval left it Paused, and a resume then ended at once. The user's cancel now outranks pause and shutdown, and the daemon cancels before denying the pending approvals.
- **Fixed (review).** Parallel scouts could spend up to about twice the remaining budget before the approval. Usd also went unchecked within a turn when the provider reported no cost. Every model call is now admitted against the run's spend plus the calls still in flight.
- **Fixed (review).** The GPU hold and the local limit applied only to the dispatched role.
  - Every step now resolves its slot, and holds at "waiting for the GPU" when no cloud slot is configured. At most one local model stream runs at a time across all runs.
  - The GPU probe no longer counts load on a device that Ollama is using, so a step does not wait on the model the previous step left loaded. A training run is detected by the Sentinel's watched runs.
- **Fixed (review).** Relays were lost on restart. `Run.relay_due` is now cleared only once the result is in the session's saved history, and the daemon relays every run still due at boot. `RunFinished` is published after the relay is queued.
- **Fixed (review).** A resumed build reported "green on the first run" and led with the stale implement text. The test loop now resumes after the fix rounds it already completed.

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
