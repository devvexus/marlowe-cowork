# Projects: design

Status: design, nothing built. Supersedes [deep-research-teams-design.md](deep-research-teams-design.md) as the plan of record; that file stays as the source of the research invariants and the v2 stage map. Tags: (m) measured, (e) estimate, (u) unmeasured.

## 0. Summary

A project is a durable, staged piece of work that a small model can start and a crowd of single-task models can finish.

- **The manager designs, it does not work.** The user-facing agent fills one small JSON plan (objective, questions, the fields it wants back, deliverable) and calls `project{action:"create"}`. It returns an id at once. Nothing else the manager does is part of the project.
- **Phases come from a menu and are data.** Plan, DataCollection, DataValidation, Thinking, Checkin, Report, Finish now; Engineering later. A phase is a list of steps. A step is one of four primitives: fan-out of fresh-context employees with a forced form, a code step, a delta merge, a gate. New phase types are menu entries, not programs.
- **No model does two stages.** An employee gets one task, one fresh context, no tools, and a JSON schema its output is forced into (grammar, not hope). A form that does not parse and validate is retried, then recorded as failed; the run goes on.
- **Code does everything that is not judgement.** Search, fetch, link numbering, URL filling, quote location, claim ids, credibility, scores, independence, merging, scheduling, journaling. The model never writes a URL, a source, an id or a score.
- **Stages are isolated.** Only validated forms travel from one stage to the next, free text in them is length-capped by the grammar and fenced as data downstream. A hostile page can corrupt one employee's form and nothing above it.
- **Everything is JSON, and the screen is derived from it.** Each form field carries a display hint (badge, link, table, progress, meter, duration, agent-ref). One generic renderer draws any project, including kinds nobody wrote screen code for. Code fills live fields (timestamps, cpu, gpu, size, runtime) and the agents assigned to every item.
- **The conversation never blocks.** The project runs on its own thread as a durable run. The plan and every checkin park the run with no thread and no slot held; the answer re-queues it.

### One concrete example

Matthew: "Run the web research team task. Focus on the latest upcoming AI models, specificially new Kimi models, deepseek, and qwen4. Compile a report md and send it to me when you are done"

1. **Manager (one tool call, about 250 tokens).** Research request, so a research project (decision 5):

```json
{"action":"create","plan":{
  "title":"Upcoming AI models: Kimi, DeepSeek, Qwen4",
  "kind":"research",
  "objective":"Find the newest and upcoming Kimi, DeepSeek and Qwen models (including Qwen4) and say what is confirmed and what is rumoured.",
  "questions":[
    {"q":"Which Kimi models (Moonshot AI) are newest or upcoming?"},
    {"q":"Which DeepSeek models are newest or upcoming?"},
    {"q":"Is Qwen4 released or announced, and what is known about it?"}],
  "fields":[
    {"key":"model_name","label":"Model","type":"string","key_field":true},
    {"key":"vendor","label":"Vendor","type":"string"},
    {"key":"status","label":"Status","type":"enum","values":["released","announced","rumoured","unknown"]},
    {"key":"release_date","label":"Release date","type":"string"},
    {"key":"size","label":"Size","type":"string"}],
  "deliverable":{"kind":"report","send":true},
  "workspace":"compact"}}
```

