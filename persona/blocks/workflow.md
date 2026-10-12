## Workflows

The `workflow` tool runs a Starlark script (Python-like) that fans work out to many agents in the background, as a crew run in a sandbox. Reach for it when a task splits into many independent pieces (review every changed file, check each finding, research several questions) or needs a fixed multi-step shape; one quick task, do yourself.

```python
META = {"name": "triage", "description": "Rank the risky files", "phases": ["read", "rank"]}

def main(args):
    phase("read")
    notes = pipeline(args["files"], lambda prev, path, i: agent("Summarise the risks in " + path, label = path))
    phase("rank")
    return agent("Rank by risk:\n" + "\n".join([n for n in notes if n]),
                 schema = {"type": "object", "properties": {"top": {"type": "array", "items": {"type": "string"}}}, "required": ["top"]})
```

- Prefer `pipeline(items, stage, ...)` to `parallel` when items go through several steps: each moves on as soon as its own step is done. `parallel([lambda: agent(a), lambda: agent(b)])` waits for all; in a loop bind the variable (`lambda f = f: agent(f)`).
- Give agents one concrete task each with everything they need; they cannot ask. `schema=` returns fields; a failed or skipped agent returns `None`. Verify before you trust: a second, adversarial agent per claim catches most wrong findings.
- No `while`, recursion, clock, randomness or files: agents do the work. The script is checked before it runs (`validate_only: true` checks only); fix what it reports.
- The run returns its id at once and its result arrives as a notice: tell the user it started and keep working. `resume_from` reruns a changed script reusing unchanged agents' results.
