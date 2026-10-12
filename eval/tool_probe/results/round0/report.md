# Tool probe: first-try calls

62 cases. strict = right tool, schema-exact args, no rewrite; lenient = after the forgiving layer.

## Totals

| model | arm | runs | strict | lenient | scenarios 1-4 strict | meets |
|---|---|---|---|---|---|---|
| marlowe-dawn:9b | B | 124 | 51.6% | 52.4% | 5/8 | no |
| marlowe-dawn:9b | C | 124 | 50.0% | 50.0% | 4/8 | no |
| marlowe-dusk:27b-super | B | 124 | 10.5% | 12.9% | 0/8 | no |
| marlowe-dusk:27b-super | C | 124 | 54.8% | 54.8% | 6/8 | no |

## Per case

| case | request | marlowe-dawn:9b B strict/lenient | marlowe-dawn:9b C strict/lenient | marlowe-dusk:27b-super B strict/lenient | marlowe-dusk:27b-super C strict/lenient | most common wrong first call | rewrite applied |
|---|---|---|---|---|---|---|---|
| read | Show me crates/marlowe-agent/src/context.rs from line 460 | 1/2 / 1/2 | 0/2 / 0/2 | 0/2 / 0/2 | 2/2 / 2/2 | read |  |
| write | Create notes/todo.md containing '- train v2' | 2/2 / 2/2 | 2/2 / 2/2 | 0/2 / 0/2 | 1/2 / 1/2 | write |  |
| edit | In README.md change 'Marlowe 0.3' to 'Marlowe 0.4' | 1/2 / 1/2 | 2/2 / 2/2 | 0/2 / 0/2 | 2/2 / 2/2 | multi_edit |  |
| multi_edit | Rename the constant MAX_RETRY to MAX_RETRIES in src/net.rs,  | 0/2 / 0/2 | 0/2 / 0/2 | 0/2 / 0/2 | 0/2 / 0/2 | read |  |
| notebook_edit | In analysis.ipynb replace cell 3 with print(df.describe()) | 2/2 / 2/2 | 2/2 / 2/2 | 0/2 / 0/2 | 1/2 / 1/2 | glob |  |
| glob | Which Rust files are under crates/marlowe-tui? | 2/2 / 2/2 | 2/2 / 2/2 | 0/2 / 0/2 | 2/2 / 2/2 | glob |  |
| grep | Where is PrefixBudget used? | 0/2 / 0/2 | 0/2 / 0/2 | 0/2 / 0/2 | 0/2 / 0/2 | symbols |  |
| bash | Run the unit tests for marlowe-agent in the background | 0/2 / 0/2 | 0/2 / 0/2 | 0/2 / 0/2 | 0/2 / 0/2 | monitor |  |
| powershell | List the Windows services whose name starts with Ollama | 0/2 / 0/2 | 0/2 / 0/2 | 0/2 / 0/2 | 2/2 / 2/2 | bash |  |
| python | Compute the mean of 3.2, 4.8 and 5.1 with Python | 2/2 / 2/2 | 2/2 / 2/2 | 0/2 / 0/2 | 2/2 / 2/2 | python |  |
| web_fetch-arxiv | read this paper: https://arxiv.org/abs/2407.01234 | 1/2 / 1/2 | 2/2 / 2/2 | 0/2 / 0/2 | 2/2 / 2/2 | arxiv |  |
| web_fetch-url | fetch https://example.com/docs | 2/2 / 2/2 | 2/2 / 2/2 | 0/2 / 0/2 | 2/2 / 2/2 | web_fetch |  |
| arxiv | Find recent arXiv papers on min-p sampling | 2/2 / 2/2 | 2/2 / 2/2 | 0/2 / 0/2 | 2/2 / 2/2 | arxiv |  |
| web_search | What is the latest stable Rust version? | 1/2 / 1/2 | 1/2 / 1/2 | 0/2 / 0/2 | 1/2 / 1/2 | web_search |  |
| symbols | Where is fit_prefix defined? | 1/2 / 1/2 | 1/2 / 1/2 | 0/2 / 0/2 | 1/2 / 1/2 | definition |  |
| definition | Show me the body of PrefixBudget::for_window | 2/2 / 2/2 | 2/2 / 2/2 | 0/2 / 0/2 | 2/2 / 2/2 | definition |  |
| references | Who calls estimate_tokens? | 0/2 / 0/2 | 0/2 / 0/2 | 0/2 / 0/2 | 2/2 / 2/2 | symbols |  |
| hover | What type does fit_prefix return? | 0/2 / 0/2 | 0/2 / 0/2 | 0/2 / 0/2 | 0/2 / 0/2 | read |  |
| diagnostics | Does src/lib.rs have type errors? | 2/2 / 2/2 | 2/2 / 2/2 | 0/2 / 0/2 | 2/2 / 2/2 | diagnostics |  |
| rename_symbol | Rename fit_prefix to fit_stable_prefix everywhere | 2/2 / 2/2 | 2/2 / 2/2 | 0/2 / 0/2 | 1/2 / 1/2 | rename_symbol |  |
| project-test | Run the tests | 2/2 / 2/2 | 2/2 / 2/2 | 2/2 / 2/2 | 2/2 / 2/2 |  |  |
| project-gate | Is it ready to commit? | 0/2 / 0/2 | 0/2 / 0/2 | 0/2 / 0/2 | 0/2 / 0/2 | git |  |
| service | Start the vite dev server and wait until it is up | 0/2 / 0/2 | 1/2 / 1/2 | 0/2 / 0/2 | 0/2 / 0/2 | service |  |
| http_check | Check that localhost:8080/health returns 200 with ok in the  | 2/2 / 2/2 | 2/2 / 2/2 | 0/2 / 2/2 | 2/2 / 2/2 | http_check | http_check: expect_status coerced |
| browser | Screenshot the app at localhost:5173 | 2/2 / 2/2 | 0/2 / 0/2 | 1/2 / 2/2 | 2/2 / 2/2 | browser | browser: url coerced |
| todo | Plan the journal refactor in three steps and track them | 1/2 / 1/2 | 1/2 / 1/2 | 0/2 / 0/2 | 0/2 / 0/2 | glob |  |
| ask_user | Set up the new crate; ask me whether it should be a lib or a | 0/2 / 0/2 | 0/2 / 0/2 | 0/2 / 0/2 | 0/2 / 0/2 | bash |  |
| skill | /review the current diff | 1/2 / 1/2 | 2/2 / 2/2 | 0/2 / 0/2 | 2/2 / 2/2 | git |  |
| agent | Have a subagent find every place we read the env var MARLOWE | 1/2 / 1/2 | 0/2 / 0/2 | 0/2 / 0/2 | 1/2 / 1/2 | dispatch |  |
| send_message | Tell the researcher run agent_r2 to focus on 2025 papers | 2/2 / 2/2 | 2/2 / 2/2 | 0/2 / 0/2 | 2/2 / 2/2 | send_message |  |
| enter_plan | Before touching the journal format, plan it with me first | 0/2 / 0/2 | 0/2 / 0/2 | 0/2 / 0/2 | 0/2 / 0/2 | grep |  |
| exit_plan | OK, show me the plan | 1/2 / 1/2 | 2/2 / 2/2 | 0/2 / 0/2 | 0/2 / 0/2 | read |  |
| enter_worktree | Do this experiment in a separate worktree | 0/2 / 0/2 | 0/2 / 0/2 | 0/2 / 0/2 | 0/2 / 0/2 | recall |  |
| exit_worktree | We are done in the worktree, go back | 2/2 / 2/2 | 1/2 / 1/2 | 1/2 / 1/2 | 1/2 / 1/2 | git |  |
| git-diff | What changed since main? | 0/2 / 1/2 | 0/2 / 0/2 | 0/2 / 0/2 | 0/2 / 0/2 | git | git: path -> paths |
| git-commit | commit this as 'Fix budget' | 0/2 / 0/2 | 0/2 / 0/2 | 0/2 / 0/2 | 0/2 / 0/2 | git |  |
| gh-ci | Did CI pass on my branch? | 0/2 / 0/2 | 0/2 / 0/2 | 2/2 / 2/2 | 0/2 / 0/2 | git |  |
| gh-pr | Open a PR for this branch | 0/2 / 0/2 | 0/2 / 0/2 | 0/2 / 0/2 | 0/2 / 0/2 | git | git: rev coerced |
| workflow | Spawn a research workflow on entropy-based sampling | 0/2 / 0/2 | 0/2 / 0/2 | 0/2 / 0/2 | 0/2 / 0/2 | dispatch |  |
| report_findings | Review src/net.rs for bugs and report them to the panel | 0/2 / 0/2 | 0/2 / 0/2 | 0/2 / 0/2 | 0/2 / 0/2 | read |  |
| task_list | What is running in the background? | 2/2 / 2/2 | 2/2 / 2/2 | 2/2 / 2/2 | 2/2 / 2/2 |  |  |
| task_output | How is the background build doing? | 2/2 / 2/2 | 2/2 / 2/2 | 0/2 / 0/2 | 2/2 / 2/2 | task_output |  |
| task_stop | Stop the background build | 1/2 / 1/2 | 2/2 / 2/2 | 0/2 / 0/2 | 2/2 / 2/2 | bash |  |
| cancel-workflow | cancel the workflow | 2/2 / 2/2 | 0/2 / 0/2 | 0/2 / 0/2 | 2/2 / 2/2 | task_list |  |
| monitor | Tell me whenever train.log prints a loss line | 0/2 / 0/2 | 0/2 / 0/2 | 0/2 / 0/2 | 0/2 / 0/2 | grep |  |
| schedule | Remind me at 9 tomorrow to check the eval | 1/2 / 1/2 | 1/2 / 1/2 | 0/2 / 0/2 | 1/2 / 1/2 | schedule |  |
| watch | Keep an eye on runs/dawn-v3 and tell me if it NaNs | 1/2 / 1/2 | 2/2 / 2/2 | 0/2 / 0/2 | 0/2 / 0/2 | bash |  |
| notify | Training just finished. Send me a notification. | 2/2 / 2/2 | 2/2 / 2/2 | 0/2 / 0/2 | 2/2 / 2/2 | notify |  |
| send_file | Send me the loss plot at plots/loss.png | 2/2 / 2/2 | 2/2 / 2/2 | 0/2 / 0/2 | 0/2 / 0/2 | bash |  |
| notices | Anything happen while I was away? | 0/2 / 0/2 | 0/2 / 0/2 | 2/2 / 2/2 | 2/2 / 2/2 | task_list |  |
| remember | From now on use uv, not pip | 1/2 / 1/2 | 1/2 / 1/2 | 0/2 / 0/2 | 2/2 / 2/2 | bash | remember: text coerced |
| recall | What did we decide about the tokenizer last week? | 2/2 / 2/2 | 2/2 / 2/2 | 0/2 / 0/2 | 2/2 / 2/2 | recall |  |
| forget | Forget that I prefer tabs | 2/2 / 2/2 | 1/2 / 1/2 | 0/2 / 0/2 | 2/2 / 2/2 | recall |  |
| correct | Actually I switched from pip to uv | 0/2 / 0/2 | 0/2 / 0/2 | 0/2 / 0/2 | 2/2 / 2/2 | remember |  |
| dispatch | Do a deep literature review on speculative decoding in the b | 0/2 / 0/2 | 0/2 / 0/2 | 0/2 / 0/2 | 2/2 / 2/2 | arxiv |  |
| list_mcp_resources | What documents does the notes MCP server have? | 2/2 / 2/2 | 2/2 / 2/2 | 0/2 / 0/2 | 2/2 / 2/2 | list_mcp_resources |  |
| read_mcp_resource | Open notes://weekly from the notes server | 0/2 / 0/2 | 0/2 / 0/2 | 0/2 / 0/2 | 0/2 / 0/2 | read_mcp_resource |  |
| mcp-direct | Use the notes server to find my notes about the tokenizer | 2/2 / 2/2 | 2/2 / 2/2 | 1/2 / 1/2 | 2/2 / 2/2 | list_mcp_resources |  |
| mcp-overflow | Use the notes server to find my notes about the tokenizer | 2/2 / 2/2 | 2/2 / 2/2 | 2/2 / 2/2 | 2/2 / 2/2 |  |  |
| handback | You are done. Finding: the env var is read in src/env.rs:12  | 0/2 / 0/2 | 1/2 / 1/2 | 0/2 / 0/2 | 0/2 / 0/2 | read |  |
| set_statusline | Show the branch and model in my status line | 2/2 / 2/2 | 0/2 / 0/2 | 0/2 / 0/2 | 0/2 / 0/2 | glob |  |
| create_agent | Make me an agent that reviews SQL migrations | 1/2 / 1/2 | 1/2 / 1/2 | 0/2 / 0/2 | 0/2 / 0/2 | glob |  |

