# Projects: how to extend them

The design and its reasons are in [research/projects-design.md](research/projects-design.md). This page is the working guide: where things live, how to add a form, a phase type or a kind, and how to test it. Nothing here needs a change to the engine, the store, the manager tool or the renderer unless you add a new step primitive.

## What a project is, in one minute

A project is **one JSON document** (`<home>\projects\<id>\project.json`) changed only by mutations through a single writer, so every change bumps `rev` and yields JSON-pointer patches. A project has **phases** (a list from a menu), a phase has **steps** (four primitives), a step runs **tasks** over **items**, and an item is filled by a **form**. The manager writes only the plan; the engine runs the steps; employees (fresh-context, tool-less model calls forced into a form) fill items; code fills everything that is not judgement. The screen is one renderer over the JSON: nothing outside `registry/` and the toolbox knows what a "source" or an "experiment" is.

| You want | You touch |
|---|---|
| A new thing to be collected or produced (a record, a measurement) | a **form** in `crates/marlowe-project/registry/forms/<name>.json` |
| A new stage (a new sequence of steps) | a **phase** in `registry/phases/<name>.json`, plus toolbox functions if a step is code or needs rules |
| A new sort of project | a **kind** in `registry/kinds/<name>.json` |
| A new way for tasks to flow (a fifth primitive) | `spec.rs` and `engine/core.rs`; rare |

The registry is embedded at build time. The lists are in `src/registry.rs` (`FORMS`, `KINDS`, `PHASES`); a file nobody lists is not loaded.

## Adding a form

A form is both the JSON Schema an employee is forced into and the description of how each field is drawn. Fields say who writes them: `"w": "M"` the model (the only fields in the schema it is shown), `"C"` code (the default: a field nobody names never reaches a model), `"P"` the plan.

```json
{
  "form": "measurement",
  "version": 1,
  "title": "Measurement",
  "item_label": "{data.n}. {data.metric} = {data.value} [{status}]",
  "fields": [
    { "key": "n", "type": "integer", "w": "C", "show": { "as": "hidden" } },
    { "key": "metric", "type": "string", "w": "M", "max_len": 80, "required": true },
    { "key": "value", "type": "number", "w": "M", "required": true },
    { "key": "verdict", "type": "enum", "w": "M", "values": [
        { "v": "good", "def": "Within the target.", "tone": "ok", "glyph": "●" },
        { "v": "bad",  "def": "Outside the target.", "tone": "bad", "glyph": "×" } ] },
    { "key": "cpu_pct", "type": "number", "w": "C", "label": "cpu", "from": "live.cpu_pct",
      "show": { "as": "meter", "unit": "%", "min": 0, "max": 100, "warn_at": 85 } }
  ]
}
```

Rules to keep:

- **A model never writes a url, a source, an id, a score or a live number.** Mark them `"w": "C"`; `model_schema` then cannot offer the model a place to put them. Type `url` is code-only.
- **Every free-text field has `max_len`.** It becomes the grammar's cap, and text from one step enters a later prompt only fenced as data.
- **Enum values carry a definition (`def`).** The definitions are shown to the model; a vague label is a source of bad answers, so write what separates one value from the next. A `weight` can score it, a `tone` and `glyph` draw it.
- **Display hints are the vocabulary the renderer knows:** `badge`, `link`, `table` (with `columns`), `progress` (n/m), `meter` (a live number, with `unit`, `min`, `max`, `warn_at`, `crit_at`), `duration`, `time`, `agent_ref`, `text` (with `lines`), `json`, `hidden`. A form carries colour *names* (`ok`, `warn`, `bad`, `info`, `accent`, `dim`), never a colour value; the painter maps them to the locked palette.
- **Live fields** read from `live.*` (`"from": "live.cpu_pct"`), which code fills while the task runs; a field `{ "key": "agents", "type": "agents", "from": "agents" }` draws the agents assigned to the item (chips a client can open).
- **A new version** (`version: 2`) is a new form id (`measurement@2`); a project embeds the forms it uses, so old projects still draw.

Register the file in `FORMS` in `src/registry.rs`. `every_embedded_form_and_kind_parses` and the view tests check it; no form may contain a hex colour.

## Adding a phase type

A phase is a list of steps; the four primitives are:

| `kind` | Does | Needs |
|---|---|---|
| `fanout` | One fresh-context employee per input item, forced into `form` | `over` (the item kind), `form`, `role`, `class` (`employee` or `lead`) |
| `code` | A function, no model | `fn` (a name in the toolbox), optionally `lane` (`cpu`, `io`, `fetch`, `search`) |
| `merge` | A delta merge at fractions of a watched step | `watch`, `thresholds`, `group_by`, `form`, `fn` |
| `gate` | Parks the run for the user | `gate` (`plan` or `checkin`), `payload_fn`, `answer_fn` |

Write `registry/phases/<name>.json`:

```json
{
  "phase": "Measure",
  "output": "measurement@1",
  "steps": [
    { "id": "plan_runs", "kind": "code", "fn": "plan_runs", "over": "question", "lane": "cpu", "emit": "run" },
    { "id": "run", "kind": "code", "fn": "run_experiment", "over": "run",
      "after": [ { "step": "plan_runs", "edge": "stream" } ], "lane": "io", "emit": "measurement" },
    { "id": "judge", "kind": "fanout", "over": "measurement",
      "after": [ { "step": "run", "edge": "stream" } ],
      "form": "measurement@1", "role": "judge", "class": "employee", "effort": "off",
      "max_output": 400, "fn": "judge_finish", "important": true }
  ]
}
```

