# Projects: design

Status: **Stages A and B built** (the core crate `marlowe-project` and its phase engine, `marlowe_project::engine`; section 10); stages C and D are design. Supersedes [deep-research-teams-design.md](deep-research-teams-design.md) as the plan of record; that file stays as the source of the research invariants and the v2 stage map. Tags: (m) measured, (e) estimate, (u) unmeasured.

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
  "objective":"Find the newest and upcoming Kimi, DeepSeek and Qwen models (including Qwen4) and say what is confirmed and what is rumoured.",
  "questions":[
    "Which Kimi models (Moonshot AI) are newest or upcoming?",
    "Which DeepSeek models are newest or upcoming?",
    "Is Qwen4 released or announced, and what is known about it?"],
  "fields":[
    {"key":"model_name"}, {"key":"vendor"},
    {"key":"status","values":["released","announced","rumoured","unknown"]},
    {"key":"release_date"}, {"key":"size"}],
  "send":true}}
```

2. **Plan phase (code).** Validates and expands the plan (title from the objective, `kind: research`, question ids `q1..q3`, field labels from keys, `status` typed enum because it has `values`, the first field as the record key, defaults for limits, phases, workspace and deliverable), creates the project directory, builds the plan card (3 questions, up to 40 sources, 5 fields, estimated model calls, the network grant in plain words) and parks. The tool returns `{"id":"p_7f3a91c2","status":"awaiting_plan"}`. The manager tells Matthew in two lines that a plan is ready; he approves in the card or says "go" and the manager calls `project{action:"answer",answer:"approve"}`. Approving also grants the run its public-https reads (decision 1).
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
| **Plan** | The manager's JSON: objective and questions (required), optional fields, deliverable, limits, workspace disposal. Small enough for a 9b model to fill (section 2.1 counts the keys). Always version-numbered (`plan_no`). |
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

Two required keys. Everything else is optional and defaulted by code, so the smallest valid plan is an objective and a list of question strings.

```json
{
  "objective": "string, 20-500 chars, REQUIRED",
  "questions": [ "string, 10-200 chars each, 1-8 items, REQUIRED" ],
  "fields":    [ {"key":"snake_case<=32", "type":"string (default)|text|integer|number|bool|date|url|list", "values":["makes it an enum, 2-8 strings"]} ],
  "send":      false,
  "title": "optional, 3-80 chars",  "kind": "optional, research",  "workspace": "optional: keep|compact|delete",
  "deliverable": "optional: report (default)|answer|files",  "phases": ["optional, from the menu"],
  "limits": { "max_sources":40, "max_fetches":60, "vectors_per_question":5, "max_rounds":3 }
}
```

**What a 9b model has to write** (count): for a plain question set, 2 keys plus N strings. Each project-specific field costs one key (`key`); `type` is needed only when it is not a string, `values` only for an enum. Matthew's models example is 2 keys, 3 question strings, 5 field objects (6 keys) and `send`: 9 keys and 8 strings, about 250 tokens. A model that also writes `title`, `kind`, `workspace`, `deliverable`, `phases` or `limits` is accepted, but nothing requires it and the tool description does not advertise them (they ride in the persona block). `{"q": "..."}` question objects and `label` / `key_field` on a field are tolerated and ignored beyond that.

Code derives: `title` (first 60 chars of the objective), `kind` (`research`), `deliverable` (`report`), `workspace` (`compact`), `phases` (from the kind template), `limits`, field `label` (the key with spaces, capitalised), field `max_len` (string 120, text 400), `key_field` (the first field), `plan_no`, question ids, `created_ms`, the model quote (calls and tokens, e) and the grant text. Unknown keys are rejected with the list of known ones (a 9b model must not invent keys that silently do nothing). A continuation plan adds `parent_plan`, `inherit` (item ids carried), `seed_sources` (additional-source items to fetch first), `steer` (user text, data).

Rules. `fields` 0-12 (13 is an error); unknown `phases` names are rejected with the menu; `max_fetches` at least `max_sources`; `max_sources` 1-120; `max_rounds` 1-5. Everything the manager writes (objective, questions, keys, enum values) is length-capped; it reaches employee prompts fenced as data, but it is user-directed text, not untrusted web content.

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
  "fields": [ { "key":"...", "label":"...", "type":"...", "w":"M|C|P", "required":true, "from":"data.<key> (default) | live.<key> | agents",
                "max_len":140, "max_items":8, "min":0, "max":1,
                "values":[ {"v":"Critical","def":"...","weight":1.0,"tone":"bad","glyph":"!"} ],
                "of":"string", "columns":[ { "...field..." } ],
                "show": { "as":"badge" } } ],
  "rules": [ "named code checks run after the grammar, see 5.4" ] }
```

`from` says where `view` reads the value: `data.<key>` by default, `live.<key>` for code-filled live slots (cpu, gpu, size, runtime), or `agents` for the item's assigned agents. A `w:"C"` field with `from: live.*` is never stored in `data` and never reaches `model_schema`.

Field types: `string`, `text` (multi-line), `integer`, `number`, `bool`, `enum`, `url`, `timestamp`, `duration` (ms), `list` (`of` a scalar type), `table` (list of objects, `columns` are fields), `agents` (list of agent ids), `json` (anything, code-written only).

**Display hints (`show.as`)**, the whole vocabulary:

| Hint | Applies to | Renders as | Options |
|---|---|---|---|
| `text` | any scalar | plain text, wrapped, clipped with `...` | `lines` |
| `badge` | enum, bool | glyph plus colour from the value's `tone` and `glyph` | none (values carry them) |
| `link` | url | clickable `label ↗`; activating it asks the host to open the system browser (`Action::OpenUrl`, only `https`/`http` urls, the host name shown first); the raw url is one key away | `label_from` (field key) |
| `table` | table, list of objects | table with the schema-named `columns` | `columns` (keys, in order), `sort` |
| `progress` | number 0..1, or object `{done,total}` | `n/m` and a bar | `unit` |
| `meter` | number | bar against a range with warn and crit colouring; with `max: null` the value is shown without a bar (a size) | `min`, `max`, `unit`, `warn_at`, `crit_at` |
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
| `additional_sources` | M | table, max_items 8 | `link` (M, integer: a `[L<n>]` number on the page, or null), `reference` (M, string, max_len 160: a citation or name when there is no link, or null), `opportunity` (M, same labels as relevancy), `notes` (M, max_len 160, how it was referenced); `index` (C, 1-based within the source) and `source` (C, url) filled by code |
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
3. Moonshot AI releases Kimi K2.6   done   [High] ●   cred ▰▰▰▰▰▰▱▱ 0.72   [official]   runtime 00:00:41
  Kimi K2.6 released 2026-10-20, 1T MoE, 256k context
  Moonshot AI releases Kimi K2.6 ↗ (example.org)
  Official post.
  claims (2)
    K2.6 is out
    1T MoE
  additional sources (2)
    opportunity | notes | source
    [Critical] ! | release post | example.org ↗
  records (1)
    Model name | Vendor | Status | ...
  ◆ a_17 analyst