## Prompt cost of the tool schemas (measured against chars/3)

| model | arm | surface | measured tokens | chars/3 estimate | harness estimate | measured / chars-3 | chars per token |
|---|---|---|---|---|---|---|---|
| marlowe-dawn:9b | B | conductor | 7060 | 6424 | 6478 | 1.099 | 2.73 |
| marlowe-dawn:9b | B | conductor+mcp-d71929 | 7321 | 6616 | 6673 | 1.107 | 2.71 |
| marlowe-dawn:9b | B | conductor+mcp-e05666 | 7604 | 6926 | 6982 | 1.098 | 2.73 |
| marlowe-dawn:9b | B | subagent:explore | 1123 | 801 | 813 | 1.402 | 2.14 |
| marlowe-dawn:9b | B | subagent:statusline | 752 | 483 | 489 | 1.556 | 1.93 |
| marlowe-dawn:9b | B | subagent:agent-creator | 841 | 575 | 580 | 1.463 | 2.05 |
| marlowe-dawn:9b | C | conductor | 8077 | 6424 | 6478 | 1.257 | 2.39 |
| marlowe-dawn:9b | C | conductor+mcp-d71929 | 8351 | 6616 | 6673 | 1.262 | 2.38 |
| marlowe-dawn:9b | C | conductor+mcp-e05666 | 8634 | 6926 | 6982 | 1.247 | 2.41 |
| marlowe-dawn:9b | C | subagent:explore | 1232 | 801 | 813 | 1.538 | 1.95 |
| marlowe-dawn:9b | C | subagent:statusline | 812 | 483 | 489 | 1.68 | 1.79 |
| marlowe-dawn:9b | C | subagent:agent-creator | 935 | 575 | 580 | 1.627 | 1.84 |
| marlowe-dusk:27b-super | B | conductor | 7024 | 6424 | 6478 | 1.093 | 2.74 |
| marlowe-dusk:27b-super | B | conductor+mcp-d71929 | 7285 | 6616 | 6673 | 1.101 | 2.72 |
| marlowe-dusk:27b-super | B | conductor+mcp-e05666 | 7568 | 6926 | 6982 | 1.093 | 2.75 |
| marlowe-dusk:27b-super | B | subagent:explore | 1115 | 801 | 813 | 1.392 | 2.16 |
| marlowe-dusk:27b-super | B | subagent:statusline | 744 | 483 | 489 | 1.539 | 1.95 |
| marlowe-dusk:27b-super | B | subagent:agent-creator | 833 | 575 | 580 | 1.45 | 2.07 |
| marlowe-dusk:27b-super | C | conductor | 8041 | 6424 | 6478 | 1.252 | 2.4 |
| marlowe-dusk:27b-super | C | conductor+mcp-d71929 | 8315 | 6616 | 6673 | 1.257 | 2.39 |
| marlowe-dusk:27b-super | C | conductor+mcp-e05666 | 8598 | 6926 | 6982 | 1.241 | 2.42 |
| marlowe-dusk:27b-super | C | subagent:explore | 1224 | 801 | 813 | 1.528 | 1.96 |
| marlowe-dusk:27b-super | C | subagent:statusline | 804 | 483 | 489 | 1.663 | 1.8 |
| marlowe-dusk:27b-super | C | subagent:agent-creator | 927 | 575 | 580 | 1.613 | 1.86 |

## Revise and re-probe (dawn failed strict on 2 of 3 runs)

- marlowe-dawn:9b arm B: multi_edit, grep, bash, powershell, references, hover, project-gate, service, ask_user, enter_plan, enter_worktree, git-diff, git-commit, gh-ci, gh-pr, workflow, report_findings, monitor, notices, correct, dispatch, read_mcp_resource, handback
- marlowe-dawn:9b arm C: read, multi_edit, grep, bash, powershell, references, hover, project-gate, browser, ask_user, agent, enter_plan, enter_worktree, git-diff, git-commit, gh-ci, gh-pr, workflow, report_findings, cancel-workflow, monitor, notices, correct, dispatch, read_mcp_resource, set_statusline