- Step ids are **unique across the whole menu** (`plan_card` and `checkin_card`, not two `card`s).
- An **edge** is `stream` (an item moves on the moment it is `done`; this is what makes phases overlap) or `barrier` (wait until upstream has nothing left, everywhere or `per` group).
- `effort: "think"` with `think_budget` lets a verifier reason first on an engine that bounds it; elsewhere the form's own `reason` field carries it.
- `important: true` means more than a quarter of its items failing raises a failures checkin instead of letting the project go on with a hole.

Then the toolbox. `engine::Toolbox` holds the named functions; a step without a prompt builder gets `default_prompt` (the item's data as fenced input), and a fan-out without a `fn` keeps the model's output as it is.

```rust
let mut t = Toolbox::new();
t.code("plan_runs", |input, cancel| { /* return Finished { emits: vec![item], .. } */ });
t.prompt("judge", |input| Ok(Prompt::new(system_blocks, user_text)));  // optional
t.finish("judge_finish", |input, map| { /* rules + code-filled fields; Err(RuleFailure) retries */ });
t.payload("my_card", |ctx| GateCard { payload, options });       // for a gate
t.answer("my_answer", |state, gate, answer| Ok(vec![Effect::..]));
```

A **finish function** is where rules live: `Err(RuleFailure)` appends the rule's own words to the next attempt (a model is told what was wrong, once, in words it can act on), and the fields code fills (`source`, `credibility`, `index`) are set here. Strings that came from a model are cleaned with `research::plain` (urls removed, length cut) before they are stored.

Register the file in `PHASES` in `src/registry.rs` (and the phase name in `PHASE_MENU` if a plan may list it). `spec::validate_menu` runs as a test over the whole menu: unique ids, existing edges, existing forms, thinking budgets, merges that watch a real step. `Toolbox::missing_for` names a code step or gate whose function was never registered; the engine refuses to start a project with such a hole, with that sentence.

## Adding a kind

A kind is a template a plan's `kind` names: the default phase list, the forms it embeds, which saved team tasks it supersedes, and the overview rollups.

```json
{
  "kind": "measurements",
  "title": "Measurements",
  "available": true,
  "phases": ["Plan", "Measure", "Report", "Finish"],
  "forms": ["question@1", "measurement@1"],
  "supersedes": [],
  "rollups": [ { "title": "Measurements", "over": "measurement", "by": "status" },
               { "title": "Agents", "over": "agents", "by": "status" } ]
}
```

`available: false` registers a kind that is designed but not built (`engineering`): a plan may not name it. Add the file to `KINDS`. A plan with `"kind": "measurements"` is validated by the same `parse_plan` (an objective, questions, optional fields), so the manager tool needs no change; add the kind's words to `persona/blocks/project.md` only if the manager should choose it unprompted (the block is full: the 32k persona slice is 2,200 tokens).

## Testing a new phase

The pattern every phase test follows, with no network, model or GPU:

1. **A scripted fake provider** (`tests/common/mod.rs`: `Fake::with(|req, n, cancel| Say::Json(..))`). A request's `format` is the schema it was forced into, so a fake can tell the forms apart (`asks_for(req, "verdict")`); `Say::Schema` answers any form with a value that fits.
2. **A project on a throwaway home** (`Project::new`), the real `Engine`, your `Toolbox`, a `SlotPool`. Run it to its first gate, answer, run on, assert the items and the state.
3. **A resume test:** stop partway, build a new engine on the same files, and assert the model was asked nothing it had answered (the journal replays).
4. **A render test:** `view_tree` and `paint` over the finished state; a kind nobody wrote screen code for must still draw. `a_project_of_a_different_shape_runs_and_draws_with_no_code_beyond_its_json` (in `engine_flow.rs`) registers a form, two phases and a kind as data and does all of this; copy it.
5. **For anything that reaches the web,** the recorded world (`tests/world/mod.rs`, `tests/fixtures/research`): channel results, pages and model replies are files, hostile page text included.

Cargo only through `scripts/cargo-idle.ps1`.

## From the conversation to a project

The manager (the user-facing model) calls `manage_project` (`marlowe-exec/src/specs/project.rs`); the tool calls `ProjectService` (`marlowe-project/src/service.rs`); the service starts an engine through its host (`marlowe-daemon/src/project_ops.rs`). To test the manager side of a new kind you need only the service and a fake `ProjectHost` (`tests/service.rs` shows one in forty lines). A client reads and answers with `ListProjects`, `GetProject` and `AnswerProjectGate`; `marlowe project` does the same from a terminal and prints any project as JSON (`show`), as text through the renderer (`view`) or as a rollup (`overview`).

## Invariants to keep

- The conductor never waits: `create` and `answer` return at once; a card parks the project with no thread held.
- Only validated forms travel between stages; free text is capped and fenced as data downstream; an injected string in a page can corrupt one employee's form and nothing above it.
- A model never writes an address, an id, a source or a score.
- A run reads the network only under the grant the user gave by approving the plan: public https the run found, robots.txt honoured, quota counted; everything else asks.
- Journal before state: a task's record is written before it touches the state, so a restart replays instead of re-asking.
- The palette is locked; a form carries tone names.