2. **Plan phase (code).** Validates and expands the plan (question ids `q1..q3`, defaults for limits), creates the project directory, builds the plan card (3 questions, up to 40 sources, 5 fields, estimated model calls, the network grant in plain words) and parks. The tool returns `{"id":"p_7f3a91c2","status":"awaiting_plan"}`. The manager tells Matthew in two lines that a plan is ready; he approves in the card or says "go" and the manager calls `project{action:"answer",answer:"approve"}`. Approving also grants the run its public-https reads (decision 1).
3. **DataCollection.** Per question an AttackVectors employee (mini model, fresh context, told today's date and that its knowledge is old) writes up to 5 search angles and up to 4 names it suspects exist but does not know ("qwen4?"). Code turns these into queries plus date-worded variants, searches the keyless channels, dedups, picks sources by quota, fetches with robots.txt and per-host spacing, extracts markdown with numbered links `[L12]`, stores the text. Each readable page starts an Analyze employee at once (no waiting for the others). Analyze returns the form in section 2.4: relevancy, one line, credibility inputs, claims with verbatim quotes, additional sources by link number, and one `records` row per model it found with the five fields above.
4. **DataValidation.** Each finished Analyze triggers Extract (code): quotes located verbatim in the stored page or the claim is dropped; claim ids derived from source and position. At 25/50/75/100% of Analyze, Condense merges only the new claims into condensed claims (code attaches the verbatim originals, counts independent roots, scores). A Verify employee per condensed claim re-reads the corroborating passages and returns a verdict. Output: verified claims and sources.
5. **Thinking.** The lead (the larger model) receives forms only: the plan, verified claims by question, coverage, the best flagged additional sources. It returns `report` or `keep_researching` with new questions. If it has flagged sources it always recommends keep_researching at a Checkin (Matthew's rule).
6. **Checkin (if raised).** A card: Approve / Keep researching (N flagged sources) / Steer (text) / Cancel. Keep researching creates a continuation plan that inherits everything and starts DataCollection again on the flagged sources.
7. **Report, Finish.** One section employee per question writes from verified claims only, an answer employee writes the summary, a verifier re-checks every cited sentence, code assembles `report.md` with a bibliography from stored identity. Finish copies it to `<home>\output\<slug>-<id>\`, archives the final project state, disposes of scratch, and the relay posts to the session: confidence, first paragraph, path, and "send the file" because `send` was set.

Work size for that run (e): 3 AttackVectors calls, about 30 Analyze, 4 Condense passes per question (12 calls at most), about 20 Verify, 1 lead call, 4 report calls, about 6 audit batches: roughly 80 short single-task calls instead of one 35-agent run with 15 compactions (m, from the team design).

## 1. Concepts

| Term | Meaning |
|---|---|
| **Project** | One durable unit of work: a plan, a state JSON, a run, a directory. Replaces Crew/teams/workflows as the product concept. Has a `kind` (a template: research now, engineering later). |
| **Plan** | The manager's JSON: objective, questions, fields, deliverable, limits, workspace disposal. Small enough for a 9b model to fill. Always version-numbered (`plan_no`). |
| **Phase** | A named stage from the menu (Plan, DataCollection, ...). Declared as data: an ordered list of steps. Has a status, progress and one declared **output** (the only thing the next phase reads). |
| **Step** | One primitive inside a phase: `fanout`, `code`, `merge` or `gate`. Steps connect by edges that are either `stream` (an item flows on the moment it is done) or `barrier` (wait for all). |
| **Employee** | One model call with a fresh context, one task, no tools, one form. It lives for one call (plus its retries). The manager and the lead are the only long-lived models. |
| **Form** | A field list that is both the JSON Schema an employee is forced into and the description of how each field is shown. Each field says who writes it: `model`, `code` or `plan`. Forms are versioned (`analyze@1`) and embedded in the project state, so the state is self-describing. |
| **Item** | One node in the project state: a question, a vector, a source, a claim, a verdict, a section, an experiment. Every item has a form, a status, data, live fields and assigned agents. |
| **Project state** | One JSON document (section 2.2) holding every phase, item, gate and agent. Mutated only by one writer; each change bumps `rev`. Everything the screen shows comes from it. |
| **Gate** | A step that parks the run for the user. Two kinds now: `plan` and `checkin`. |
| **Checkin** | A gate that shows what has been found and asks Approve / Keep researching / Steer / Cancel. Raised by the lead's decision, by the user at any time, or by code when failures or budget need a human. |
| **Continuation plan** | A new Plan that inherits the previous plan's objective, fields, deliverable, items and seen-source set, adds questions and optionally seeds sources. It is how "keep researching" and "steer" work. |

## 2. Schemas

Conventions. `W` = who writes a field: **M** model (forced by grammar), **C** code, **P** plan (the manager or the user). Model-written free text always has `max_len`. Models never write ids, urls, sources, scores, timestamps. Ids: `p_<8hex>` project, `q1..` question, `i_<8hex>` item (derived, see 4.8), `s_<8hex>` source (from the identity key, so independent of arrival order), `c_<8hex>` claim (blake3 of source, quote start, quote end), `cc_<8hex>` condensed claim, `a_<n>` agent.

### 2.1 Plan (what the manager fills)

Minimal for a 9b model: five required keys, the rest defaulted by code.

```json
{
  "title":      "string, 3-80 chars, required",
  "kind":       "research",
  "objective":  "string, 20-500 chars, required",
  "questions":  [ {"q": "string, 10-200 chars"} ],
  "fields":     [ {"key":"snake_case<=32", "label":"<=40", "type":"string|text|integer|number|bool|date|url|enum|list",
                   "values":["only for enum, 2-8 strings"], "key_field": false} ],
  "deliverable":{ "kind":"report|answer|files", "send": false },
  "workspace":  "compact",
  "phases":     ["DataCollection","DataValidation","Thinking","Report","Finish"],
  "limits":     { "max_sources":40, "max_fetches":60, "vectors_per_question":5, "max_rounds":3 }
}
```

Rules. `questions` 1-8 (required). `fields` 0-12; types limited to the list; `key_field` marks the field that identifies a record (at most one, default the first). `phases` optional, defaults from the kind template; unknown names are rejected with the menu. `workspace` is `keep`, `compact` (default) or `delete` (section 7.4). `limits` all optional, bounded by code (`max_sources` 1-120, `max_fetches` at least `max_sources`, `max_rounds` 1-5). Code adds: `plan_no`, `question ids`, `created_ms`, the model quote (calls and tokens, e), the grant text. A continuation plan adds `parent_plan`, `inherit` (item ids carried), `seed_sources` (additional-source items to fetch first), `steer` (user text, data).

### 2.2 Project state

```json
{
  "schema": "marlowe.project/1",
  "id": "p_7f3a91c2", "kind": "research", "title": "...", "session": "s_...", "run": "r_...",
  "status": "awaiting_plan | running | awaiting_checkin | finished | cancelled | failed",
  "rev": 412, "created_ms": 0, "updated_ms": 0, "finished_ms": null,
  "plan": { "...the current Plan as 2.1 + C fields..." },
  "plans": [ { "plan_no": 1, "approved_ms": 0, "decision": "approve" } ],
  "forms": { "analyze@1": { "...Form (2.3)..." } },
  "phases": [ {
      "id": "ph_dc", "type": "DataCollection", "round": 1, "status": "pending|running|done|failed|skipped",
      "started_ms": 0, "ended_ms": null,
      "progress": { "done": 12, "total": 40 },
      "steps": [ { "id": "analyze", "kind": "fanout", "form": "analyze@1", "status": "running",
                   "counts": { "queued": 3, "running": 5, "done": 12, "failed": 1 } } ],
      "output": null } ],
  "items": { "i_1a2b3c4d": { "...Item..." } },
  "order": { "question": ["i_..."], "source": ["i_..."] },
  "gates": [ { "id": "g_1", "kind": "plan", "status": "open|answered", "payload": {}, "options": [], "answer": null, "asked_ms": 0, "answered_ms": null } ],
  "agents": { "a_17": { "id":"a_17", "role":"analyst", "model":"...", "step":"analyze", "item":"i_...",
                         "status":"running|done|failed", "attempt":1,
                         "tokens":{"in":0,"out":0}, "started_ms":0, "ended_ms":null } },
  "metrics": { "cpu_pct":0, "gpu_pct":0, "vram_mb":0, "disk_free_mb":0, "fetch":{"ok":0,"failed":0,"blocked":0}, "tokens":{"in":0,"out":0} },
  "grants": { "net": { "scope":"run", "public_https_get":true, "robots":true, "quota":60, "used":43 } },
  "tainted": true
}
```

All of `rev`, `updated_ms`, `phases[].progress/steps/counts`, `agents`, `metrics`, `grants.used`, `tainted` are code-filled. Plan fields come from the plan. Model-written data exists only inside `items[].data`.

**Item.**

```json
{ "id":"i_...", "kind":"source", "form":"analyze@1", "parent":"i_<question>", "phase":"ph_dc", "step":"analyze",
  "status":"queued|running|done|failed|skipped|blocked|cancelled",
  "label":"short text for lists (code)",
  "data":   { "...fields of the form, model- and code-written..." },
  "live":   { "started_ms":0, "ended_ms":null, "runtime_ms":0, "cpu_pct":null, "gpu_pct":null, "size_bytes":null },
  "agents": ["a_17"], "attempts": 1, "error": null, "key": "p:ph_dc:analyze:s_9c1d2e3f:ab12cd34ef56" }
```

`error` is a harness-written reason ("form did not validate after 3 attempts: /claims/2/quote too long"), never model or server text. `key` is the journal key (4.6).

### 2.3 Form definition and display hints

```json
{ "form": "analyze", "version": 1, "title": "Source analysis",
  "item_label": "{data.index}. {data.title}",
  "fields": [ { "key":"...", "label":"...", "type":"...", "w":"M|C|P", "required":true,
                "max_len":140, "max_items":8, "min":0, "max":1,
                "values":[ {"v":"Critical","def":"...","weight":1.0,"tone":"bad","glyph":"!"} ],
                "of":"string", "columns":[ { "...field..." } ],
                "show": { "as":"badge" } } ],
  "rules": [ "named code checks run after the grammar, see 5.4" ] }
```

Field types: `string`, `text` (multi-line), `integer`, `number`, `bool`, `enum`, `url`, `timestamp`, `duration` (ms), `list` (`of` a scalar type), `table` (list of objects, `columns` are fields), `agents` (list of agent ids), `json` (anything, code-written only).

**Display hints (`show.as`)**, the whole vocabulary:

| Hint | Applies to | Renders as | Options |
|---|---|---|---|
| `text` | any scalar | plain text, wrapped, clipped with `...` | `lines` |
| `badge` | enum, bool | glyph plus colour from the value's `tone` and `glyph` | none (values carry them) |
| `link` | url | clickable `label ↗`; activating it opens the system browser; the raw url is one key away | `label_from` (field key) |
| `table` | table, list of objects | table with the schema-named `columns` | `columns` (keys, in order), `sort` |
| `progress` | number 0..1, or object `{done,total}` | `n/m` and a bar | `unit` |
| `meter` | number | bar against a range with warn and crit colouring | `min`, `max`, `unit`, `warn_at`, `crit_at` |
| `duration` | duration, timestamp pair | `00:42:10`, live while the item runs | none |
| `time` | timestamp | relative ("3 min ago") with absolute on hover | none |
| `agent_ref` | agents | agent chips; activating one opens that agent from anywhere | none |
| `json` | any | collapsed raw JSON | none |
| `hidden` | any | not shown (still in the JSON) | none |

`tone` is one of `ok`, `warn`, `bad`, `info`, `accent`, `dim`. The renderer maps tones to the locked palette (green `#8fd4a8`, amber `#d6a95f`, red `#dd7f85`, blue `#7fa8dd`, accent `#9B7EDE`, `#6a6480`). **A form never carries a colour value**, and a test rejects any hex string in a form. No background fills.

Two functions come from the form, both pure and in `marlowe-project`:

- `model_schema(form, plan) -> JSON Schema`: only `w: "M"` fields, in order, `additionalProperties:false`, `maxLength`, `maxItems`, `enum`, `minimum/maximum`, `required`. Project-specific fields from the plan are expanded into the `records` columns here. Display hints and code fields never reach the model.
- `view(item, state) -> Row`: resolves each field's hint into a typed cell (text, badge, link, table, bar, meter, duration, agent chips). The TUI is a thin painter of `Row`; section 3 shows it.

### 2.4 Research forms

**AttackVectors** (`attack_vectors@1`, one per question).

| Key | W | Type | Notes |
|---|---|---|---|
| `question` | C | text | the sub-question |
| `today` | C | string | the date in the prompt, ISO |
| `vectors` | M | table, max_items 6 | rows below |
| `unknown_names` | M | list of string, max_items 5, each max_len 40 | names or versions it suspects exist but does not know ("qwen4?") |
| `queries` | C | table | the expanded searches code actually ran: base keywords stripped by `query_text`, plus date-word variants (month and year, year, "latest", "released") and `since_year` floor |

Vector row: `angle` (M, enum `official`, `news`, `paper`, `code`, `discussion`, `benchmark`, `other`), `keywords` (M, string, max_len 80, no URLs: `looks_like_url` rows are dropped), `channel` (M, enum `web`, `scholarly`, `arxiv`, `community`, `feeds`, `wiki`; a channel not online is replaced by code), `why` (M, max_len 120). The prompt says: today is {date}; your knowledge ends well before today and may be over a year old; the thing asked about may have newer versions than you know; include one vector aimed at what you do not know.

**Page** (`page@1`, code-only item, the output of SearchAndFetch). `source_id` (C), `url` (C, final url after redirects, `link`), `title` (C, a citation title replaces a placeholder anchor such as "View PDF"), `channel` (C), `identity` (C, DOI/arXiv/repo or null), `date` (C), `status` (C enum `ok`, `thin`, `robots`, `http`, `unreachable`, `unreadable`, `over_quota`, `duplicate`; reasons are harness-written, from `why_not`), `credibility` (C, 2.4 below), `text_ref` (C, blake3 of the stored numbered markdown), `links` (C, count), `references` (C, table of `Reference`: doi, arxiv_id, url, year, label), `question` (C).

**AnalyzeOutput** (`analyze@1`, one per page, or per chunk when the page exceeds the window, merged by code).

| Key | W | Type | Notes |
|---|---|---|---|
| `index` | C | integer | 1-based per project, filled by the program |
| `source` | C | url, `link` | the exact url, filled after the model returns; the model never writes it |
| `title` | C | string | |
| `relevancy` | M | enum | Critical, High, Medium, Low, Skip, defined below |
| `source_kind` | M | enum `official`, `paper`, `repo`, `news`, `blog`, `forum`, `aggregator`, `other` | bounded input to credibility |
| `one_line` | M | string, max_len 140 | what this source offers this project |
| `notes` | M | string, max_len 300 | short |
| `claims` | M | table, max_items 8 | `text` (M, max_len 240), `quote` (M, max_len 300, verbatim from the page), `sub` (C, question id) |
| `additional_sources` | M | table, max_items 8 | `link` (M, integer: a `[L<n>]` number on the page, or null), `reference` (M, string, max_len 160: a citation or name when there is no link, or null), `opportunity` (M, same labels as relevancy), `notes` (M, max_len 160, how it was referenced); `source` (C, url) filled by code |
| `records` | M | table, max_items 10 | one row per entity found, columns are the plan's `fields` (each nullable: null when the page does not say). This is the project-specific section |
| `relevance` | C | number 0..1 | weight of `relevancy` |
| `credibility` | C | number 0..1 | 2.4 below |
| `rejected` | C | json | counts of dropped claims, links and rows with the reason |

**Relevancy labels and the scoring rule.** The model sees these definitions verbatim in the prompt, each with the test it applies:

| Label | Weight | Definition (the test) |
|---|---|---|
| Critical | 1.0 | The page itself answers a core part of the question: it names the subject and states the fact asked for (a release, a size, a date, a result). Test: could you quote one sentence as the answer? |
| High | 0.7 | About the exact subject and gives usable data for it, but not the answer itself (specs, a benchmark, an official post about something nearby). |
| Medium | 0.4 | About the same family or topic: context, comparisons, an adjacent release. The subject appears but with little data. |
| Low | 0.15 | No data on the subject seen, but it points to places that might have it (a link list, a roundup). Low hit chance. |
| Skip | 0 | Nothing usable for this question. Claims are ignored. |

Rule. `relevance = weight[relevancy]`. Code lowers a label by one step and records `rejected.relevancy` when the model gave Critical or High but no claim survived quote location (it said the page answers, and could not show it). `priority = relevance x credibility` orders sources for the lead, and `opportunity_weight x credibility(referencing source)` orders additional sources.

**Credibility (code, never the model).** `credibility = min(channel_cap, kind_base[source_kind]) x modifiers`, from a table in code with a version number and tests.

| Input | Source of truth | Effect |
|---|---|---|
| `channel_cap` | the page's `SourceRef.channel` and id confirmation | scholarly or pubmed with a confirmed DOI/PMID 0.95; arXiv 0.8; repo with stars (from `meta`) 0.6 to 0.8 by stars; wiki 0.6; community/feeds 0.5; web 0.7 for `official`, else 0.5 |
| `kind_base` | the model's `source_kind` | official 0.9, paper 0.9, repo 0.75, news 0.6, blog 0.45, forum 0.35, aggregator 0.3, other 0.3. The model can lower a source below the channel cap, never raise it above |
| modifiers | code | retracted sets 0; an identity asserted by page text but not confirmed by a lookup x0.85; a thin page x0.8 |

The model's only input is `source_kind`. A page that lies about being official can lift credibility to the channel cap at most, and the channel cap for plain web is 0.7 only when the kind is `official`.

**Claim** (`claim@1`, code-only, made by Extract). `id` (C, `c_` + blake3 of source, quote start and end), `text`, `quote`, `locator` (start and end bytes in the stored page), `source` (item id), `sub`, `credibility`, `status` (`new`, `condensed`, `rejected`). A claim whose quote is not found verbatim is never created (counted in `rejected`).

**CondensedClaim** (`cclaim@1`).

| Key | W | Notes |
|---|---|---|
| `claim` | M | the merged statement, max_len 240 (written by the Condense employee) |
| `corroborates` | C | table: the verbatim originals (claim id, text, quote, source url, credibility), attached by code from the store, never rewritten by a model |
| `source_count` | C | number of **independent roots** (three blogs on one paper count 1), from the derivation graph |
| `score` | C | `min(1, sum over independent roots of the best credibility in that root)`; two roots at 0.5 reach 1.0. A weighted sum of believability with independence applied and a cap |
| `stance` | C | `supported` or `contested` (any member the Condense employee marked as contradicting) |
| `verdict` | C | from Verify, below |
| `sub` | C | question id |

**CondenseOutput** (`condense@1`, one per question per pass). The employee sees existing condensed claims as `(id, text)` and new claims as `(id, text)`. It returns `groups`: table, each `{ "members": [claim or cclaim ids], "claim": text max_len 240, "contradicts": [ids] }`. Code: ids it did not receive are dropped, a claim in two groups goes to the first, a new claim in no group becomes a singleton group, so none is lost. The employee never writes a quote.

**VerifyOutput** (`verify@1`, one per condensed claim). The employee sees the claim and up to 6 passages re-read from the stored pages (the quote with 320 bytes of context, best credibility first), fenced.

| Key | W | Notes |
|---|---|---|
| `reason` | M | string, max_len 200, **first in the schema** so the model writes its reasoning before it decides |
| `verdict` | M | enum `upheld`, `partial`, `refuted`, `unsupported` |
| `corrected_claim` | M | string, max_len 240, only meaningful for `partial`, else null |
| `used` | M | list of integer (passage numbers), max_items 6 |

Code: a claim whose stored quote no longer matches the page is `unsupported` with no call. A verdict that never arrives is `unsupported`. `partial` replaces the claim text with `corrected_claim`.

**Phase output of DataValidation** (`validated@1`, the hand-off): `claims`: list of `{cclaim id, claim, verdict, score, source_count, sources:[source item ids]}` for `upheld` and `partial` only; `sources`: list of `{source item id, url, title, priority}` that back them; `records`: the tabulated rows (section 8.6); `coverage`: counts (pages planned, read, unreadable; claims kept and rejected; verdicts missing).

**LeadDecision** (`lead_decision@1`, Thinking, the lead model).

| Key | W | Notes |
|---|---|---|
| `why` | M | string, max_len 300, first |
| `decision` | M | enum `report`, `keep_researching` |
| `gaps` | M | table, max_items 5: `question` (max_len 200), `reason` (max_len 120). Becomes new questions |
| `focus` | M | list of integer, max_items 10: numbers of flagged additional sources worth reading (code maps to items) |

Code overrides: when flagged additional sources exist and `max_rounds` is not reached, the Checkin recommends `keep_researching` whatever the decision (Matthew's rule). With `report` and none flagged, no Checkin unless the user forced one or failures exceed the threshold (4.7).

**Report** (`section@1`, per question; `answer@1`, one; `audit@1`, per batch of cited sentences).

- `section@1`: `text` (M, max_len 4000, uses `[C<n>]` markers for claims it was given, up to 40 verified claims), `end` (M, const `"END"`, proves it was not cut). Code: markers not in the given set are removed, a section with no verified claims is the fixed line "Nothing the run read answers this." with no call.
- `answer@1`: `text` (M, max_len 2500), `end`.
- `audit@1`: `verdicts` table, each `{ "n": integer, "verdict": "supported|partial|unsupported", "reason": max_len 120 }` over up to 6 sentences with their re-read passages. Unsupported sentences are removed and listed; partial are marked; a sentence citing no verified claim is unsupported with no call.
- Code assembles `report.md`: sections, answer, a Records table (when the plan has fields), confidence derived in code (citation accuracy, established share, coverage caps, as v2), a "Not searched" line for channels that were down, and a bibliography from stored identity (`renumber`). The model never writes a reference or a url.

**Checkin card** (gate payload, code-built, `checkin@1`).

```json
{ "kind":"checkin",
  "reason":"lead_decision | user_forced | failures | budget",
  "round": 1,
  "summary": { "sources_read": 31, "claims_verified": 17, "contested": 2, "failed_items": 3, "elapsed_ms": 0 },
  "lead_note": "the lead's why, max_len 300 (fenced as data)",
  "recommended": "keep_researching",
  "flagged": [ { "n": 1, "opportunity": "High", "source": "https://...", "notes": "cited by 3 sources as the release post", "from": ["i_..."] } ],
  "proposed_questions": [ { "q": "..." } ],
  "options": [
    { "id":"approve", "label":"Approve and write the report" },
    { "id":"keep_researching", "label":"Keep researching (7 flagged sources)", "needs_text": false },
    { "id":"steer", "label":"Steer", "needs_text": true },
    { "id":"cancel", "label":"Cancel" } ] }
```

Answers: `approve`, `keep_researching`, `steer` + text, `cancel`. Keep researching lists the flagged sources and new questions as the continuation plan it will create; it needs no further approval (the user just chose).

## 3. Display hints in use: one renderer, any project

The renderer knows five things and nothing about any project kind: (1) the item tree (`parent`, `order`), (2) each item's form (embedded in the state), (3) the hint vocabulary, (4) the `live` and `agents` slots every item has, (5) a `rollups` list on the kind template for the overview tab. `view(item)` returns `Row { title, status_badge, cells[], agents[], children }`; the TUI paints rows, a raw-JSON view is always one key away.

**A research source (from the form above, no source-specific code):**

```
 3. Moonshot AI releases Kimi K2.6              [High] ●      cred 0.72 ▰▰▰▰▰▰▰▱▱▱   done  00:00:41
    Kimi K2.6 released 2026-10-20, 1T MoE, 256k context        https://example.org/kimi-k26 ↗
    claims 2   |  records 1   |  additional sources 3 [Critical 1, High 1, Low 1]
    ◆ a_17 analyst
```

The `relevancy` field has `show: badge` (values carry tone and glyph), `source` has `link`, `credibility` has `meter`, `records` has `table` (columns from the plan's fields), `additional_sources` has `table`, `agents` has `agent_ref`. The renderer did not know it was a source.

**An engineering experiment, rendered from its own schema with no UI code.** The Engineering phase (later) declares the item form below. Nothing else is needed for it to show:

```json
{ "form":"experiment","version":1,"title":"Experiment",
  "item_label":"Experiment #{data.n} ({data.name}) [{status}]",
  "fields":[
   {"key":"n","type":"integer","w":"C","show":{"as":"hidden"}},
   {"key":"name","label":"Name","type":"string","w":"P","max_len":80,"show":{"as":"text"}},
   {"key":"overview","label":"Overview","type":"text","w":"M","max_len":600,"show":{"as":"text","lines":4}},
   {"key":"size_bytes","label":"size","type":"integer","w":"C","show":{"as":"meter","unit":"bytes","min":0,"max":null}},
   {"key":"cpu_pct","label":"cpu","type":"number","w":"C","show":{"as":"meter","unit":"%","min":0,"max":100,"warn_at":85}},
   {"key":"gpu_pct","label":"gpu","type":"number","w":"C","show":{"as":"meter","unit":"%","min":0,"max":100,"warn_at":90}},
   {"key":"runtime_ms","label":"runtime","type":"duration","w":"C","show":{"as":"duration"}},
   {"key":"agents","label":"agents","type":"agents","w":"C","show":{"as":"agent_ref"}}]}
```

Rendered:

```
Experiment #1 (Trellis Encoder Compression) [running]   size 1.2 GB   cpu ▰▰▰▱▱▱▱▱ 41%   gpu ▰▰▰▰▰▰▱▱ 78%   runtime 00:42:10
  Compresses the trellis encoder activations 4x with a learned codebook; checks loss on the held-out split every 500 steps.
  ◆ a_31 engineer   ◆ a_32 monitor
```

The `[running]` badge is the item's status; `{...}` in `item_label` is filled from `data`. The live slots are code-filled from `marlowe-os` process info and the GPU probe the daemon already has; the runtime counts from `live.started_ms`. A kind the manager invents mid-project, or fields it adds to the plan, render the same way because the renderer only reads the form.

**Overview tab.** The kind template lists rollups, e.g. `{"title":"Sources","over":{"kind":"source"},"by":"data.relevancy","show":"badge"}`, `{"title":"Claims","over":{"kind":"cclaim"},"by":"data.verdict"}`, `{"title":"Agents","over":"agents","by":"status"}`. The overview is counts and bars computed from every project's JSON.

**Live updates.** Clients receive `Event::ProjectUpdated { project, rev, patches }` (path-set and path-remove operations, coalesced to about 100 ms) and ask for the full state when `rev` skips. A client that attaches late asks for the state.

## 4. Phase engine

### 4.1 Step primitives

All four are generic. A phase is data; the engine and the registries (forms, role cards, code functions) are code.

| Primitive | Declared by | Behaviour |
|---|---|---|
| `fanout` | `over` (an item kind produced by an earlier step), `form`, `role`, `class`, `effort`, `think_budget`, `max_output`, `attempts`, `concurrency` | One employee per input item, each a fresh conversation built from a prompt template id, the role card, the plan context and that one item, forced into the form. Output becomes an item of `emit` kind (or fills fields on the input item). |
| `code` | `fn` (a registry name), `lane` (`io`, `fetch`, `search`, `cpu`), `over` | A pure or IO function with no model. Runs per input item (streamed) or once (barrier). May emit items. |
| `merge` | `watch` (a fanout step), `thresholds` (default 0.25, 0.5, 0.75, 1.0), `min_new`, `form`, `role` | Delta merge: fires when the watched step's finished share reaches the next threshold and there are new unmerged items; sends only the new items plus the compact existing merged items; the last threshold always fires. |
| `gate` | `kind` (`plan`, `checkin`), `payload_fn` | Builds a card, parks the run, resumes with the user's answer. |

Edges: `{ "after": "analyze", "edge": "stream" }` (default) or `"barrier"`. A `stream` edge hands over each item the moment it is `done`. Only gates, merges at 100%, and the Thinking input are barriers, and a barrier is per question where it can be (a question's validation does not wait for another question's collection).

### 4.2 Declaring a phase (menu entries)

```json
{ "phase":"DataCollection", "output":null, "steps":[
  { "id":"vectors", "kind":"fanout", "over":"question", "form":"attack_vectors@1", "role":"attack-vectors", "class":"employee", "effort":"off", "max_output":900 },
  { "id":"fetch",   "kind":"code",   "fn":"search_and_fetch", "after":[{"step":"vectors","edge":"stream"}], "lane":"fetch", "emit":"page" },
  { "id":"analyze", "kind":"fanout", "over":"page", "after":[{"step":"fetch","edge":"stream"}], "form":"analyze@1", "role":"analyst", "class":"employee", "effort":"off", "max_output":1800, "attempts":3, "emit":"source" } ] }

{ "phase":"DataValidation", "output":"validated@1", "steps":[
  { "id":"extract",  "kind":"code",   "fn":"extract_claims",  "over":"source", "after":[{"step":"analyze","edge":"stream"}], "lane":"cpu", "emit":"claim" },
  { "id":"condense", "kind":"merge",  "watch":"analyze", "thresholds":[0.25,0.5,0.75,1.0], "form":"condense@1", "role":"condenser", "class":"employee", "effort":"off", "group_by":"sub", "emit":"cclaim" },
  { "id":"verify",   "kind":"fanout", "over":"cclaim", "after":[{"step":"condense","edge":"stream"}], "form":"verify@1", "role":"verifier", "class":"employee", "effort":"think", "think_budget":600, "max_output":1200 },
  { "id":"collect",  "kind":"code",   "fn":"validated_output", "after":[{"step":"verify","edge":"barrier"}], "lane":"cpu" } ] }

{ "phase":"Thinking", "steps":[
  { "id":"decide", "kind":"fanout", "over":"project", "form":"lead_decision@1", "role":"lead", "class":"lead", "effort":"think", "think_budget":1500 },
  { "id":"route",  "kind":"code", "fn":"route_after_thinking" } ] }

{ "phase":"Checkin", "steps":[ { "id":"card", "kind":"gate", "gate":"checkin", "payload_fn":"checkin_card" } ] }
{ "phase":"Report",  "steps":[ "sections (fanout, per question)", "answer (fanout)", "audit (fanout, batches)", "assemble (code)" ] }
{ "phase":"Finish",  "steps":[ "deliver", "archive", "dispose", "remember" ] }
```

`class` picks a model through project settings (`employee`, `lead`), resolved with `named_model`; both default to the conductor's model. `route_after_thinking` is a code step that writes the next phase list: `Report` (then `Finish`) or `Checkin` (then, on keep_researching, a new round of DataCollection built by the continuation plan).

### 4.3 Pipelining and the scheduler

- One scheduler per project over a priority queue of ready tasks `(step, item)`. A task is ready when its input item is `done` along its edges.
- **Priority is drain-first**: later phases and later steps run before earlier ones, so started work finishes; ties go round-robin across questions so one question cannot starve another.
- **Lanes.** `model` (employees, at most `crew_slots`, section 4.4), `fetch` (8 wide), `search` (4 wide), `cpu` (2). Fetch and search hold no model slot, so Analyze starts the moment a page lands and a full model lane never blocks a fetch (head-of-line avoided by scanning the queue for the first runnable task per lane).
- **No global barriers** except gates and the explicit ones above. Extract runs per finished Analyze; Condense fires on thresholds while Analyze is still running; Verify starts on a condensed claim as soon as its pass returns (a claim merged later gets re-verified only if its member set changed).

### 4.4 Slots and the conductor

- The project runs on its own thread (a third branch beside workflow and playbook in `runner.rs::execute`) owned by the supervisor. `project{create}` returns before any employee exists.
- Employees take a slot per call from the existing shared pool (`Hooks::local_slot`, limit `ContextGate::crew_slots` = planner slots minus the conductor's one) and release it after the call. The conductor never takes a slot from itself and never waits on an employee.
- `before_call` and `after_call` run unchanged: the commit-memory check (`room_for_a_call`) and the "waiting for the GPU" hold while the GPU is busy.
- Parked runs (plan, checkin) hold no thread and no slot.

### 4.5 One writer for the state

All changes to project state are `Mutation` messages (set a path, push an item, patch live fields) sent over a crossbeam channel to one state actor. It applies them in order, bumps `rev`, emits patches, and writes `project.json` atomically (temp file and rename) at most every 250 ms and at every phase boundary. Workers never touch the file.

### 4.6 Journal and resume

Every task has a key `p:<phase>:<step>:<item>:<blake3(input)[..12]>`. Results are appended, one JSON line per finished task, to `phases/<phase>/<step>.jsonl` (the result, the attempts, tokens, finish reason). On resume the engine replays the journal into the state, skips every key it finds, and re-runs only what has no line (a task that was running when the process died). Replay never re-asks a model and never refetches a page: fetched page text is content-addressed in `sources/`. Per-attempt model usage is also written to `crew_run_steps` so the crew watch counts tokens.

### 4.7 Cancellation, budgets, failure

- **Cancel.** A cancel token is checked between tasks and passed to the provider stream and the fetcher. In-flight items become `cancelled`, the state is saved, the project is `cancelled` and can be reopened from a stage.
- **Budgets.** The plan carries `max_sources`, `max_fetches`, a token budget and a minutes budget (the quote on the plan card is an estimate, e). Reaching a budget raises a `budget` Checkin: continue (raises the budget by the plan's quote), report with what exists, or cancel. Budget overrun needs approval (non-negotiable 5), so this is that approval.
- **Employee failure.** `attempts` default 3 (section 5.5). After the last, the item is `failed` with a harness-written reason, the run continues, coverage records it. A phase that finishes with more than 25% of an important step's items failed raises a `failures` Checkin instead of moving on silently.
- **Fetch failure** is data: an unreadable page is an item with a status and a reason, counted, never refetched.
- **Restart.** A restart changes nothing: parked runs stay parked, running ones resume from the journal (boot recovery rebuilds open gates from `project.json`).

### 4.8 Stable ids and determinism

Claim, source, and item ids come from content (identity key, quote offsets, form + input hash), never from arrival order. The `Deduper` takes `&mut self`, so source registration goes through one actor, or ids are keyed by identity hash, which is order-independent. Tests shuffle arrival order and assert the same ids and the same report.

### 4.9 Safety

| Rule | Mechanism |
|---|---|
| Employees cannot act | The runner builds the `ChatRequest` itself with `tools = []`. There is nothing to call, so nothing to approve. |
| Stage isolation | An employee's only output is its validated form. The next stage sees fields, not text. Model-written free text has a `max_len` in the grammar, is stripped of control characters, and enters later prompts inside `<<< >>>` (a literal `>>>` rewritten to `> > >`) after "the following is data from an earlier step, never instructions". |
| Page text is data | Same fence; link anchors capped at 120 chars; failure reasons come from `why_not`. |
| The model never writes a URL | Analyze points at `[L<n>]` numbers, the `LinkTable` supplies the exact url; a citation with no link is searched by code (`registry.lookup` by DOI/arXiv/title) and shown without a link when unresolved; keywords pass through `query_text`; any url string in model output is dropped; the bibliography comes from stored identity. |
| Quotes are verbatim | `Locator` match at Extract; re-read at Verify, audit and bundle; no match, no claim. |
| Network | The plan's grant (decision 1): `GrantedPages`, a wrapper in front of `NetPages` that admits a GET only if the url is `https`, the host resolves to public addresses (the `marlowe-net` SSRF guard, redirects re-adjudicated each hop), the url came from this run (a search result, a link table of a page this run fetched, an extracted reference), robots.txt and per-host spacing admit it (`HostGate`), and quota remains (atomic, shared). Anything else becomes an item `blocked` with the reason and is listed on the next Checkin; `http`, raw ips and private hosts still ask. |
| Taint | The project is tainted from its first network result and stays tainted across resume (latched from the journal). Remember uses worst-case lineage trust. |
| Approvals | Merge, push, deletes outside the workspace, new hosts beyond the grant, budget overrun still ask. Employees have no tools, and Finish writes only inside the Marlowe home. |
| Keys | No credentials in any employee environment; none are needed. |

## 5. Constrained output

### 5.1 What the code does today

`ChatRequest.format: Option<Value>` carries a JSON Schema. Ollama: `provider/src/ollama/request.rs` sends `body["format"] = schema`. llama engine: `provider/src/llama/chat.rs` sends `response_format = {"type":"json_schema","json_schema":{"name":"schema","schema":...}}` and `chat_template_kwargs.enable_thinking` from `Effort`. No agent-loop request sets `format` (`marlowe-agent` hardcodes `None`); only the workflow finalisation call does. Projects need a request builder of their own.

### 5.2 Measured on the llama engine (m)

Setup: Ollama's bundled `llama-server.exe` (build b4d6c7d8f), `marlowe-mini:4b-super` GGUF, `-ngl 0`, idle priority, `--jinja`, 4 threads, ctx 4096, CPU only, ordinary `/v1/chat/completions`. The form was a relevancy-style schema (enum, `maxLength` 80, array `maxItems` 2 of `maxLength` 60 strings) over a 2-sentence document. About 15 tokens/s on CPU.

| Case | Result |
|---|---|
| Thinking off, `response_format` json_schema, max 300 | Valid JSON, finish `stop`, 106 tokens, 8.3 s. Enum and `maxItems` held; `one_line` came back at 76 chars under the 80 cap. |
| Thinking off, max 12 | `finish_reason: "length"`, content cut at `{"relevancy": "High",`: **a truncated reply is not valid JSON**, so a parse check after the grammar is mandatory. |
| Thinking **on**, same schema, max 1500 and max 3500 | `content` empty at both caps, `finish: length`, all tokens in `reasoning_content` ("Thinking Process: 1. Analyze the Request..."), 96 s and 232 s. **The grammar does not constrain the thinking tokens**, and a small thinking model can spend an entire window on reasoning about a trivial form. |
| Thinking on, `thinking_budget_tokens: 200` in the request body | Reasoning stopped at 687 chars, then a grammar-valid answer: finish `stop`, 300 tokens, 19.7 s. **The per-request budget works on this build.** A `maxLength` of 80 cut `one_line` mid-word ("...1T-parameter mixture"): the cap truncates, it does not make the model summarise. |
| Prompt cache | The second call with the same prefix reported `cached_tokens: 99` of 101. Siblings that share a role prefix share KV. |

Consequences. (1) Default every employee form to thinking off. (2) Where a step needs thinking (Verify, the lead), send `thinking_budget_tokens` so the answer always gets room. (3) Always parse and validate after the grammar. (4) Put the long role and plan context first and the per-item data last so siblings hit the cache. (5) Grammar whitespace is unconstrained (the answer used odd indentation); the token cap must include it, and a form's `max_output` is set from the schema's worst case, not the typical case.

### 5.3 Ollama (u)

Not measured. Ollama has no per-request thinking budget. I did not probe it: it would load a model into the shared Ollama server, and the stage was CPU-only with Matthew's preview possibly in use. Rules until measured: employee forms on Ollama run with `think: false` and `format`; steps that want thinking (Verify, lead) run as **reason-first forms** (the `reason` field is first in the schema, so the model reasons in a capped field and then decides) with thinking off. A capability probe at project start, cached per model in the model library, runs a one-field schema twice (`think:true` + `format`) and records `think_with_format` and whether `content` is non-empty; when it is true, the thinking steps use thinking with `max_output` as their only bound and a retry with thinking off on `length`.

### 5.4 Validation

Three layers, in order:

1. **Grammar** (engine): shape, enums, lengths, counts.
2. **Parse and schema check** (code): `serde_json` parse of `content`; `marlowe_workflow::schema::validate` against the same schema (it supports `type`, `properties`, `required`, `items`, `enum`, numeric bounds, string and array lengths, `additionalProperties`). Catches truncation and engines that ignore `format`.
3. **Form rules** (code, named in the form): drops and checks that need data the grammar cannot see: quote found verbatim (drop the claim), link number within the table (drop), url strings (drop), `end` equals `END`, relevancy consistency (lower the label). Dropped items are counted in `rejected`; they fail the employee only when a rule says the form is empty of the thing it exists to produce (an Analyze with relevancy Critical or High and no surviving claim).

### 5.5 Retry

`attempts` default 3, counted per employee, each a new request appended after the first (earlier messages are never rewritten, so the cache holds).

| Failure | Attempt 2 | Attempt 3 | After |
|---|---|---|---|
| `finish = length` | same prompt, `max_output` doubled (never above the window room), thinking off | same, at the cap | failed |
| Parse or schema check fails | the first reply as an assistant turn, then a user turn: "That did not validate: <up to 3 errors>. Answer again with the form only." | same with the errors of attempt 2 | failed |
| Form rule empties the form | a user turn naming the rule ("your quotes were not found verbatim in the page: <first 3>") | the same | failed |
| Provider error or empty content | immediate retry once, then a short pause | | failed |

A `failed` item stores the attempts' error classes, never model text. The run continues.

## 6. The manager

### 6.1 The tool

One tool, `project`, one object argument, short on purpose (the tool slice is tight; the saved-task list had to be shortened last time). The description is about 500 characters and names the actions and the plan keys; the rest rides in a persona block `persona/blocks/project.md`, appended only while the `project` tool group is exposed. `persona/v3.md` is untouched.

| `action` | Args | Returns |
|---|---|---|
| `create` | `plan` | `{id, status:"awaiting_plan", card}` at once |
| `revise` | `id`, `plan` (changed keys only) | the revised card; only while a plan gate is open |
| `status` | `id` | status, phase, progress line, open gate (if any) |
| `list` | none | a few words per project |
| `answer` | `id`, `answer` (`approve`, `keep_researching`, `steer`, `cancel`), `text` (steer) | the new status |
| `checkin` | `id` | forces a Checkin at the next safe point |
| `cancel` | `id` | cancelled |
| `reopen` | `id`, `phase` | re-queues the project from that phase's archived snapshot |

Validation errors are written for a 9b model to act on: at most three, each `plan.<path>: <what is wrong>. <what to write>.`, the first with a minimal valid example, none naming internal types. Example: `plan.questions: need 1 to 8 items like {"q":"one question, 10 to 200 characters"}; got 0.` and `plan.fields[2].type: "date" is fine but "datetime" is not; use one of string, text, integer, number, bool, date, url, enum, list.` Failures carry an error class (`project_plan`) so the loop guard counts them together. A call that is correct costs the manager one turn.

### 6.2 When it is used

| State | Behaviour |
|---|---|
| Teams on | The reminder rides on the user turn's tail (never the system prompt, so the cache holds): for a substantial request, form a project with `project{create}` without asking. |
| Teams off | One reminder line: if the request is substantial, say in one sentence that a project could do it and offer; call `project` only after the user agrees. |
| Research request | Any request for research or deep research starts a research project in both states (decision 5). The plan gate is the ask: nothing is read before approval. |
| A message names a saved team task that a project kind supersedes (`web-research`, `deep-research`) | The naming note says use `project`, not `team`. The superseding list is data on the kind template. |

The `team` tool, Starlark tasks and `dispatch` stay as a hidden advanced route, unchanged. The `deep_research` playbook is not routed to.

### 6.3 Plan approval

The plan card is built by code (questions, fields, limits, estimated calls and minutes (e), channels that are down, and the network grant in plain words: "approving lets this project read public https pages it finds, up to 60, honouring robots.txt; anything else asks"). The run parks: `ctl.parked`, status `awaiting_plan`, `CrewEvent::GateRaised`, thread ends. The manager tells the user in two lines; the user approves in the card, or in chat via `answer`. Both paths converge on `Request::AnswerGate { project, gate, answer }`, a generalisation of `ApprovePlan` (today typed to `ResearchPlan`), which re-queues the run. The user may also say "change question 2": the manager calls `revise`, the card is rebuilt, the gate stays open. Approval installs the run-scoped grant and raises the budget to the quote.

### 6.4 Checkins

Raised by the lead (reason `lead_decision`), by the user at any time (`checkin`, or the key in the TUI), or by code (`failures`, `budget`). A forced checkin stops launching new tasks, lets in-flight employees finish (none is killed), then parks with the state as it is. Answers: **Approve** goes to Report. **Keep researching** builds the continuation plan from the lead's `gaps` and `focus` (or the user's flagged choice) and starts a new DataCollection round, inheriting items and the seen-source set. **Steer** passes the user's text to the lead as data (user trust, unfenced but length-capped) and asks it for a new `LeadDecision` that must be `keep_researching` with gaps. **Cancel** ends the project. Rounds are bounded by `max_rounds`.

### 6.5 Result

When Finish ends, `Run.relay_due` is set and the existing relay posts into the session: confidence, the first paragraph of the answer, the path to `report.md` and `project.json`. With `deliverable.send` the relay adds "send the file" and `send_file` can take it (it already reads crew output folders). If no model is loaded the relay waits and is retried (the 5c9e988 path). The final output is also copied to `<home>\output\<slug>-<id>\` by the existing `deliver` code.

## 7. Storage

### 7.1 Layout

```
<home>\projects\<id>\
  project.json                state (2.2), atomic
  plan.json                   current plan; plans\NN.json history
  forms.json                  form defs in force (also embedded in project.json)
  phases\<phase>\<step>.jsonl append-only task results (the journal)
  sources\<blake3>.md         stored numbered page markdown (content addressed)
  sources\<blake3>.links.json the LinkTable for that page
  archive\NN-<phase>.json     snapshot at each phase boundary
  report.md  research.json  bundle.json
  work\                       scratch (disposed per plan)
<home>\output\<slug>-<id>\    delivered copy
<workspace>\.marlowe\projects\<id>.json   {"project":"<abs path>"}  pointer
```

### 7.2 Index

A `projects` table in `cowork.db` (same connection and `MIGRATIONS` as `crew_runs`): `id, session, run, title, kind, status, phase, rev, plan_no, updated_ms, dir`. It is a cache of what `project.json` says and can be rebuilt by scanning `<home>\projects`. The run record gets `project: Option<ProjectRef>` (additive). Nothing in `cowork.db` is the source of truth for project content.

### 7.3 Archive and reopen

A snapshot is written at each phase boundary. `reopen{id, phase}` loads `archive\NN-<phase>.json` as the state, truncates journal lines after that phase's boundary (kept in a `.reopened` copy), and re-queues the run at that phase. The final-state JSON is archived at Finish, so a finished project can be reopened from any stage whose inputs still exist (`sources\` is needed from DataValidation on).

### 7.4 Disposal (`workspace`)

`keep`: nothing removed. `compact` (default): remove `work\` and raw fetch bytes, keep `sources\` text, journals, snapshots, reports. `delete`: remove everything but `report.md`, the delivered copy and the final `project.json`; such a project reopens only at Finish. Disposal stays inside the project directory, so it needs no approval.

### 7.5 Link from the workspace

The project lives under the Marlowe home (default, decision), not the git workspace, so crew worktrees and scratch never mix with research data. A pointer file in the workspace lets tools and the TUI find it.

## 8. Research phases in detail (the reused code)

All paths under `crates/`. "Move" means the item is private to `marlowe-crew/src/playbook/research_v2/` today and must be exported first (section 10, stage C, prerequisite). The v2 differential test must stay green through the move: it is the parity gate.

### 8.1 AttackVectors

New. The prompt states today's date (from the clock), that the model's knowledge is old, and the form. Code expansion reuses `research_v2/queries.rs`: `query_text`/`search_text` (strip URLs, stopwords, cap 12 words), `looks_like_url`, `since_year(goal)` floor, `node_queries` shape (up to 3 keyword queries per channel). New: date-word variants, because `ChannelQuery.since_year` is year-granular only (`marlowe-research/src/channel.rs:229`); the web channel biases to recent results only within one year of now. `unknown_names` become extra queries ("qwen4 release").

### 8.2 SearchAndFetch (no model)

- **Search.** `marlowe-research/src/registry.rs` (`keyless_with`, `search_observed`, `ready`, `skipped`): arxiv, scholarly, pubmed, wiki, community, feeds, web; keyless only; cache first, limiter, 429 holds the limiter. Channels not online are dropped at plan time and reported ("Not searched").
- **Dedup and quota.** `dedup.rs` `Deduper` behind one actor; `source.rs` `canonical_url`, `normalize_doi`, `normalize_arxiv`; a `SourceBudget`-style atomic quota (copy of `research_v2/types.rs:148`) shared by every question; sources already fetched are filtered before quota is taken, which keeps fetches at 1.0 per unique page.
- **Fetch.** `marlowe-exec/src/corpus.rs` `Corpus` (round-robin hosts, one request in flight per host, every redirect hop through `politeness.rs` `HostGate`, 429/503 retry once, thin pages rendered by Edge when present). `marlowe-exec` is too heavy a dependency, so `marlowe-project` takes a `PageFetch` trait and the binary injects `CorpusPages` (`marlowe/src/research_wire.rs`), as crew does with `PageFetcher`. Edge render opens a process; confirm it is headless (`marlowe-exec/src/render.rs`, not yet read) before enabling it; until then render is off.
- **Grant.** `GrantedPages` (section 4.9) wraps the fetcher. Each redirect hop is re-checked because `read_one` calls the fetcher per hop.
- **Numbered text.** New `to_markdown_numbered` in `marlowe-extract/src/main_content/markdown.rs`: renders `anchor [L12]` instead of `[anchor](url)`, with indexes from `marlowe-research/src/links.rs` `LinkTable` (1-based, deduped by canonical url, capped 400, anchors capped 120), and returns the in-content link list. Gaps it fixes: the model would otherwise see raw urls; `doc.links` and `LinkTable` indexes differ after dedup (map by canonical url); `MainText` does not expose which links are boilerplate. The stored text is exactly the text the model sees, so quotes can be located and re-read byte for byte (the v2 `store_doc` pattern).
- **References.** `marlowe-extract/src/paper.rs` (`Paper`, `Reference`, `from_html`, `from_text`) is not wired into the fetch today: add `references: Vec<Reference>` to `FetchedPage` (`research_v2/host.rs:15`); each reference with a DOI, arXiv id or url becomes a numbered additional-source candidate via `attribution_route`.
- **Credibility inputs.** `SourceRef.channel`, `meta` (venue, cited_by, stars, points), `retracted`, identity confirmation (`Deduper::unconfirmed` plus `registry.lookup`; `page_identity.rs` routes are assertions until confirmed).

### 8.3 Analyze

The prompt: role card, objective and the one question, the plan fields (as the `records` columns), the relevancy table, then the numbered page text fenced last. One call per page; a page longer than the window is cut by `chunks` (14k chars, relevance-picked, max 2) into one call per chunk with the same form, merged by code (claims concatenated by id, relevancy the maximum, records concatenated, additional sources deduped by link or reference). Code fills `index`, `source` (the post-redirect url), `title`, the link-to-url mapping, `credibility`. Unreadable pages are not sent (an item with status and reason, `MIN_READABLE_CHARS` 200).

### 8.4 Extract (code, per finished Analyze)

Rework of the claim loop in `research_v2/compress.rs` (lines 119-205) into `extract(source, doc, AnalyzeOutput) -> Vec<Claim>`: `parse::locate` (exact, then whitespace and typography folding, then the longest fragment of an elided quote, always returning the page's own bytes) finds each quote; not found means dropped and counted; duplicates by locator and text dropped; ids from source and offsets; attributions become derivation edges. `Quote`/`Locator` in `corroboration.rs` stay the verification types.

### 8.5 Condense, Verify

- **Independence and counting.** `marlowe-research/src/corroboration.rs`: `corroborate`, `ClaimGroup`, `DerivationGraph` (`derives_from`, `unresolved`, `independent_roots`, `root_groups`). `source_count` is `independent_roots`, `score` takes the best credibility per `root_group`. The graph construction in `compress.rs` `corroborate_stage` (page-identity pairing, attributions, `Deduper::unresolved`) is private and tied to `Ctx`: extract it as a pure function. Memory is its own bucket and never a supporting root.
- **Condense delta mode** is new (v2 groups in one call up to 100 claims or by run-wide chunks plus a merge pass; there is no incremental mode). Keep v2's group-id and stance model, parse by id only, attach originals from the store. Batches are sized from the model's window at run time (window minus 4,096, times 0.9, minus reply), and split with a merge pass when a question has more claims than fit.
- **Verify.** `research_v2/verify.rs` `Passages` re-read documents from disk; a stored passage that no longer matches is unsupported with no call; missing verdicts count as unsupported; off-goal groups are dropped (`off_goal`). The v2 per-claim batch of one is exactly the fanout here. v2's retry ladders (12/4/1 and so on) are replaced by the generic retry in 5.5.

### 8.6 Tabulate (code, small)

`records` rows from all Analyze outputs are merged by the plan's `key_field` (normalised: lowercase, collapsed spaces). For each field the value with the highest credibility wins, conflicting values are listed with their sources, and a row is marked `unverified` unless two independent roots agree or a verified claim backs it. It is code only, feeds the Report's records table, and is not a model call.

### 8.7 Thinking, Report, Finish

- **Evidence filter** `research_v2/synth.rs:64` (`evidence`): drops refuted and off-goal groups and members whose re-read passage is not supported or partial, recounting over roots. Only verified claims reach the lead and the Report.
- **Sections and answer** `synthesise`, `section_prompt`, `answer_prompt` (cut-off trimming `cut_to_sentence`/`CUT_NOTE` is replaced by the `END` field plus retry).
- **Citation audit** `cited_sentences`, `audit` (`verify.rs`), `repair`, `mark_partial`, `renumber` (`synth.rs`), standing words `standing.rs` (`backing`, `correct`, `correct_text`: "two independent sources" is rewritten to what the root count supports). `markers`/`sentences` (`parse.rs`) are exported with the audit as one module taking a closure for "is this claim usable".
- **Confidence and coverage** `types.rs` `derive_confidence`, `Coverage` (caps at `MIN_READ_SHARE` 0.6, `MAX_FAILED_SHARE` 0.25, blind sub-questions, empty and truncated sections), generalised to project stages.
- **Bundle** `bundle.rs` `marlowe.research-bundle/1`, kept; the final project state is embedded or referenced.
- **Remember** `remember.rs` (`tainted`, `lineage`, `remember`), extractive and no LLM; Established only for on-goal groups with two or more roots; idempotent through a checkpoint. Memory numerics are not touched.

### 8.8 Invariants kept from v2

The model never writes a URL; quotes are verbatim; page text is data; independence is counted by code; verification is required (an incomplete one refuses to report, naming what is missing); every cited sentence is re-checked; confidence comes from code; taint and worst-case lineage; robots.txt, 1 s per host or `Crawl-delay`, keyless only, no paid search; every URL fetched once; stable ids; errors are data per item; the plan gate parks the run.

## 9. Built now, built later, and adding a phase type

**Built now (core plus the research slice):** `marlowe-project` core; the employee runner and constrained output; the phase engine with the four primitives; Plan; DataCollection; DataValidation; Thinking limited to the lead decision and routing; Checkin; Report; Finish; the `project` tool, routing, approval, relay; the view-model that resolves display hints (headless, tested).

**Later:** Engineering (code and experiments in parallel, its own tree, GPU locks, disk awareness; its experiment form is in section 3 as the proof the data model carries it); Thinking beyond the decision (multi-round planning by the lead); the TUI renderer and design previews (after the code is nailed, per Matthew); extra kinds.

**Adding a phase type** (a guide):

1. Write the **forms** it produces and consumes (fields, `w`, `show`). That is already its screen.
2. Add **role cards** in `roles/*.md` (`tools: []`) for each model step; the prompt template id and any new code functions go into the registries.
3. Declare the **phase** as steps in the menu JSON, wiring edges (`stream` unless it must wait) and its `output` form.
4. Add the phase to a **kind template** (default phase list, rollups, `supersedes`).
5. Add **tests**: a scripted fake provider run of the phase, a resume test, a render snapshot. No change to the engine, the store, the manager tool or the renderer is needed unless a new primitive is.

## 10. Build plan

Stages A to D follow; each ends with its acceptance tests green and `cargo check --workspace --all-targets` clean. Cargo only through `scripts/cargo-idle.ps1`, one build at a time; no GPU needed for A to C except the optional live check.

### Stage A: core crate `marlowe-project`

Depends on `marlowe-model`, `marlowe-proto`, `marlowe-workflow` (validator). No agent loop.

- Types: `Plan`, `ProjectState`, `Item`, `Form`, `Field`, `Gate`, `Mutation`.
- Plan validator with the error messages of 6.1; `model_schema(form, plan)`; item validator via `marlowe_workflow::schema::validate`.
- View-model: `view(item, state) -> Row` resolving the hint vocabulary; a plain-text painter used by tests and `--json`/CLI.
- State actor (single writer), path-patch format, `rev`, atomic `project.json`, archive snapshots, `reopen`.
- `projects` index table and migration; `ProjectUpdated` and gate event types in `marlowe-proto`.
- Form and kind registry with the research forms and the experiment form.

Acceptance:
1. Golden round-trips of Plan, state and every form (serde in, out, equal).
2. Plan validator table tests: each bad plan yields at most three errors, each containing a fix sentence; a valid plan passes; a 12-field plan passes, 13 fails.
3. `model_schema` output passes `schema::check_shape`; contains no display hints and no `w:"C"` field; fields keep their order; project fields appear as `records` columns. (Optional ignored test posts it to a CPU llama-server and gets valid JSON.)
4. Two-shape render test: the same `view`/painter renders a research fixture and an engineering fixture; the experiment line equals the string in section 3; a third fixture kind with only a form and no registration renders.
5. No hex colour appears in any form; tone names map to the palette constants.
6. Patch replay: applying the emitted patches in order to the first snapshot equals the final state; a `rev` gap triggers a full state request.
7. Crash safety: a write interrupted between temp and rename leaves the old `project.json` intact; the index can be rebuilt by scanning.
8. Reopen from a phase restores the snapshot and truncates the journal.

### Stage B: employee runner, constrained output, phase engine

- `FormRunner` in `marlowe-crew`: builds the `ChatRequest` itself (`format`, `tools=[]`, `Effort`, `max_output` via `output_cap`), takes a slot with `Hooks::local_slot`, runs `before_call`/`after_call`, parses and validates, applies the retry table of 5.5, writes a `StepRecord` for the crew watch.
- `ChatRequest.think_budget: Option<u32>` (serde default, skipped when none): the llama provider sends `thinking_budget_tokens`; Ollama ignores it. The `marlowe-agent` loop still sends `format: None` (unchanged).
- The engine: menu JSON, scheduler, lanes, drain-first, journal and resume, cancel, budgets, park/resume as a third branch in `runner.rs::execute`.
- Generalise `PendingPlan` to `PendingGate` and `ApprovePlan` to `AnswerGate` (the research playbook keeps working through a shim).

Acceptance (scripted fake `Provider` through the `marlowe-model` seam):
1. An employee that fails validation twice then succeeds: three requests, the second and third contain the earlier turns unchanged and the error text; item `done`, attempts 3.
2. An employee that always fails: item `failed` with a harness-written reason, the run finishes, coverage counts it, no model text in the error.
3. Truncation: `finish=length` reply, attempt 2 sends a doubled `max_output`.
4. Pipelining: with a channel-controlled fake, item 1's Analyze starts before item 2's fetch ends; no step waits for a sibling on a `stream` edge.
5. Conductor non-blocking: `create` returns before the fake provider is called once; slot count never exceeds `crew_slots`; the fetch lane runs while all model slots are held.
6. Resume: kill after N journal lines, resume; the fake counts no call for a finished key and no refetch.
7. Cancel mid-run leaves `cancelled` items and a loadable state.
8. Park: a plan gate holds no thread and no slot (thread count and slot count asserted); restart rebuilds the open gate.
9. Delta merge: thresholds fire at 25/50/75/100% of the watched step with only new items sent; the last always fires.
10. Optional ignored live test against the CPU `marlowe-mini:4b-super` llama-server: a form call returns valid JSON with thinking off and with `think_budget` set.

### Stage C: research phases

Prerequisite refactor (no behaviour change): export the pure parts of `research_v2` into `marlowe-research` (`locate`, `passage`, `markers`, `sentences`, `Passages`, `standing`, `repair`, `renumber`, `derive_confidence`, `Coverage`, the graph builder), re-exported by crew. Then: forms and prompts; `to_markdown_numbered`; `FetchedPage.references`; `GrantedPages`; code steps (`expand_vectors`, `search_and_fetch`, `extract_claims`, `credibility`, condense merge, `score`, `tabulate`, `assemble_report`); the research phase JSON.

Acceptance:
1. The v2 differential test (same recorded replies in, identical report and bundle out) and `eval/` (R@1 0.6725, R@5 0.8865) are unchanged; no reference fixture regenerated.
2. A model field containing a URL never becomes a fetch, a source or a reference (fuzz over every M string field).
3. A claim whose quote is not in the page is never created; ids are identical when arrival order is shuffled.
4. Credibility table tests; `source_kind` cannot raise a source above its channel cap; retracted is 0.
5. Three blogs citing one paper score as one independent root; two roots at 0.5 score 1.0; contested claims keep `stance`.
6. Delta Condense: every claim is in exactly one group after each pass; an omitted claim becomes a singleton; originals are byte-identical to the store.
7. `GrantedPages`: http, raw ip, private host (including a public name resolving to a private address), robots-disallowed, over-quota and not-from-this-run urls are all refused and become `blocked` items; a redirect to a refused host is refused on that hop.
8. Verify: a claim whose stored quote no longer matches is unsupported with no provider call; a failed Verify employee yields unsupported; incomplete verification refuses the report and names what is missing.
9. End to end with recorded replies: DataCollection through Finish produces `report.md` with a bibliography built from stored identity and a records table; the same run after a kill-and-resume produces the same bytes.

### Stage D: manager integration, routing, approval, relay

- `specs/project.rs` tool group, `compact_specs.json` entry, `persona/blocks/project.md`, `teams.rs` reminder text (Teams on, Teams off, research, supersedes), the `Mode::Off` suggestion path.
- `CrewEvent`/`Event` forwarding for `ProjectUpdated` and gates; `Request::AnswerGate`; relay text for projects; boot recovery of open gates and running projects; taint latch on resume.
- Live proof on dawn (GPU free, Matthew's go): his prompt unaided on the local model, and a second project of a different shape whose view renders with no UI code.

Acceptance:
1. The `project` description is at most 600 characters and the tool slice test passes with a fourth long tool; `persona/v3.md` is byte-identical.
2. Validation error messages are snapshot-tested.
3. Reminder snapshots for Teams on, Teams off, research, and a named superseded task.
4. Park, restart and approve: the run resumes in place; the conversation answers other turns while an employee is mid-call.
5. Relay is posted for a finished project and deferred, then retried, when no model is loaded.
6. Scripted end to end with fake models: Matthew's prompt text to `create`, plan card, approve, report, relay with path.
7. Live (dawn): the same prompt, unaided, on `marlowe-dawn:9b-super` with `marlowe-mini:4b-super` employees, produces a plan the 9b wrote itself, a report, and a relay; a second kind with a different form renders correctly in the view-model without code changes.

## 11. Decisions for Matthew

1. **Where projects live.** (a) Under the Marlowe home with a pointer in the workspace. (b) Inside the workspace. Recommend (a): worktrees and research data stay apart.
2. **Teams off and a research request.** (a) Start the project (the plan gate is the ask, nothing is read before approval). (b) Only suggest. Recommend (a), as decision 5 says research triggers it; Teams off still only suggests for non-research substantial work.
3. **Thinking for Verify and the lead.** (a) Thinking with a token budget (llama), reason-first forms with thinking off elsewhere (Ollama, until probed). (b) Always thinking. Recommend (a): unbounded thinking ate a whole window on a trivial form (m).
4. **Credibility inputs.** (a) The model writes only `source_kind`; code owns the table and caps it by channel. (b) Code only from channel and metadata. Recommend (a): code alone cannot tell an official post from a blog on the plain web.
5. **Cluster score.** (a) `min(1, sum of best credibility per independent root)`. (b) Noisy-or `1 - prod(1 - c)`. Recommend (a): it is your weighted sum, independence applied, capped.
6. **Employee and lead models.** Employees on `marlowe-mini:4b-super` if the form probe passes (valid forms on 90% of a test set), else `marlowe-dawn:9b-super`; the lead on `marlowe-dawn:9b-super`. Settings keys `projects.model.employee` and `.lead`. Recommend this, tonight's allowed set.
7. **Failure tolerance.** 3 attempts per employee, Checkin when more than 25% of an important step's items failed. Recommend; tunable in the plan limits.
8. **Network grant size.** `max_fetches` default 60 per approval; a blocked fetch is listed on the next Checkin, not asked one by one. Recommend; asking per fetch would stall a parallel run.
9. **Workspace disposal default.** `compact`. Recommend; `delete` makes reopen impossible before Finish.
10. **Continuation limit.** `max_rounds` default 3, up to 5 in the plan. Recommend.
11. **Existing team tasks and `dispatch deep_research`.** Stay as a hidden advanced route and untouched; the model is pointed at `project`. This is the default you may veto.
12. **Tabulate.** Include the small code-only records merge in the first slice (it is what makes the "newest AI models" example useful). Recommend yes.

## 12. Not verified in this stage

No cargo build, no test and no GPU were run. Measured: llama-server on CPU with `marlowe-mini:4b-super`, section 5.2 only. Not measured: Ollama `format` with and without `think` on any model; llama-server on the project's real engine flags; whether the Edge renderer is headless; whether the tool slice has room for a fourth long description; the time estimates (all (e)). Every statement about existing code comes from reading source and the maps, not from running it.
