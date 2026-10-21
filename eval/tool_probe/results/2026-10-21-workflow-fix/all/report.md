# Tool probe: first-try calls

77 cases. strict = right tool, schema-exact args, no rewrite; lenient = after the forgiving layer.

## Totals

| model | arm | runs | strict | lenient | first try | rescued | scenarios 1-4 strict | meets |
|---|---|---|---|---|---|---|---|---|
| marlowe-dawn:9b-super | C | 231 | 64.5% | 66.2% | 138 | 15 | 9/12 | no |

## Per case

| case | request | marlowe-dawn:9b-super C strict/lenient | most common wrong first call | rewrite applied |
|---|---|---|---|---|
| read | Show me crates/marlowe-agent/src/context.rs from line 460 | 2/3 / 2/3 ft 2/3 | read |  |
| write | Create notes/todo.md containing '- train v2' | 2/3 / 2/3 ft 2/3 | write |  |
| edit | In README.md change 'Marlowe 0.3' to 'Marlowe 0.4' | 3/3 / 3/3 ft 0/3 |  |  |
| multi_edit | Rename the constant MAX_RETRY to MAX_RETRIES in src/net.rs,  | 0/3 / 0/3 ft 0/3 | rename_symbol |  |
| notebook_edit | In analysis.ipynb replace cell 3 with print(df.describe()) | 1/3 / 1/3 ft 0/3 | edit |  |
| glob | Which Rust files are under crates/marlowe-tui? | 3/3 / 3/3 ft 3/3 |  |  |
| grep | Where is PrefixBudget used? | 2/3 / 2/3 ft 2/3 | symbols |  |
| bash | Run the unit tests for marlowe-agent in the background | 1/3 / 1/3 ft 1/3 | dispatch |  |
| powershell | List the Windows services whose name starts with Ollama | 2/3 / 2/3 ft 2/3 | powershell |  |
| python | Compute the mean of 3.2, 4.8 and 5.1 with Python | 2/3 / 2/3 ft 2/3 |  |  |
| web_fetch-arxiv | read this paper: https://arxiv.org/abs/2407.01234 | 2/3 / 2/3 ft 2/3 | web_fetch |  |
| web_fetch-url | fetch https://example.com/docs | 3/3 / 3/3 ft 3/3 |  |  |
| arxiv | Find recent arXiv papers on min-p sampling | 2/3 / 2/3 ft 2/3 | arxiv |  |
| web_search | What is the latest stable Rust version? | 2/3 / 2/3 ft 2/3 |  |  |
| symbols | Where is fit_prefix defined? | 2/3 / 2/3 ft 2/3 | definition |  |
| definition | Show me the body of PrefixBudget::for_window | 2/3 / 2/3 ft 2/3 | symbols |  |
| references | Who calls estimate_tokens? | 2/3 / 2/3 ft 2/3 | symbols |  |
| hover | What type does fit_prefix return? | 2/3 / 2/3 ft 2/3 | definition |  |
| diagnostics | Does src/lib.rs have type errors? | 3/3 / 3/3 ft 3/3 |  |  |
| rename_symbol | Rename fit_prefix to fit_stable_prefix everywhere | 1/3 / 1/3 ft 1/3 | renames_symbol |  |
| project-test | Run the tests | 3/3 / 3/3 ft 3/3 |  |  |
| project-gate | Is it ready to commit? | 0/3 / 0/3 ft 0/3 | git |  |
| service | Start the vite dev server and wait until it is up | 0/3 / 0/3 ft 0/3 | service |  |
| http_check | Check that localhost:8080/health returns 200 with ok in the  | 3/3 / 3/3 ft 3/3 |  |  |
| browser | Screenshot the app at localhost:5173 | 1/3 / 1/3 ft 1/3 | browser |  |
| todo | Plan the journal refactor in three steps and track them | 0/3 / 0/3 ft 0/3 | enter_plan |  |
| ask_user | Set up the new crate; ask me whether it should be a lib or a | 0/3 / 0/3 ft 0/3 | bash |  |
| skill | /review the current diff | 0/3 / 0/3 ft 0/3 | git |  |
| agent | Have a subagent find every place we read the env var MARLOWE | 3/3 / 3/3 ft 3/3 |  |  |
| send_message | Tell the researcher run agent_r2 to focus on 2025 papers | 2/3 / 2/3 ft 2/3 |  |  |
| enter_plan | Before touching the journal format, plan it with me first | 2/3 / 2/3 ft 2/3 |  |  |
| exit_plan | OK, show me the plan | 3/3 / 3/3 ft 3/3 |  |  |
| enter_worktree | Do this experiment in a separate worktree | 0/3 / 0/3 ft 0/3 |  |  |
| exit_worktree | We are done in the worktree, go back | 3/3 / 3/3 ft 3/3 |  |  |
| git-diff | What changed since main? | 3/3 / 3/3 ft 3/3 |  |  |
| git-commit | commit this as 'Fix budget' | 1/3 / 1/3 ft 1/3 | git |  |
| gh-ci | Did CI pass on my branch? | 1/3 / 1/3 ft 1/3 | gh |  |
| gh-pr | Open a PR for this branch | 0/3 / 0/3 ft 0/3 | git |  |
| workflow | Spawn a research workflow on entropy-based sampling | 2/3 / 2/3 ft 1/3 |  |  |
| report_findings | Review src/net.rs for bugs and report them to the panel | 0/3 / 0/3 ft 0/3 | read |  |
| task_list | What is running in the background? | 3/3 / 3/3 ft 3/3 |  |  |
| task_output | How is the background build doing? | 3/3 / 3/3 ft 3/3 |  |  |
| task_stop | Stop the background build | 3/3 / 3/3 ft 3/3 |  |  |
| cancel-workflow | cancel the workflow | 2/3 / 2/3 ft 0/3 | task_list |  |
| monitor | Tell me whenever train.log prints a loss line | 1/3 / 1/3 ft 1/3 | bash |  |
| schedule | Remind me at 9 tomorrow to check the eval | 2/3 / 2/3 ft 2/3 |  |  |
| watch | Keep an eye on runs/dawn-v3 and tell me if it NaNs | 2/3 / 2/3 ft 2/3 |  |  |
| notify | Training just finished. Send me a notification. | 3/3 / 3/3 ft 3/3 |  |  |
| send_file | Send me the loss plot at plots/loss.png | 2/3 / 2/3 ft 2/3 | read |  |
| notices | Anything happen while I was away? | 0/3 / 0/3 ft 0/3 | task_list |  |
| remember | From now on use uv, not pip | 2/3 / 2/3 ft 2/3 |  |  |
| recall | What did we decide about the tokenizer last week? | 3/3 / 3/3 ft 3/3 |  |  |
| forget | Forget that I prefer tabs | 3/3 / 3/3 ft 0/3 |  |  |
| correct | Actually I switched from pip to uv | 0/3 / 0/3 ft 0/3 | remember |  |
| dispatch | Do a deep literature review on speculative decoding in the b | 3/3 / 3/3 ft 3/3 |  |  |
| list_mcp_resources | What documents does the notes MCP server have? | 3/3 / 3/3 ft 3/3 |  |  |
| read_mcp_resource | Open notes://weekly from the notes server | 0/3 / 0/3 ft 0/3 | list_mcp_resources |  |
| mcp-direct | Use the notes server to find my notes about the tokenizer | 3/3 / 3/3 ft 3/3 |  |  |
| mcp-overflow | Use the notes server to find my notes about the tokenizer | 3/3 / 3/3 ft 3/3 |  |  |
| handback | You are done. Finding: the env var is read in src/env.rs:12  | 1/3 / 1/3 ft 1/3 | read |  |
| set_statusline | Show the branch and model in my status line | 0/3 / 0/3 ft 0/3 | handback |  |
| create_agent | Make me an agent that reviews SQL migrations | 0/3 / 0/3 ft 0/3 | glob |  |
| workflow-summarize-repo | Read-only, spawn a workflow to summarize the repo workspace. | 2/3 / 3/3 ft 2/3 | workflow | workflow: item_type ->  |
| workflow-per-crate | Use a workflow: one agent per crate in crates/ reports what  | 3/3 / 3/3 ft 3/3 |  |  |
| workflow-research-three | Run a workflow that researches entropy-based sampling with t | 3/3 / 3/3 ft 2/3 |  |  |
| workflow-review-commits | Spawn a workflow to review the last 3 commits for bugs. | 3/3 / 3/3 ft 3/3 |  |  |
| cancel-workflow-notice | Cancel the workflow | 3/3 / 3/3 ft 3/3 |  |  |
| stop-agent-notice | stop that agent | 3/3 / 3/3 ft 3/3 |  |  |
| stop-agent-bare-id | stop that agent | 0/3 / 3/3 ft 0/3 | task_stop |  |
| stop-agent-dispatched | stop that agent | 3/3 / 3/3 ft 3/3 |  |  |
| workflow-use-a | Use a workflow to find every TODO in the code and group them | 3/3 / 3/3 ft 3/3 |  |  |
| workflow-fanout-words | Fan out a few agents over the top-level folders and tell me  | 3/3 / 3/3 ft 3/3 |  |  |
| workflow-run-as | Run this as a workflow: compare how the services handle erro | 3/3 / 3/3 ft 3/3 |  |  |
| workflow-kick-off | Kick off a workflow comparing min-p, top-k and nucleus sampl | 3/3 / 3/3 ft 3/3 |  |  |
| workflow-session-repeat | Read-only, spawn a workflow to summarize the repo workspace. | 3/3 / 3/3 ft 3/3 |  |  |
| workflow-question | What is the difference between a workflow and dispatch? | 3/3 / 3/3 ft 3/3 |  |  |
| workflow-why-failed | Why did my last workflow fail? | 3/3 / 3/3 ft 3/3 |  |  |

## Prompt cost of the tool schemas (measured against chars/3)

| model | arm | surface | measured tokens | chars/3 estimate | harness estimate | measured / chars-3 | chars per token |
|---|---|---|---|---|---|---|---|

## Revise and re-probe (dawn failed strict on 2 of 3 runs)

- marlowe-dawn:9b-super arm C: multi_edit, notebook_edit, bash, rename_symbol, project-gate, service, browser, todo, ask_user, skill, enter_worktree, git-commit, gh-ci, gh-pr, report_findings, monitor, notices, correct, read_mcp_resource, handback, set_statusline, create_agent, stop-agent-bare-id
