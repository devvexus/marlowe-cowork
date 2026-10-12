# Tool probe: first-try calls

15 cases. strict = right tool, schema-exact args, no rewrite; lenient = after the forgiving layer.

## Totals

| model | arm | runs | strict | lenient | scenarios 1-4 strict | meets |
|---|---|---|---|---|---|---|
| marlowe-dawn:9b | C | 45 | 28.9% | 28.9% | 0/3 | no |
| marlowe-dusk:27b-super | C | 45 | 42.2% | 42.2% | 0/3 | no |

## Per case

| case | request | marlowe-dawn:9b C strict/lenient | marlowe-dusk:27b-super C strict/lenient | most common wrong first call | rewrite applied |
|---|---|---|---|---|---|
| grep | Where is PrefixBudget used? | 2/3 / 2/3 | 2/3 / 2/3 | symbols |  |
| bash | Run the unit tests for marlowe-agent in the background | 0/3 / 0/3 | 0/3 / 0/3 | bash |  |
| powershell | List the Windows services whose name starts with Ollama | 1/3 / 1/3 | 3/3 / 3/3 | bash |  |
| symbols | Where is fit_prefix defined? | 1/3 / 1/3 | 3/3 / 3/3 | definition |  |
| references | Who calls estimate_tokens? | 1/3 / 1/3 | 2/3 / 2/3 | grep |  |
| hover | What type does fit_prefix return? | 1/3 / 1/3 | 2/3 / 2/3 | read |  |
| project-gate | Is it ready to commit? | 0/3 / 0/3 | 0/3 / 0/3 | git |  |
| service | Start the vite dev server and wait until it is up | 1/3 / 1/3 | 0/3 / 0/3 | service |  |
| ask_user | Set up the new crate; ask me whether it should be a lib or a | 0/3 / 0/3 | 0/3 / 0/3 | write |  |
| agent | Have a subagent find every place we read the env var MARLOWE | 1/3 / 1/3 | 3/3 / 3/3 | grep |  |
| enter_plan | Before touching the journal format, plan it with me first | 0/3 / 0/3 | 1/3 / 1/3 | recall |  |
| enter_worktree | Do this experiment in a separate worktree | 0/3 / 0/3 | 0/3 / 0/3 | recall |  |
| workflow | Spawn a research workflow on entropy-based sampling | 0/3 / 0/3 | 0/3 / 0/3 | dispatch |  |
| monitor | Tell me whenever train.log prints a loss line | 3/3 / 3/3 | 0/3 / 0/3 | glob |  |
| dispatch | Do a deep literature review on speculative decoding in the b | 2/3 / 2/3 | 3/3 / 3/3 |  |  |

## Prompt cost of the tool schemas (measured against chars/3)

| model | arm | surface | measured tokens | chars/3 estimate | harness estimate | measured / chars-3 | chars per token |
|---|---|---|---|---|---|---|---|

## Revise and re-probe (dawn failed strict on 2 of 3 runs)

- marlowe-dawn:9b arm C: bash, powershell, symbols, references, hover, project-gate, service, ask_user, agent, enter_plan, enter_worktree, workflow