```

(This is the plain-text painter's output in `view.rs`, from a test. Painter rules, settled in code: the title line is the filled `item_label`, then the status word unless the label already shows `{status}`, then the *inline* cells in field order (badge, meter, progress, duration, time) as `label value`, bars 8 cells wide, the bar before a meter's value and after a progress count; *block* cells (text, link, table, json) go under it; the agent chips are always the last line and come from `item.agents` whether or not the form lists an `agent_ref` field; children nest two spaces in.)

The `relevancy` field has `show: badge` (values carry tone and glyph), `source` has `link`, `credibility` has `meter`, `records` has `table` (columns from the plan's fields), `additional_sources` has `table`, `agents` has `agent_ref`. The renderer did not know it was a source.

**An engineering experiment, rendered from its own schema with no UI code.** The Engineering phase (later) declares the item form below (shipped as `registry/forms/experiment.json`; `name` is `hidden` because the label already carries it, which is what makes the painted line equal the one below, pinned by a test). Nothing else is needed for it to show:

```json
{ "form":"experiment","version":1,"title":"Experiment",
  "item_label":"Experiment #{data.n} ({data.name}) [{status}]",
  "fields":[
   {"key":"n","type":"integer","w":"C","show":{"as":"hidden"}},
   {"key":"name","label":"Name","type":"string","w":"P","max_len":80,"show":{"as":"hidden"}},
   {"key":"overview","label":"Overview","type":"text","w":"M","max_len":600,"show":{"as":"text","lines":4}},
   {"key":"size_bytes","label":"size","type":"integer","w":"C","from":"live.size_bytes","show":{"as":"meter","unit":"bytes","min":0,"max":null}},
   {"key":"cpu_pct","label":"cpu","type":"number","w":"C","from":"live.cpu_pct","show":{"as":"meter","unit":"%","min":0,"max":100,"warn_at":85}},
   {"key":"gpu_pct","label":"gpu","type":"number","w":"C","from":"live.gpu_pct","show":{"as":"meter","unit":"%","min":0,"max":100,"warn_at":90}},
   {"key":"runtime_ms","label":"runtime","type":"duration","w":"C","from":"live.runtime_ms","show":{"as":"duration"}},
   {"key":"agents","label":"agents","type":"agents","w":"C","from":"agents","show":{"as":"agent_ref"}}]}
```

Rendered:

```
Experiment #1 (Trellis Encoder Compression) [running]   size 1.2 GB   cpu ▰▰▰▱▱▱▱▱ 41%   gpu ▰▰▰▰▰▰▱▱ 78%   runtime 00:42:10
  Compresses the trellis encoder activations 4x with a learned codebook; checks loss on the held-out split every 500 steps.
  ◆ a_31 engineer   ◆ a_32 monitor
