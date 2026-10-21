---
name: team-task-authoring
description: How to write a team task script (Starlark) for the `team` tool: objective first, then the specialists; the API, the checker's rules, quality patterns, batching, resume and worked examples. Load before writing a script.
---

# Writing a team task

A team task is an objective plus a team of specialist agents that carry it out, written as a
Starlark file (Python-like; no Python runtime). The `team` tool checks the script before anything
runs and hands back every error with its line. It runs in the background as a crew run; its agents
are crew steps in a sandbox, journalled and resumable. Only the user's own words, a saved name, a
skill or Teams being on make you form one; otherwise work yourself, or say what a team would do and
cost and ask.

## Plan before you write

1. **The objective**: one sentence on what the finished work is, and what "done" looks like.
2. **Scout inline first.** List the files, read the manifest, find the real work-list with your own
   tools. Never put a path, name or count in a script that you have not seen.
3. **Then the specialists**: one concrete task each, with everything it needs in its prompt (paths,
   the exact question, the answer's shape). Agents cannot ask questions mid-run.
4. **Scale to the request.** A modest team (under 10 specialists) unless the user or the session's
   size guideline says more; a one-line question is not a team task.
5. **Multi-phase work is several team tasks in sequence** (understand, design, build, review): run
   one, read its result, then write the next. Stay in the loop between them.

## Shape

```python
META = {
    "name": "audit-deps",                       # optional for inline scripts
    "description": "Check each dependency for a newer major version",
    "when_to_use": "The user asks which dependencies are outdated.",   # shown in the index
    "phases": ["scan", "report"],               # optional, groups the progress view
}

def main(args):            # args: the JSON the caller passed (also the global `args`)
    result = agent("List the dependencies in " + args["manifest"] + " that have a newer major version")
    return result          # JSON-like: dicts, lists, strings, numbers, bools, None
```

META must be a pure literal: strings, numbers, True/False/None, lists, dicts. No names, calls,
f-strings or arithmetic. It comes first. The top level only defines constants and functions; agents
start in `main`.

## API

| Call | Returns |
|---|---|
| `agent(prompt, label=, phase=, schema=, model=, effort=, isolation="worktree", agent_type=)` | the agent's final text; with `schema`, a dict matching it; `None` when it failed or was skipped |
| `parallel([f, g, ...])` | a barrier: each function's result in order, once all are done; a function that fails gives `None` |
| `pipeline(items, stage1, stage2, ...)` | no barrier: each item through the stages independently; a stage is `stage(prev, item, index)` (fewer parameters are fine; `prev` is the item for the first stage); a failing stage gives `None` for that item |
| `phase(name)` | enters a phase: later agents are grouped under it |
| `log(...)`, `print(...)` | a line in the progress view and the journal |
| `team(name, args)` | runs a saved team task and returns its result (one level deep; `workflow(...)` still works) |
| `budget.total`, `budget.spent()`, `budget.remaining()` | the session's token target (`+500k`), or `None` when none was set; `agent()` refuses once it is spent |
| `json.encode(x)`, `json.decode(s)` | JSON text |

Limits: 1000 agents per run, 4096 items per `parallel`/`pipeline` call, at most 16 agents at once
(fewer on a local model; the rest queue). One pass of the script runs at most 5 million statements,
comprehension iterations and lambda calls, for at most 120 s, and holds at most 256 MB; `range()`
gives at most 100000 numbers; `*` and `+` build at most 16 MB; a prompt is at most 1 MB; a `log`
line is cut at 4000 characters. The top level runs only once the run starts, never during the check.

## Starlark gotchas (the checker names each one with its line)

| Python habit | Starlark |
|---|---|
| `import json` | none: `json`, `budget`, `args` are already there. A script cannot read files or run commands; agents do, and lists come in through `args` |
| `x is None`, `x is not None` | `x == None`, `x != None` |
| `f"{r['n']} {len(xs)}"` | an f-string holds only a plain name: assign first, or join: `"n=" + str(r["n"])` |
| `"abc" "def"` (neighbouring strings) | `"abc" + "def"`, or one string; use `\n` or triple quotes for several lines |
| `while cond:` / recursion | `for i in range(n):` with `break`; extend a list between rounds |
| `[lambda: agent(f) for f in files]` | bind the variable: `lambda f = f: agent(f)`, or `pipeline(files, lambda prev, f, i: agent(f))` |
| `x *= n`, `x <<= n` | `x = x * n`, `x = x << n` |
| `try`/`except` | a failed or skipped agent returns `None`: test it |
| `isinstance(x, list)` | `type(x) == "list"` |

Also: no clock, randomness, files, network or `load()`: the script must be deterministic, which is
what lets a resume replay it. META is a pure literal (no names, calls, f-strings or arithmetic),
first in the file. Names starting with `_wf_` are reserved. Schemas: the root is
`{"type": "object", ...}` and `required` names only declared properties; the subset understood is
type, properties, required, items, enum, const, minimum/maximum, minLength/maxLength,
minItems/maxItems, additionalProperties, anyOf, oneOf. Use `schema=` for anything the script reads
and keep schemas small.

## Quality patterns

- **Pipeline by default.** Each item moves on as soon as its own step is done; `parallel` per stage
  makes everyone wait for the slowest. Barrier (`parallel`) only when a step needs every earlier
  result (a merge, a ranking, a dedupe).
- **Adversarial verify.** Skeptics try to refute each claim; keep it on a majority. Most false
  findings die here. Skip it for trivial or already-verified results.
- **Perspective-diverse verify.** A different lens per verifier (correctness, "would this really
  happen", security), not the same prompt N times.
- **Judge panel.** Candidates compete; several judges score against one rubric; the script picks by
  score, not by the first answer.
- **Loop until dry.** Rounds of finders told what is already found; stop when a round adds nothing
  new or the budget is low.
- **Completeness critic.** One agent gets the objective and the result and names what is missing;
  run another round on the gaps.
- **No silent caps.** Sampling, truncating or skipping: `log()` what was dropped and why, and say so
  in the result.
- **Wide, then narrow.** Many cheap finders, fewer careful verifiers, one writer.
- **Budget.** `if budget.remaining() != None and budget.remaining() < 20000: break`.

## Large inputs: batch

Hundreds of items: never one agent per item. Chunk the list in `main` (`items[i:i + n]`, size in
`args` so a rerun can change it), pipeline the chunks, each returning a structured list, then merge
and `log()` the count.

## Resume

Rerun a changed script with `resume_from` (the run id): every unchanged agent call before the first
change replays from cache, so put the expensive, stable work first. The script is saved to a file;
edit it and resume rather than starting over.

## Example: review, then verify with skeptics and lenses

```python
META = {"name": "file-review", "description": "Review each file, then verify each issue with skeptics", "phases": ["review", "verify", "summary"]}

ISSUES = {
    "type": "object",
    "properties": {"issues": {"type": "array", "items": {
        "type": "object",
        "properties": {"line": {"type": "integer"}, "claim": {"type": "string"}},
        "required": ["line", "claim"],
    }}},
    "required": ["issues"],
}
VERDICT = {"type": "object", "properties": {"real": {"type": "boolean"}}, "required": ["real"]}
LENSES = ["Would this really happen on a real input?", "Is it already handled elsewhere in the code?", "Is it a real defect rather than style?"]

def verify(path, issue):
    votes = parallel([lambda lens = lens: agent(lens + " Try to refute " + path + ":" + str(issue["line"]) + " " + issue["claim"], phase = "verify", schema = VERDICT) for lens in LENSES])
    real = [v for v in votes if v != None and v["real"]]
    return len(real) * 2 > len(LENSES)

def review(prev, path, i):
    found = agent("Review " + path + " for bugs. Report each with its line.", label = path, phase = "review", schema = ISSUES)
    if found == None:
        return []
    return [{"file": path, "line": x["line"], "claim": x["claim"]} for x in found["issues"] if verify(path, x)]

def main(args):
    per_file = pipeline(args["files"], review)
    issues = []
    for found in per_file:
        if found:
            issues.extend(found)
    phase("summary")
    log(str(len(issues)) + " verified issue(s) in " + str(len(args["files"])) + " file(s)")
    return {"issues": issues}
```

`args["files"]` comes from your own scouting (`glob`, `grep`), never from a guess.

## Example: judge panel

```python
META = {"name": "pick-design", "description": "Draft designs in parallel, score each with a judge panel, return the best", "phases": ["draft", "judge"]}

SCORE = {"type": "object", "properties": {"score": {"type": "integer", "minimum": 1, "maximum": 10}}, "required": ["score"]}

def main(args):
    drafts = parallel([lambda a = a: agent("Design: " + args["goal"] + ". Approach: " + a, phase = "draft") for a in args["angles"]])
    phase("judge")
    best = None
    best_total = -1
    for i in range(len(drafts)):
        if drafts[i] == None:
            log("draft " + str(i + 1) + " failed and was dropped")
            continue
        scores = parallel([lambda j = j, i = i: agent("Score 1-10 for " + args["rubric"] + " (judge " + str(j + 1) + "):\n" + drafts[i], phase = "judge", schema = SCORE) for j in range(3)])
        total = 0
        for s in scores:
            if s != None:
                total = total + s["score"]
        if total > best_total:
            best_total = total
            best = drafts[i]
    return {"best": best, "total": best_total}
```

## Example: batch a large list

```python
META = {"name": "classify-all", "description": "Classify a long list in batches and merge", "phases": ["classify"]}

LABELS = {
    "type": "object",
    "properties": {"labels": {"type": "array", "items": {
        "type": "object",
        "properties": {"item": {"type": "string"}, "label": {"type": "string"}},
        "required": ["item", "label"],
    }}},
    "required": ["labels"],
}

def main(args):
    items = args["items"]
    size = args.get("batch", 25)
    batches = [items[i:i + size] for i in range(0, len(items), size)]
    done = pipeline(batches, lambda prev, b, i: agent("Label each item as " + args["question"] + ". Items:\n" + json.encode(b), label = "batch " + str(i + 1), phase = "classify", schema = LABELS))
    merged = []
    for d in done:
        if d == None:
            log("a batch failed; its items are missing from the result")
        else:
            merged.extend(d["labels"])
    log(str(len(merged)) + " of " + str(len(items)) + " items labelled")
    return {"labels": merged}
```

## Example: loop until dry, then a completeness critic

```python
META = {"name": "find-all", "description": "Keep finding instances of a pattern until a round finds nothing new, then check for gaps", "phases": ["find", "critic"]}

FOUND = {
    "type": "object",
    "properties": {"items": {"type": "array", "items": {"type": "string"}}},
    "required": ["items"],
}
GAPS = {"type": "object", "properties": {"gaps": {"type": "array", "items": {"type": "string"}}}, "required": ["gaps"]}

def main(args):
    seen = []
    for round in range(args.get("max_rounds", 4)):
        if budget.remaining() != None and budget.remaining() < 20000:
            log("stopping early: the token budget is nearly spent")
            break
        phase("find")
        fresh = agent(
            "In " + args["where"] + ", list every " + args["what"] + ". Already found, do not repeat: " + json.encode(seen),
            label = "round " + str(round + 1), schema = FOUND,
        )
        new = [x for x in (fresh["items"] if fresh != None else []) if x not in seen]
        if not new:
            break
        seen.extend(new)
    phase("critic")
    critic = agent("Objective: every " + args["what"] + " in " + args["where"] + ". Found: " + json.encode(seen) + ". What is missing?", schema = GAPS)
    gaps = critic["gaps"] if critic != None else []
    return {"found": seen, "gaps": gaps}
```

## Running

- `team{script: "...", args: {...}}` (inline; saved under the run's directory),
  `team{path: "..."}`, `team{name: "code-review", args: {...}}`.
- `validate_only: true` checks without running. `background: false` waits for the result.
- Saved team tasks: `.marlowe/workflows/<name>.star` (project), `<home>/workflows/<name>.star`
  (user), plugins' `workflows/`, and the built-ins `code-review` and `simplify`.
- A Claude Code `.claude/workflows/<name>.js` is converted once with `/team convert <name>`.
- The call returns at once with a task id; the result arrives as a notice; `/teams` shows it live.
