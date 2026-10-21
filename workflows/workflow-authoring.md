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

## Rules the checker enforces

- No `while`, no recursion: loop with `for i in range(n):` and `break`.
- No clock, randomness, files, network or `load()`: agents do all the work. The script is
  deterministic, which is what lets a resume replay it.
- No `try`/`except`: a failed agent returns `None`; check for it.
- A lambda or nested `def` inside a loop or comprehension must not read the loop variable, or a
  variable the loop body sets, unbound: write `[lambda f = f: agent(f) for f in files]`,
  `lambda p = p: agent(p)`, or use `pipeline(files, lambda prev, f, i: agent(f))`.
- Write `x = x * n` and `x = x << n`: `*=` and `<<=` are not available. Names starting with `_wf_` are reserved.
- Pass lists to a script through `args`; scouting happens before, with your own tools.
- Schemas: the root is `{"type": "object", ...}` and `required` names only declared properties.
  The subset understood: type, properties, required, items, enum, const, minimum/maximum,
  minLength/maxLength, minItems/maxItems, additionalProperties, anyOf, oneOf.

## Quality patterns

- **Pipeline by default.** When items take several steps, `pipeline` lets each item move on as soon
  as its own step is done; `parallel` per stage makes everyone wait for the slowest. Use a barrier
  (`parallel`) only when a step needs every earlier result (a merge, a ranking, a dedupe).
- **Structured answers for anything the script reads.** Use `schema=`; keep schemas small.
- **Adversarial verify.** A second agent that tries to refute each claim drops most false
  findings; for important claims use several skeptics and keep a claim on a majority.
- **Perspective-diverse verify.** Give each verifier a different lens (correctness, security,
  "would this really happen"), not the same prompt N times.
- **Judge panel.** When candidates compete (designs, fixes), several judges score them against the
  same rubric; the script picks by the scores, not by the first answer.
- **Loop until dry.** Run finders in rounds with `for i in range(n)`, telling each round what is
  already found; stop when a round adds nothing new or the budget is low.
- **Completeness critic.** One agent is given the objective and the result and asked what is
  missing; if it names gaps, run another round on them.
- **No silent caps.** If you sample, truncate or skip, `log()` what was dropped and why, and say so
  in the result.
- **Fan out wide, then narrow.** Many cheap finders, fewer careful verifiers, one writer.
- **Budget loops.** `if budget.remaining() != None and budget.remaining() < 20000: break`.

## Large inputs: batch

For hundreds of items do not send one agent per item. Cut the list into batches in `main`
(`items[i:i + 20]`), one agent per batch, each returning a structured list, then merge. Keep the
batch size in `args` so a rerun can change it.

## Resume

Rerunning a changed script with `resume_from` (the run id) reuses every unchanged agent call before
the first changed one; put the expensive, stable work first. The script is saved to a file: edit it
and resume rather than starting over.

## Example: review every changed file, verify, summarise

```python
META = {"name": "file-review", "description": "Review each changed file and verify each issue", "phases": ["review", "verify", "summary"]}

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

def review(prev, path, i):
    found = agent("Review " + path + " for bugs. Report each with its line.", label = path, phase = "review", schema = ISSUES)
    if found == None:
        return []
    kept = []
    for issue in found["issues"]:
        v = agent("Try to refute: " + path + ":" + str(issue["line"]) + " " + issue["claim"], phase = "verify", schema = VERDICT)
        if v != None and v["real"]:
            kept.append({"file": path, "line": issue["line"], "claim": issue["claim"]})
    return kept

def main(args):
    per_file = pipeline(args["files"], review)
    issues = []
    for found in per_file:
        if found:
            issues.extend(found)
    phase("summary")
    log(str(len(issues)) + " verified issue(s)")
    return {"issues": issues}
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
