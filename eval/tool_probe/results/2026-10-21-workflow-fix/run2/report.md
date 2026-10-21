# Tool probe: first-try calls

12 cases. strict = right tool, schema-exact args, no rewrite; lenient = after the forgiving layer.

## Totals

| model | arm | runs | strict | lenient | first try | rescued | scenarios 1-4 strict | meets |
|---|---|---|---|---|---|---|---|---|
| marlowe-dawn:9b-super | C | 36 | 83.3% | 83.3% | 28 | 2 | 1/3 | no |

## Per case

| case | request | marlowe-dawn:9b-super C strict/lenient | most common wrong first call | rewrite applied |
|---|---|---|---|---|
| workflow | Spawn a research workflow on entropy-based sampling | 1/3 / 1/3 ft 1/3 |  |  |
| workflow-summarize-repo | Read-only, spawn a workflow to summarize the repo workspace. | 3/3 / 3/3 ft 3/3 |  |  |
| workflow-per-crate | Use a workflow: one agent per crate in crates/ reports what  | 1/3 / 1/3 ft 1/3 | workflow |  |
| workflow-research-three | Run a workflow that researches entropy-based sampling with t | 2/3 / 2/3 ft 2/3 | workflow |  |
| workflow-review-commits | Spawn a workflow to review the last 3 commits for bugs. | 2/3 / 2/3 ft 2/3 |  |  |
| workflow-use-a | Use a workflow to find every TODO in the code and group them | 3/3 / 3/3 ft 2/3 |  |  |
| workflow-fanout-words | Fan out a few agents over the top-level folders and tell me  | 3/3 / 3/3 ft 3/3 |  |  |
| workflow-run-as | Run this as a workflow: compare how the services handle erro | 3/3 / 3/3 ft 3/3 |  |  |
| workflow-kick-off | Kick off a workflow comparing min-p, top-k and nucleus sampl | 3/3 / 3/3 ft 3/3 |  |  |
| workflow-session-repeat | Read-only, spawn a workflow to summarize the repo workspace. | 3/3 / 3/3 ft 2/3 |  |  |
| workflow-question | What is the difference between a workflow and dispatch? | 3/3 / 3/3 ft 3/3 |  |  |
| workflow-why-failed | Why did my last workflow fail? | 3/3 / 3/3 ft 3/3 |  |  |

## Prompt cost of the tool schemas (measured against chars/3)

| model | arm | surface | measured tokens | chars/3 estimate | harness estimate | measured / chars-3 | chars per token |
|---|---|---|---|---|---|---|---|

## Revise and re-probe (dawn failed strict on 2 of 3 runs)

- marlowe-dawn:9b-super arm C: workflow, workflow-per-crate