```

The `[running]` badge is the item's status; `{...}` in `item_label` is filled from `data`. The live slots are code-filled from `marlowe-os` process info and the GPU probe the daemon already has; the runtime counts from `live.started_ms`. A kind the manager invents mid-project, or fields it adds to the plan, render the same way because the renderer only reads the form.

**Overview tab.** The kind template lists rollups, e.g. `{"title":"Sources","over":{"kind":"source"},"by":"data.relevancy","show":"badge"}`, `{"title":"Claims","over":{"kind":"cclaim"},"by":"data.verdict"}`, `{"title":"Agents","over":"agents","by":"status"}`. The overview is counts and bars computed from every project's JSON. A kind with no `rollups` falls back to counting its items by kind and status, so an unregistered kind still has an overview. Text from pages (titles, anchors) has control and escape characters stripped when it is stored, so a hostile page cannot drive the terminal.

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

Edges: `{ "after": "analyze", "edge": "stream" }` (default) or `"barrier"`; a barrier may add `"per":"sub"` to wait per group (per question) instead of for everything. A `stream` edge hands over each item the moment it is `done`. Only gates, merges at 100%, and the Thinking input are barriers, and a barrier carries `per:"sub"` where it can (a question's validation does not wait for another question's collection).

### 4.2 Declaring a phase (menu entries)

*Built (stage B).* The menu is `crates/marlowe-project/registry/phases/*.json`, one file per phase type (`plan`, `data_collection`, `data_validation`, `thinking`, `checkin`, `report`, `finish`), loaded into `Registry::menu()`. `spec::validate_menu` checks the whole menu in a test: step ids are unique across the menu (so the two gates are `plan_card` and `checkin_card`, not two `card`s), every `after` and `watch` names a step declared earlier, there is no cycle, every form is registered, an employee step has a role and a form, a thinking step has a `think_budget`, a merge watches a fan-out that is upstream of it, a gate names its card. A step has these keys:

| Key | Meaning |
|---|---|
| `id`, `kind` | the step's name, and `fanout`, `code`, `merge` or `gate` |
| `over` | the item kind it runs over; `project` or nothing means one task for the whole project |
| `scope` | for a step with no upstream step: every item of the kind (`all`, default) or only this round's (`round`: a continuation's new questions) |
| `after` | upstream steps: a name, or `{step, edge: stream\|barrier, per: "sub"}` |
| `form`, `role`, `prompt`, `class`, `effort`, `think_budget`, `max_output`, `attempts` | the employee: its form, role card, prompt builder (defaults to the role), `employee` or `lead` model, `off` or `think`, the reasoning bound, the answer bound, tries |
| `emit` | the kind of item the step makes. A fan-out with `emit` makes one item per input (id derived from step and input, filled with the form's data, shown `running` while the employee works); without it the data fills the input item |
| `fn` | a code step's function; for a fan-out or merge, its *finish* function (the form's rules and the code-filled fields) |
| `lane`, `concurrency` | a code step's lane (`cpu` default, `io`, `fetch`, `search`) and a cap on its tasks at once; a fan-out or merge is always on the `model` lane |
| `watch`, `thresholds`, `min_new`, `group_by` | a merge: the fan-out that sets the pace, the shares it fires at, the fewest new items worth a non-final pass, the group (`sub`: per question) |
| `gate`, `payload_fn`, `answer_fn` | a gate: its kind, the function that builds the card, the one that acts on the answer |
| `important` | more than `failure_share` of this step's items failing raises a `failures` checkin |

DataCollection and DataValidation as shipped (the files are the source of truth):

```json
{ "phase":"DataCollection", "steps":[
  { "id":"vectors", "kind":"fanout", "over":"question", "scope":"round", "form":"attack_vectors@1", "role":"attack-vectors", "effort":"off", "max_output":900, "emit":"vector" },
  { "id":"fetch",   "kind":"code", "fn":"search_and_fetch", "over":"vector", "after":[{"step":"vectors","edge":"stream"}], "lane":"fetch", "emit":"page" },
  { "id":"analyze", "kind":"fanout", "over":"page", "after":[{"step":"fetch","edge":"stream"}], "form":"analyze@1", "role":"analyst", "effort":"off", "max_output":1800, "attempts":3, "emit":"source", "fn":"analyze_finish", "important":true } ] }

{ "phase":"DataValidation", "output":"validated@1", "steps":[
  { "id":"extract",  "kind":"code", "fn":"extract_claims", "over":"source", "after":[{"step":"analyze","edge":"stream"}], "lane":"cpu", "emit":"claim" },
  { "id":"condense", "kind":"merge", "over":"claim", "after":[{"step":"extract","edge":"stream"}], "watch":"analyze", "thresholds":[0.25,0.5,0.75,1.0], "group_by":"sub",
                      "form":"condense@1", "role":"condenser", "max_output":1500, "emit":"cclaim", "fn":"condense_apply" },
  { "id":"verify",   "kind":"fanout", "over":"cclaim", "after":[{"step":"condense","edge":"stream"}], "form":"verify@1", "role":"verifier",
                      "effort":"think", "think_budget":600, "max_output":1200, "fn":"verify_finish", "important":true },
  { "id":"collect",  "kind":"code", "fn":"validated_output", "after":[{"step":"verify","edge":"barrier"}], "lane":"cpu" } ] }
```

Plan is `expand` then `plan_card` (a gate); Thinking is `decide` (a lead fan-out over the project, making a `decision` item) then `route`; Checkin is the gate `checkin_card`; Report is `sections`, `answer`, `cite`, `audit`, `assemble`; Finish is `deliver`, `archive`, `dispose`, `remember`. Differences from the first sketch: `vectors` makes `vector` items (so `fetch` runs per vector), `condense` names the item kind it merges (`over: claim`) and where its input comes from (`after: extract`) as well as the step that sets its pace (`watch: analyze`), and the gates carry an `answer_fn`.

**Dynamic phase list.** A continuation and a checkin add phase instances with `Effect::InsertPhase` (`Mutation::InsertPhase`: it goes in ahead of the Report that waits, so the order stays the order of running). Ids carry the round (`ph_dc_2`). The Plan gate of a continuation round is answered by the engine (`approve`, journaled) because the user has just chosen to continue. `class` picks a model through the engine config (`employee_model`, `lead_model`, both default to the conductor's model); stage D resolves them from settings with `named_model`.

### 4.3 Pipelining and the scheduler

*Built.* `Engine::run` (`engine/core.rs`) is **level-triggered**: every round it reads a small index of the state (built on the state actor's thread, nothing cloned), works out which items are ready for which step, queues the tasks it has not queued, replays those the journal holds, and launches the best queued task of each lane with room. Because a round starts from the state and the journal and not from remembered events, a fresh start, a resume after a crash and the run after a user's answer are one code path.

- **Phases overlap where the data does.** A pending phase becomes active when everything before it is done, or earlier when every unfinished phase before it is one it streams from and that is running (DataValidation extracts the moment the first source is analysed; Thinking, with no cross-phase `after`, waits for DataValidation to finish). A step is *closed* when its upstream steps are closed and none of its tasks is queued or running; a phase is done when its steps are closed. A step may close only on a fresh index (a replay in the same round could have put the very items it waits for).
- **Ready.** A task over an item is ready when the item is `done` and was made by an `after` step in the same round (or, with no `after`, matches `scope`); a `barrier` edge also needs the upstream step closed, or with `per: sub` only that question's group (no queued or running task of the upstream steps for it). A task over the project needs all its `after` steps closed.
- **Priority is drain-first**: later phase, then later step, then the question least recently served, then the oldest. Ties go round-robin across questions (test: six pages land at once on one slot, the questions take turns). The best *runnable* task is chosen, so a full model lane never blocks a fetch (head-of-line).
- **Lanes.** `model:<model>` per model (width: `ModelGate::width`, the crew's slots), `fetch` 8, `search` 4, `cpu` 2, `io` 2 (`EngineConfig::lanes`). Fetch holds no model slot. Each task runs on a worker thread; the loop only reads, queues and launches.
- **Delta merge.** A merge pass per group (question) fires when the *watched* step's finished share reaches the next threshold and the group has at least `min_new` unconsumed items; the final pass fires when everything upstream of the merge is idle for the group and takes what is left. The share is finished / (the step's tasks so far + tasks still queued or running upstream of it), an estimate (a page not yet fetched will become one more analysis). One pass per crossed threshold, never one per finished employee; one pass in flight per group; a pass's input is only the claims no earlier pass took, plus the items earlier passes made (`TaskInput.existing`, offered compactly by the prompt builder). A pass the employee could not make still reaches its finish function with `None`, which may build a degraded result (singletons), so no claim is lost.
- **A project that cannot advance** (a misdeclared menu, a gate nothing answers) ends `Failed` with the waiting phases and open steps named, instead of spinning.

### 4.4 Slots and the conductor

*Built.* The engine runs on its own thread (`Engine::spawn` returns at once; test: the call returns in under 200 ms with the model not yet called). Employees take a slot per call through `ModelGate`: `enter(model, cancel)` blocks (honouring cancel) and returns a `Permit` that frees the slot on `finish(usage)` or drop. `SlotPool` is a self-contained pool (tests, headless runs); `marlowe_crew::project_gate::ProjectGate` is the crew's: it calls the run's `StepHooks::before_call` and `after_call`, so a project's calls share the one pool across every run (`crew_slots`, the conductor's slot left alone), the commit-memory check, the "waiting for the GPU" hold and the run's budget. The conductor never takes a slot from itself and never waits on an employee. A gate that refuses (out of memory) fails the employee with its own words; the model is not called. Parked runs hold no thread and no slot (test: the thread has ended and the pool is empty). *Not yet:* the third branch in `runner.rs::execute` and `Run.project` that start an engine from a crew run (stage D, with the `project` tool).

### 4.5 One writer for the state

All changes to project state are `Mutation` messages sent over a crossbeam channel to one state actor. It applies them in order, bumps `rev`, emits patches, and writes `project.json` atomically (temp file and rename) at most every 250 ms and at every phase boundary. Workers never touch the file. Stage B adds `ProjectHandle::read(f)`: `f` runs on the actor's thread against the current state (nothing cloned) and a panic in it is an error, not the end of the writer.

### 4.6 Journal and resume

*Built.* Every task has a key `p:<phase>:<step>:<item>:<hash12>` (`record::task_key`: the hash covers the item id, which derives from content, the step's form and function, and the phase's round; a merge pass hashes its member ids, `merge_key`; an answered gate has `answer_key`). When a task finishes, the coordinator appends one `Record` line to `phases/<phase>/<step>.jsonl` (`_gates/answers.jsonl` for gates the engine raised itself) **before** the result touches the state. A record holds everything the task changed: the items as they end, the agent, the effects, the phase output, the taint, tokens, and for a merge the members and the threshold. On start the engine loads every journal; an input whose key has a line is *replayed* (the record applied again: every part is idempotent) and never asked or fetched again; only a task with no line runs. A merge's recorded membership is replayed as it was, so a different threshold timing cannot re-run a finished pass. Replay of effects is idempotent by construction (`InsertPhase` skips an existing phase, `NetGrant` only raises the quota, `RaiseBudget` is absolute). On start, items and agents left `running` by a dead process are set back to `queued` and `failed`. Per-attempt usage reaches the crew watch through the permit's `finish(usage)` (the run's spend); `crew_run_steps` rows are stage D.

### 4.7 Cancellation, budgets, failure

*Built.*
- **Cancel.** `Control::cancel` fires the token the providers and gate waits watch; the loop waits for the workers (they return at once), marks every `queued` or `running` item `cancelled`, running agents `failed`, unfinished phases `skipped`, the project `cancelled`, and saves. A cancelled project is terminal until reopened.
- **Budgets.** `EngineConfig::budget` (`tokens`, `minutes`; the plan card's quote in stage D). Before a model task launches the engine checks tokens used (journaled usage, persisted in `metrics`) and active minutes (`metrics.active_ms`, parked time excluded) against the cap times `1 + budget_raises`. Over the cap, nothing new launches, what is in flight finishes, and the project parks on a `budget` checkin: Continue (adds one quote, `RaiseBudget{to}`), Report with what exists, Cancel.
- **Forced checkin.** `Control::request_checkin` stops launching, lets in-flight employees finish (none is killed), then parks on a `user_forced` checkin: Approve (jump to the Report), Keep researching (go on as it was; the tasks that were queued are re-derived), Steer (the text is kept on the plan as data), Cancel. A checkin at Thinking (`lead_decision`) starts a continuation round instead.
- **Jump to report** (`Effect::JumpToReport`): every unfinished phase before the next Report (else Finish) is `skipped`.
- **Employee failure.** After `attempts` the item is `failed` with a harness-written reason (section 5.5), the run goes on. When a phase's steps are all closed and more than `failure_share` (25%) of an `important` step's tasks failed, the project parks on a `failures` checkin (Continue, Report, Cancel), once per phase (the gate records the phase).
- **A function that panics** fails its task (`step ... stopped unexpectedly`); it never leaves the engine waiting.
- **Fetch failure** is data: the fetch function returns a page item with a status; an `Err` fails the task and is counted.
- **Restart.** A restart changes nothing: a parked project parks on the same card with no call, a project that was running resumes from the journal.

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
| What can still travel up | Stage isolation confines a hostile page to one employee's form; it does not make the fields trustworthy. A claim text (240 chars) can reach the lead through Condense and Verify, so the lead's inputs are fenced, its `gaps` and `focus` carry no urls, and every `keep_researching` goes through a Checkin card that shows the proposed questions and flagged sources to the user before anything new is read. Follow-up fetches use only urls from link tables of pages this run fetched, never a string a model composed, so a page can steer a fetch to a public page but cannot encode data into a url. |
| Taint | The project is tainted from its first network result and stays tainted across resume (latched from the journal). Remember uses worst-case lineage trust. |
| Approvals | Merge, push, deletes outside the workspace, new hosts beyond the grant, budget overrun still ask. Employees have no tools, and Finish writes only inside the Marlowe home. |
| Keys | No credentials in any employee environment; none are needed. |

### 4.10 The toolbox: what a phase declaration points at

*Built (`engine/functions.rs`, `engine/builtin.rs`).* A phase names functions; the `Toolbox` holds them. `Toolbox::missing_for(menu)` lists every code function, gate card and finish function the project's phases name that is not registered, and the engine ends `Failed` with that list rather than start. Prompt builders are optional (a step without one gets `default_prompt`: role, objective, today's date, the item fenced as data), so a new phase type runs before anyone tunes its wording.

| Kind | Registered under | Does |
|---|---|---|
| prompt | the step's `prompt`, else `role` | system blocks (role card and plan context, shared by siblings so the cache holds) and the one user message (the item, last, fenced) |
| finish | the step's `fn` on a fan-out or merge | the form's rules and the code-filled fields: model output in, `Finished{data, label, emits, effects, output, rejected, tainted}` out, or a `RuleFailure` the model is told (design 5.5) |
| code | the step's `fn` on a code step | `Fn(&TaskInput, &CancelToken) -> Result<Finished, String>`; `Err` is a harness-written reason |
| payload, answer | a gate's `payload_fn`, `answer_fn` | the card from the state; the `Effect`s of an answer |

`Effect` is the only way a function changes the project beyond its own items: `InsertPhase`, `SetPlan`, `PlanRecord`, `NetGrant`, `PhaseStatus`, `JumpToReport`, `RaiseBudget`. Effects are serialised into the journal record, so they replay. Generic functions ship in `Toolbox::new()`: `plan_expand` (makes the question items a continuation plan adds), `plan_card` (with the network grant in plain words), `plan_answer` (approve records the decision and installs the run-scoped grant, quota = fetches used + the plan's `max_fetches`), `checkin_card`, `checkin_answer` (by reason), `archive`; `builtin::continuation_effects` builds a round (next plan, plan record, the round's phases inserted before the waiting Report). Research-specific functions (`search_and_fetch`, `analyze_finish`, `extract_claims`, `condense_apply`, `verify_finish`, `validated_output`, `route_after_thinking`, `cited_batches`, `assemble_report`, `deliver`, `dispose`, `remember`) are stage C; the stage B tests register fakes under those names. The form `rules` strings (`claim_in_one_group`, ...) name checks a finish function runs.

## 5. Constrained output

### 5.1 What the code does today

`ChatRequest.format: Option<Value>` carries a JSON Schema. Ollama: `provider/src/ollama/request.rs` sends `body["format"] = schema`. llama engine: `provider/src/llama/chat.rs` sends `response_format = {"type":"json_schema","json_schema":{"name":"schema","schema":...}}` and `chat_template_kwargs.enable_thinking` from `Effort`. No agent-loop request sets `format` (`marlowe-agent` hardcodes `None`); only the workflow finalisation call does. Projects need a request builder of their own (built in stage B: section 5.6).

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
2. **Parse and schema check** (code): `serde_json` parse of `content`; `marlowe_contract::schema::validate` (moved there in Stage A; `marlowe_workflow::schema` re-exports it) against the same schema (it supports `type`, `properties`, `required`, `items`, `enum`, numeric bounds, string and array lengths, `additionalProperties`). Catches truncation and engines that ignore `format`.
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

### 5.6 Built (stage B)

`engine/runner.rs::run_employee(provider, gate, cancel, cfg, call, finish, observe)` is the employee.

- **The request** is built by the runner, never by the agent loop: the role and plan context as system blocks (the shared prefix, first), the one item as the single user message (last), `format` = the form's `model_schema`, `tools = []`, `lane = Other`, `max_output_tokens` = the form's bound (plus the reasoning allowance when it thinks), `think_budget`. A fresh context every time.
- **Thinking by engine** (`RunnerConfig.think`, set by the caller because `OllamaProvider` reports `ProviderKind::Ollama` for both of its engines): `Budgeted` (the llama.cpp engine) sends effort medium and `think_budget`, and the output limit includes the reasoning tokens; `WithFormat` (a probe found thinking plus a grammar works on that Ollama model) sends effort medium and no budget, the output limit being the only bound; `ReasonFirst` (the default, Ollama until probed) answers with thinking off and lets the form's own first field, `reason`, carry the reasoning. A step with `effort: think` and no `think_budget` never thinks. The probe itself is stage D.
- **`ChatRequest.think_budget`** is `Option<u32>`, absent from the wire when none; `OllamaProvider::stream_llama` sends it as `thinking_budget_tokens` (`llama::chat::set_think_budget`); Ollama proper and the cloud providers ignore it; the `marlowe-agent` loop still sends none and `format: None`.
- **Validation** is the three layers of 5.4: the grammar, `serde_json` plus `marlowe_contract::schema::validate`, then the finish function's rules.
- **Retry** is the table of 5.5. The length retry resends the *same* messages (a cut reply is never appended) with the limit doubled up to the window room and thinking off; the others append the reply exactly as produced, thinking included, then one user turn, so earlier turns are never rewritten. The first provider error or empty reply is retried at once, later ones after `retry_pause`; an error that cannot be retried (auth, bad request) fails at once.
- **The failure reason is harness-written** from classes and validator pointers: `form did not validate after 3 attempts: /relevancy: must be one of ...`, `the reply was cut off by the output limit on all 3 attempts`, ``form rule `quotes_found` rejected the reply on all 3 attempts``, `the model call failed (network) on 3 attempts`, `the model could not be used: <the gate's words>`. A test feeds an employee an injection string as its answer and asserts it appears nowhere in the stored state.
- **Observation**: the first admitted attempt puts an agent on the screen (listed on the input item and on the emitted item, which shows `running`); every attempt updates the agent's attempt number and tokens.

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

The plan card is built by code from cached channel readiness (no probing inside the tool call, so `create` stays instant; the card refreshes if a probe finishes while it is open) (questions, fields, limits, estimated calls and minutes (e), channels that are down, the search services it will query, and the network grant in plain words: "approving lets this project read public https pages it finds, up to 60, honouring robots.txt; anything else asks"). The run parks: `ctl.parked`, status `awaiting_plan`, `CrewEvent::GateRaised`, thread ends. The manager tells the user in two lines; the user approves in the card, or in chat via `answer`. Both paths converge on `Request::AnswerGate { project, gate, answer }`, a generalisation of `ApprovePlan` (today typed to `ResearchPlan`), which re-queues the run. The user may also say "change question 2": the manager calls `revise`, the card is rebuilt, the gate stays open. Approval installs the run-scoped grant and raises the budget to the quote.

### 6.4 Checkins

Raised by the lead (reason `lead_decision`: always when it says `keep_researching`, and when it says `report` but flagged sources exist), by the user at any time (`checkin`, or the key in the TUI), or by code (`failures`, `budget`). A forced checkin stops launching new tasks, lets in-flight employees finish (none is killed), then parks with the state as it is. Answers: **Approve** goes to Report. **Keep researching** builds the continuation plan from the lead's `gaps` and `focus` (or the user's flagged choice) and starts a new DataCollection round, inheriting items and the seen-source set. **Steer** passes the user's text to the lead as data (user trust, unfenced but length-capped) and asks it for a new `LeadDecision` that must be `keep_researching` with gaps. **Cancel** ends the project. Rounds are bounded by `max_rounds`.

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

A `projects` table in `cowork.db`: `id, session, run, title, kind, status, phase, rev, plan_no, updated_ms, dir`. *Built:* `marlowe-project` owns it (`store::INDEX_SCHEMA`, `CREATE ... IF NOT EXISTS`, on its own WAL connection to the same file, so the crate does not depend on `marlowe-crew`; the crew store may run the same string in its migration list). It is a cache of what `project.json` says and is rebuilt by scanning `<home>\projects` (`rebuild_index`, which also sweeps stale temp files and runs at boot before any writer; a reader's `load` never deletes a writer's temp file). The run record gets `project: Option<ProjectRef>` (additive). Nothing in `cowork.db` is the source of truth for project content.

### 7.3 Archive and reopen

A snapshot is written at each phase boundary. `reopen{id, phase}` loads `archive\NN-<phase>.json` as the state, truncates journal lines after that phase's boundary (kept in a `.reopened` copy), and re-queues the run at that phase. *Built, and settled:* a snapshot is taken **when the labelled phase is about to start** (label `ph_dv` is "the state DataValidation starts from"; `final` at Finish), so reopening at a phase re-runs that phase. The file holds `{snapshot:{no,label,taken_ms,rev,journal:{relpath: bytes}},state}`; the byte marks are what the journals are cut back to. `reopen(id, label-or-NN, now)` returns the snapshot state running again (parked on its card instead if the snapshot had an open gate), with `rev` past every revision seen so clients refetch; the cut journal bytes and the later snapshots move to `.reopened\<now>\`. `final` with no snapshot of that name reopens from `project.json` as it stands (how a `delete`-disposed project reopens at Finish). The final-state JSON is archived at Finish, so a finished project can be reopened from any stage whose inputs still exist (`sources\` is needed from DataValidation on).

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
- **Fetch.** `marlowe-exec/src/corpus.rs` `Corpus` (round-robin hosts, one request in flight per host, every redirect hop through `politeness.rs` `HostGate`, 429/503 retry once, thin pages rendered by Edge when present). `marlowe-exec` is too heavy a dependency, so `marlowe-project` takes a `PageFetch` trait and the binary injects `CorpusPages` (`marlowe/src/research_wire.rs`), as crew does with `PageFetcher`. Edge render is headless (`--headless=new`, scrubbed environment, kill-on-close job object, read in `marlowe-exec/src/render.rs`), so it may render thin pages and never opens a window.
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
- **Remember** `remember.rs` (`tainted`, `lineage`, `remember`), extractive and no LLM; Established only for on-goal groups with two or more roots; idempotent through a checkpoint. Page-derived memories enter with web and agent trust, untrusted lineage and the normal maturation delay; "Established" is a label, not a bypass (a Stage C test asserts no project memory skips maturation). Memory numerics are not touched.

### 8.8 Invariants kept from v2

The model never writes a URL; quotes are verbatim; page text is data; independence is counted by code; verification is required (an incomplete one refuses to report, naming what is missing); every cited sentence is re-checked; confidence comes from code; taint and worst-case lineage; robots.txt, 1 s per host or `Crawl-delay`, keyless only, no paid search; every URL fetched once; stable ids; errors are data per item; the plan gate parks the run.

## 9. Built now, built later, and adding a phase type

**Built now (core plus the research slice):** `marlowe-project` core (stage A); the employee runner and constrained output; the phase engine with the four primitives (stage B, both built); Plan; DataCollection; DataValidation; Thinking limited to the lead decision and routing; Checkin; Report; Finish; the `project` tool, routing, approval, relay; the view-model that resolves display hints (headless, tested).

**Later:** Engineering (code and experiments in parallel, its own tree, GPU locks, disk awareness; its experiment form is in section 3 as the proof the data model carries it); Thinking beyond the decision (multi-round planning by the lead); the TUI renderer and design previews (after the code is nailed, per Matthew); extra kinds.

**Adding a phase type** (a guide):

1. Write the **forms** it produces and consumes (fields, `w`, `show`). That is already its screen.
2. Name what each model step needs: a **role** (and, if the default wording is not good enough, a prompt builder registered in the `Toolbox` under that name), a **finish function** if the form has rules or code-filled fields, and any new **code functions**. A step with none of these runs on `default_prompt` and the model's output as it is.
3. Declare the **phase** as steps in `registry/phases/<name>.json` (one line in `registry.rs` lists the file), wiring edges (`stream` unless it must wait) and its `output` form. `validate_menu` checks it.
4. Add the phase to a **kind template** (default phase list, rollups, `supersedes`).
5. Add **tests**: a scripted fake provider run of the phase, a resume test, a render snapshot. No change to the engine, the store, the manager tool or the renderer is needed unless a new primitive is. Proof: `a_project_of_a_different_shape_runs_and_draws_with_no_code_beyond_its_json` registers a form, two phases and a kind as data, and the engine runs and draws it.

## 10. Build plan

Stages A to D follow; each ends with its acceptance tests green and `cargo check --workspace --all-targets` clean. Cargo only through `scripts/cargo-idle.ps1`, one build at a time; no GPU needed for A to C except the optional live check.

### Stage A: core crate `marlowe-project` (built)

Depends on `marlowe-contract` (the schema validator) and `marlowe-proto` (patch and event types). It does not depend on `marlowe-model` yet; nothing in stage A calls a model. `schema.rs` (pure `serde_json`, `check_shape` and `validate`) moved from `marlowe-workflow` into `marlowe-contract` and is re-exported as `marlowe_workflow::schema`, so no caller changed and the Starlark interpreter stays out of this crate. No agent loop.

What exists, one module each (all public items have rustdoc; `#![warn(missing_docs)]` is on):

| Module | Built |
|---|---|
| `plan` | `parse_plan` (validation with the 6.1 messages, expansion, defaults), `revise_plan` (changed keys over `Plan::to_input()`), `continue_plan` (inherits; ids carry on through `question_offset`; refused at `max_rounds`). |
| `form` | `Form`, `Field`, `Show`, `ShowAs`, `Tone`, `EnumValue`; `Form::validate` (a model field needs its length caps, never a url/timestamp/duration/JSON, a hint must fit its type, no colour value anywhere); `model_schema`, `data_schema`. |
| `registry` | 14 forms (`registry/forms/*.json`: question, attack_vectors, page, analyze, claim, cclaim, condense, verify, lead_decision, section, answer, audit, validated, experiment) and 2 kinds (`registry/kinds/*.json`: `research` available; `engineering` registered with `available: false`, so a plan cannot name it yet). A new kind is a JSON file and one line in `registry.rs`. |
| `state` | `ProjectState` (2.2), `Item`, `Live` (typed slots plus `extra`), `Phase`, `Gate`, `Agent`, `Grants`, `Metrics`; `ProjectState::new` embeds the kind's forms, makes the phases pending and one `question` item per question. |
| `mutation` | 16 mutations; `apply` checks references, validates a `done` item against its form, bumps `rev`, returns patches. |
| `replica` | `apply_patches`, `Replica` (gap detection by `base`). |
| `view` | `view`, `view_tree`, `paint`, `rollups`, `overview`, `paint_overview`. |
| `store` | `ProjectStore`: create/save/load, index (`list`, `row`, `resumable`, `rebuild_index`), journal, content-addressed sources, `archive`, `snapshots`, `reopen`, `link_workspace`, `dispose`. |
| `actor` | `ProjectActor`/`ProjectHandle`: one writer thread per running project, coalesced `ProjectEvent`s, saves. |
| `marlowe-proto` | `ProjectPatch`, `PatchOp`, `ProjectGateOption`; events `ProjectUpdated {project, base, rev, patches}`, `ProjectState`, `ProjectGateRaised`, `ProjectGateAnswered`. |

Settled while building (the design above is amended where it differed):

- **Patches** are computed by diffing the touched parts of the state before and after each mutation (a part is a top-level field, one item, one agent, one form, or one kind's order list), so a mutation cannot forget to report a change. Objects are compared key by key, arrays are replaced whole, and every update ends with `set /rev` and `set /updated_ms`. `ProjectUpdated` carries `base` as well as `rev` because the actor coalesces several revisions into one event; a client applies it only if its copy is at `base`, else it asks for the full state.
- **Actor timing.** Patches are held up to 100 ms, `project.json` is written at most every 250 ms, and always at flush, archive and stop. A gate is different: updates are flushed, then the gate event goes out, then `project.json` is written before the call returns, so a parked run survives a crash with its card.
- **A `done` item is checked.** `PutItem` and `UpdateItem` refuse (state unchanged) a `done` item whose data breaks `data_schema` of its form: unknown key, missing required field, enum outside its values, string or table over its cap. Items that are not `done`, and items whose form is not embedded (a kind nobody registered), are not checked.
- **Forms.** `w` defaults to code; `required` defaults to true for model fields and false otherwise; `nullable` (a column the page may not answer); `columns_from: "plan.fields"` is how `records` takes the plan's fields as columns (a plan field becomes a nullable model column, an enum if it has `values`, text capped at the plan's `max_len`; `date` and `url` plan types become capped strings, because a model never writes a URL: the Stage C rule that drops URL-shaped strings covers them). Enum `values` read as plain strings or `{v, def, weight, tone, glyph}`. A badge on a value with no tone draws `info`. Grammar `required` is every model field (a nullable one may say null).
- **Plan.** Questions accept `{"q": ...}` and a field may be a bare key string; `label` and `key_field` on a field are tolerated. `type: "enum"` with `values` is accepted. A continuation plan's `questions` are the new ones only (ids carry on from the last number); earlier questions stay as items in the state. `max_fetches` defaults to `max(60, max_sources)`; an explicit one below `max_sources` is an error. Control characters in manager text are replaced by spaces and lengths count characters.
- **Rollups.** In a kind template `over` is a string: an item kind, or `agents`.
- **Untrusted text.** Everything drawn that did not come from the harness passes `marlowe_contract::sanitize_*` (an escape becomes a visible `<U+001B>`); a link is `openable` only for `http`/`https` without whitespace or control characters.
- **Not in stage A:** the plan card's cost estimate and grant text (needs channel readiness, stage D), the phase-menu declarations of 4.2 (stage B), `Request::GetProject`/`AnswerGate` (stage D), `ChatRequest.think_budget` (stage B).

Acceptance, as tested (86 unit tests and one end-to-end test in `tests/flow.rs`; none needs a model or a GPU):
1. Golden round-trips: `plan_round_trips_through_json_and_through_its_canonical_input`, `state_round_trips_through_json_equal`, `every_form_round_trips_through_serde_unchanged`, `a_registered_forms_file_equals_its_reserialised_value`, `every_registered_form_is_sound`.
2. Plan validator: `each_bad_plan_gets_at_most_three_errors_each_with_a_fix_sentence` (13 cases), `twelve_fields_pass_and_thirteen_fail`, `eight_questions_pass_and_nine_fail`.
3. `model_schema`: `model_schema_keeps_only_model_fields_in_order_with_their_caps`, `plan_fields_become_the_records_columns_and_nullable_columns_accept_null`, `the_model_never_sees_a_code_field_in_any_research_form`, `analyze_schema_matches_the_design_table`. (The optional CPU llama-server post stays for stage B.)
4. Two-shape render: `a_research_source_draws_from_its_form_with_no_source_specific_code`, `an_engineering_experiment_renders_the_design_line_from_its_own_form` (string equality with section 3), `a_kind_with_only_a_form_and_no_registration_renders`, `an_item_whose_form_is_missing_is_drawn_field_by_field`, `overview_counts_from_the_json_and_falls_back_for_an_unregistered_kind`.
5. Palette: `tones_map_to_the_locked_palette`, `no_form_carries_a_colour_value`, `a_form_with_a_colour_value_is_refused`.
6. Patch replay: `every_mutation_bumps_rev_and_reports_patches_that_replay_to_the_same_state`, `mutations_apply_in_order_and_patches_are_coalesced_until_a_flush`, `a_gap_asks_for_the_full_state_and_leaves_the_copy_alone`.
7. Crash safety: `a_write_that_stops_before_the_rename_leaves_the_old_state_whole`, `the_index_can_be_rebuilt_by_scanning`, `the_journal_appends_lines_and_ignores_a_torn_last_line`.
8. Reopen: `archive_then_reopen_restores_the_snapshot_and_truncates_the_journal`, `a_finished_project_reopens_from_final_even_when_its_archive_was_deleted`.

### Stage B: employee runner, constrained output, phase engine (built)

The engine lives in `marlowe_project::engine`, not in `marlowe-crew` as first sketched: crew depends on project, not the other way, so the engine sees the crew's slots through the `ModelGate` trait and `marlowe_crew::project_gate::ProjectGate` adapts them. `marlowe-project` now depends on `marlowe-model` (serde only).

| Module | Built |
|---|---|
| `spec` | `PhaseSpec`, `StepSpec`, `After`, `Edge`, `Lane`, `Think`, `Scope`; `validate_menu`. Menu in `registry/phases/*.json`, loaded by `Registry`. |
| `engine::runner` | `run_employee`, `EmployeeCall`, `RuleFailure`, `ThinkSupport`, `RunnerConfig`, `Observer`: section 5.6. |
| `engine::gate` | `ModelGate`, `Permit`, `SlotPool`, `Unlimited`. |
| `engine::fence` | `fence`, `clean`: free text from an earlier step enters a prompt as capped, control-stripped data inside `<<< >>>`, a literal `>>>` rewritten. |
| `engine::functions` | `Toolbox`, `TaskInput`, `Finished`, `Effect`, `GateCard`, `default_prompt`: section 4.10. |
| `engine::builtin` | the generic plan and checkin functions, `continuation_effects`. |
| `engine::record` | `Record`, `task_key`, `merge_key`, `answer_key`: section 4.6. |
| `engine` (`core`) | `Engine` (`run`, `spawn`, `answer`, `control`), `Control` (`cancel`, `request_checkin`), `RunOutcome` (`Finished`, `Parked{gate}`, `Cancelled`, `Failed{reason}`), `EngineConfig`, `answer_gate`: sections 4.3 to 4.7. |
| `marlowe-model` | `ChatRequest.think_budget`. |
| `marlowe-provider` | `llama::chat::set_think_budget`, called from `stream_llama`. |
| `marlowe-crew` | `project_gate::ProjectGate` over `StepHooks`; `runner::project_gate` builds one from a run's hooks (unused until stage D calls it). |
| `marlowe-project` state | `Mutation::InsertPhase`, `ProjectHandle::read`, `Gate.phase`/`Gate.step`, `Metrics.active_ms`/`budget_raises`. |

Settled while building (the design above is amended where it differed):

- **The engine returns, it does not wait.** A park is `RunOutcome::Parked`: the thread ends. `Engine::answer` (or `answer_gate(handle, ...)`) records the answer and the next `run` carries on; nothing is remembered between runs except the state and the journal.
- **Gates by step or by engine.** A gate step (`plan_card`, `checkin_card`) parks when reached; forced, budget and failures checkins belong to no step (`Gate.step` empty, `payload.phase` names what they are about) and their answers are handled by `checkin_answer` and journaled under `_gates`. A `cancel` answer cancels the engine. An answer function that refuses leaves the gate answered and the project going on without effects (the record is `failed`, with the reason).
- **Fill or emit.** A fan-out with `emit` writes a new item; without it the data is merged into the input item (Verify fills the claim it checked) and a failure marks that item `failed`. A code step that returns data fills its input item the same way. The data an item gets is the finish function's `data`; with no finish function it is the model's output as it is.
- **An item whose data breaks its form** when the record is applied (the finish function made something the form refuses) is stored `failed` with the validator's first two pointers, never `done`.
- **Parents.** A new item's parent is the question it belongs to (the input's parent, or the input itself when it is a question; a merge's first member's parent), so the tree stays question-rooted however deep the pipeline.
- **Group.** `sub` is the id of the nearest question ancestor; per-question barriers and merges key on it.
- **Agents.** `a_<n>` numbers continue from the highest in the state. The employee's record ends in the journal with its tokens; the agent is listed on its input and its emitted item (a merge's agent on the items it made, not on the claims it read).
- **Budgets and rounds are stored.** `Metrics.active_ms` and `budget_raises` persist, so a parked project neither loses its running time nor its raised budget.
- **The first sketch's `FormRunner` in `marlowe-crew`** and "generalise `PendingPlan` to `PendingGate`, `ApprovePlan` to `AnswerGate`" move to stage D: they exist to start an engine from a crew run and answer from the TUI, and need the `project` tool and `Run.project` to be reachable at all.

Acceptance, as tested (the 10 in `tests/engine.rs` and `tests/engine_flow.rs` are scripted fake `Provider`s through the `marlowe-model` seam; the real research menu runs with fake code steps under the names stage C will fill):
1. *Retry:* `two_invalid_replies_then_a_good_one_is_three_requests_that_keep_the_earlier_turns`, `a_form_rule_that_rejects_sends_its_own_turn_and_the_attempt_after_can_pass`, `a_provider_error_or_an_empty_reply_retries_the_same_request`.
2. *Always fails:* `an_employee_that_always_fails_ends_failed_with_a_reason_that_holds_no_model_text`; at engine level `employees_that_keep_failing_are_recorded_failed_without_model_text_and_the_run_asks_the_user` (four sources `failed` after 3 attempts, a `failures` checkin, then the run finishes), `a_gate_that_refuses_fails_the_employee_with_the_gates_words_and_never_calls_the_model`.
3. *Truncation:* `a_reply_cut_by_the_limit_retries_the_same_request_with_the_limit_doubled_then_capped`.
4. *Pipelining:* `an_analysis_starts_while_another_questions_fetch_is_still_running`; scheduling order: `ties_go_round_robin_across_questions_so_none_is_starved`.
5. *Conductor and lanes:* `spawning_returns_before_the_model_is_called_and_the_slot_limit_holds`, `a_fetch_runs_while_every_model_slot_is_held`; crew side `a_permit_takes_a_crew_slot_and_gives_it_back_booking_the_spend`, `the_pool_is_the_crews_so_a_third_call_waits_for_a_slot`.
6. *Resume:* `a_restart_replays_the_journal_and_never_asks_a_model_or_refetches_a_page` (state at the plan card, journal whole: zero model and fetch calls, same items and data), `a_task_with_no_journal_line_is_the_only_thing_that_runs_again` (the lines of Verify, Thinking, Report and Finish removed: exactly those six plus the lead, sections, answer and audit run).
7. *Cancel:* `cancelling_mid_run_leaves_cancelled_items_and_a_state_that_loads`.
8. *Park:* `the_research_menu_parks_on_its_plan_card_then_runs_to_a_finished_project`, `a_parked_project_survives_a_restart_and_resumes_in_place`.
9. *Delta merge:* `a_delta_merge_fires_at_each_quarter_of_the_watched_step_and_sends_only_what_is_new` (eight writers released a few at a time: no pass at 1 of 8, one each at 2, 4, 6, the last when the rest finish; every claim in exactly one pass).
10. *Live, CPU (ignored test `tests/live_cpu.rs`):* a real llama-server on `marlowe-mini:4b-super`, `-ngl 0`, idle priority: with thinking off the `analyze@1` form came back valid in one attempt (170 prompt and 281 output tokens, 21 s); with `think_budget` 200 the `verify@1` form came back valid in one attempt after 752 characters of reasoning (24 s), and the request body carried `thinking_budget_tokens` and `n_predict` = answer bound plus reasoning allowance. One sample each, on the provider's own body builder; see section 12 for what it did not show.
Beyond the list: `the_research_menu_parks_on_its_plan_card_then_runs_to_a_finished_project` also checks agents on items, the grant, snapshots and the painted screen; budgets (`a_token_budget_stops_new_calls_and_parks_on_a_budget_checkin_that_continue_raises`, `choosing_report_at_a_budget_checkin_skips_ahead_to_the_report`); forced checkins (`a_forced_checkin_lets_in_flight_work_finish_then_parks_and_resumes_on_keep_going`, `approving_a_forced_checkin_writes_the_report_with_what_exists`); the lead's loop (`the_leads_keep_researching_goes_through_a_checkin_and_a_continuation_round`: phases `ph_plan_2` to `ph_think_2` inserted, only the new question researched, the report covers all four questions); `a_project_of_a_different_shape_runs_and_draws_with_no_code_beyond_its_json` (a kind, a form and two phases registered as data run on `Toolbox::new()` and a model that answers from the schema, and draw with badge and agent chip); `a_function_that_panics_fails_its_task_and_the_run_goes_on`; `a_read_runs_against_the_current_state_and_a_panicking_read_leaves_the_actor_alive`.

Not in stage B: the stage C functions; the plan card's cost estimate; cpu/gpu/disk probes feeding `metrics` and `live` (the `metrics` and `live` slots exist; nothing samples yet); the Ollama thinking probe; the `execute` branch, `Run.project`, `AnswerGate` and the events to the TUI (stage D); the model plan from settings.

### Stage C: research phases

Prerequisite refactor (no behaviour change): export the pure parts of `research_v2` into `marlowe-research` (`locate`, `passage`, `markers`, `sentences`, `Passages`, `standing`, `repair`, `renumber`, `derive_confidence`, `Coverage`, the graph builder), re-exported by crew. Then: forms and prompts; `to_markdown_numbered`; `FetchedPage.references`; `GrantedPages`; code steps (`expand_vectors`, `search_and_fetch`, `extract_claims`, `credibility`, condense merge, `score`, `tabulate`, `assemble_report`); the research phase JSON.

Sub-steps: **C1** the export refactor with the differential test as its only gate (acceptance 1); **C2** numbered text, references, `GrantedPages`, search and fetch (2 for links, 7); **C3** Analyze, Extract, credibility, Condense, Verify (2, 3, 4, 5, 6, 8); **C4** Tabulate, Thinking, Report, Finish, remember and the end to end (9, and the maturation test). Each sub-step ends with `cargo check --workspace --all-targets` and its tests.

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

Sub-steps: **D1** tool, spec, persona block, reminders (acceptance 1, 2, 3); **D2** events, `AnswerGate`, relay, boot recovery (4, 5, 6); **D3** the live proof (7).

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
6. **Employee and lead models.** Default: both are the conductor's model (no second model loaded, one KV cache). Opt-in: employees on `marlowe-mini:4b-super` once the form probe passes (valid forms on 90% of a test set) and the GPU has room for both; the lead stays on `marlowe-dawn:9b-super`. Settings keys `projects.model.employee` and `.lead`. A two-model run on one GPU is checked against the GPU-idle gate and VRAM before the live proof.
7. **Failure tolerance.** 3 attempts per employee, Checkin when more than 25% of an important step's items failed. Recommend; tunable in the plan limits.
8. **Network grant size.** `max_fetches` default 60 per approval; a blocked fetch is listed on the next Checkin, not asked one by one. Recommend; asking per fetch would stall a parallel run.
9. **Workspace disposal default.** `compact`. Recommend; `delete` makes reopen impossible before Finish.
10. **Continuation limit.** `max_rounds` default 3, up to 5 in the plan. Recommend.
11. **Existing team tasks and `dispatch deep_research`.** Stay as a hidden advanced route and untouched; the model is pointed at `project`. This is the default you may veto.
12. **Tabulate.** Include the small code-only records merge in the first slice (it is what makes the "newest AI models" example useful). Recommend yes.

## 12. Not verified

**Stage B (built).** `cargo check --workspace --all-targets` is clean (the warnings in `marlowe`, `marlowe-tui` and `marlowe-memory` were there before). Tests that passed: `marlowe-project` 117 unit tests, 1 stage A end to end, 3 engine and 16 scheduler tests (four runs in a row, no flake), `marlowe-crew` all 8 binaries (103 unit tests, 4 of them the `ProjectGate`), `marlowe-model` 37; the two live tests are ignored by default and were run once by hand against a CPU llama-server on `marlowe-mini:4b-super` (nothing on the GPU, Matthew's preview and Ollama untouched, the server I started was stopped).

Not verified:
- **Quality of a 4b employee.** The live Verify sample was *valid* and *wrong*: the passage says 256k, the model answered `refuted` and invented a `corrected_claim` of 32768, and its `reason` slipped into Chinese. The grammar guarantees the shape, not the judgement. Stage C needs finish rules (a `refuted` or a `corrected_claim` must name a passage; a `corrected_claim` only on `partial`) and the 90% probe of decision 6 before a 4b model is trusted with Verify; the lead model may need to be the verifier.
- **Ollama proper.** `format` with and without `think` was not probed; `ThinkSupport::ReasonFirst` is the safe default, not a measurement. Whether `Budgeted` holds for the other models the llama engine serves is one model's worth of evidence.
- **The crew's real pool.** `ProjectGate` was tested against a fake `StepHooks` that behaves like the pool (a counting pool, a refusal, booked usage). It was not run inside a real `Supervisor` run: nothing starts an engine from a crew run until stage D.
- **Scale and time.** The scheduler re-reads an index of every item each round; it was run with tens of items, not thousands. Wall-clock for a real project, and the cost estimate on the plan card, are unmeasured (e).
- **Crash fidelity.** Resume was tested by giving a new process the journal and an older `project.json`; a kill at an arbitrary instant, a torn journal line inside a record and two engines on one project were not tried (the journal reader ignores a torn last line, tested in stage A; nothing stops two engines).
- **Not built:** the stage C functions, the cpu/gpu/disk probes that fill `metrics` and `live`, the Ollama thinking probe, the `project` tool and everything in stage D.
