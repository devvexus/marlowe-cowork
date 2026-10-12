---
name: workflow-authoring
description: Reference for writing a workflow script (Starlark): the API, the rules that keep a script valid, quality patterns and worked examples. Load before writing a script for the `workflow` tool.
---

# Writing workflow scripts

A workflow is a Starlark file (Python-like; no Python runtime). The `workflow` tool checks it before
anything runs and hands back every error with its line. It runs in the background as a crew run;
its agents are crew steps in a sandbox, journalled, resumable.

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
f-strings or arithmetic. The top level only defines constants and functions; agents start in `main`.

## API

| Call | Returns |
|---|---|
| `agent(prompt, label=, phase=, schema=, model=, effort=, isolation="worktree", agent_type=)` | the agent's final text; with `schema`, a dict matching it; `None` when it failed or was skipped |
| `parallel([f, g, ...])` | each function's result in order, once all are done; a function that fails gives `None` |
| `pipeline(items, stage1, stage2, ...)` | each item through the stages, independently; a stage is `stage(prev, item, index)` (fewer parameters are fine; `prev` is the item for the first stage); a failing stage gives `None` for that item |
| `phase(name)` | enters a phase: later agents are grouped under it |
| `log(...)`, `print(...)` | a line in the progress view and the journal |
| `workflow(name, args)` | runs a saved workflow and returns its result (one level deep) |
| `budget.total`, `budget.spent()`, `budget.remaining()` | the session's token target (`+500k`), or `None` when none was set; `agent()` refuses once it is spent |
| `json.encode(x)`, `json.decode(s)` | JSON text |

Limits: 1000 agents per run, 4096 items per `parallel`/`pipeline` call, at most 16 agents at once
(fewer on a local model). One pass of the script runs at most 5 million statements, comprehension
iterations and lambda calls, for at most 120 s, and holds at most 256 MB; `range()` gives at most
100000 numbers; `*` and `+` build at most 16 MB; a prompt is at most 1 MB; a `log` line is cut at 4000
characters. The top level runs only once the run starts, never during the check.

## Rules the checker enforces

- No `while`, no recursion: loop with `for i in range(n):` and `break`.
- No clock, randomness, files, network or `load()`: agents do all the work.
- No `try`/`except`: a failed agent returns `None`; check for it.
- A lambda or nested `def` inside a loop or comprehension must not read the loop variable, or a
  variable the loop body sets, unbound: write `[lambda f = f: agent(f) for f in files]`,
  `lambda p = p: agent(p)`, or use `pipeline(files, lambda prev, f, i: agent(f))`.
- Write `x = x * n` and `x = x << n`: `*=` and `<<=` are not available. Names starting with `_wf_` are reserved.
- Schemas: the root is `{"type": "object", ...}` and `required` names only declared properties.
  The subset understood: type, properties, required, items, enum, const, minimum/maximum,
  minLength/maxLength, minItems/maxItems, additionalProperties, anyOf, oneOf.

## Quality patterns

- **Pipeline over barrier.** When items take several steps, `pipeline` lets each item move on as soon
  as its own step is done; `parallel` per stage makes everyone wait for the slowest.
- **One concrete task per agent**, with everything it needs in the prompt (paths, the exact question,
  the answer's shape). Agents cannot ask questions mid-run.
- **Structured answers for anything the script reads.** Use `schema=`; keep schemas small.
- **Verify adversarially.** A second agent that tries to refute each claim drops most false
  findings; keep a claim on a majority of verifiers.
- **Fan out wide, then narrow.** Many cheap finders, then fewer careful verifiers, then one writer.
- **Budget loops.** `for i in range(50): if budget.remaining() != None and budget.remaining() < 20000: break`.
- **Resume.** Rerunning a changed script with `resume_from` reuses every unchanged call before the
  first changed one; put the expensive, stable work first.

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

## Running

- `workflow{script: "...", args: {...}}` (inline; saved under the run's directory),
  `workflow{script_path: "..."}`, `workflow{name: "code-review", args: {...}}`.
- `validate_only: true` checks without running. `background: false` waits for the result.
- Saved workflows: `.marlowe/workflows/<name>.star` (project), `<home>/workflows/<name>.star` (user),
  plugins' `workflows/`, and the built-ins `code-review` and `simplify`.
- A Claude Code `.claude/workflows/<name>.js` is converted once with `/workflow convert <name>`.
