# Recorded-script checklist (design 2 `[R]` rows)

What the key-replay tests cannot prove: that a real terminal paints and reacts. Run once per
terminal, **Windows Terminal** and the **VS Code integrated terminal**, against a daemon on a
scratch workspace with a scripted or local model. Tick each line; any miss is a bug in the row.

| Row | Do | Expect |
|---|---|---|
| U1 | Ask for a one-line edit to a file; at the approval, look before answering | the diff, not the arguments: red `-`, green `+`, hunk headers in the structure tint, no background fills |
| U1 | Allow it; look at the tool line | the same diff under it, long ones capped with "N more rows, ^o shows all" |
| U2 | Hover a `src/lib.rs:12` on a tool line, then a diff header | underline on hover; Ctrl+click opens the file at that line (set `editor_url = "vscode://file/{path}:{line}:{col}"` in `/config` first) |
| U2 | Click an `https://` URL in a reply | opens the browser; prose like `a/b` stays plain |
| U3 | Copy a screenshot, press Ctrl+V in the box | `[Image #1]` chip; send; the turn carries the image |
| U3 | Drag a `.png` from Explorer into the box | the path becomes an attachment, not text |
| T18 | Run `!ping -n 30 127.0.0.1`, press Ctrl+B | the command moves to a job; output keeps arriving in `/jobs` and the box is free |
| T23 | Let the model call `ask_user` with two questions | a modal; arrows + Enter answer, the answers go back in order, Esc skips |
| C14 | `/effort high`, send a turn that thinks | the reasoning shows folded as `thinking · Ns`; Enter or a click opens it in place |
| T33 | Let the model call `ask_user` with options that carry a description and a preview | the focused option's preview is a monospace block beside the list (under it below 130 columns), it changes as the cursor moves, the two-column alignment survives a resize |
| M13 | Switch to plan mode, ask for a change, let the model call `exit_plan` | the plan modal shows the rendered plan; `e` opens the file in `$EDITOR`, and after saving the modal shows the edit; 1 / 2 / 3 behave as the footer says; Esc leaves plan mode on |
| WF7 | `/workflow code-review` on a repo with a pending diff | the progress view fills in by phase; `s` on a running agent marks it skipped and the run goes on; `log()` lines arrive in order; the completion turn reads `↳ background · workflow`, not `you` |
| BG1 | Start `sleep 60` with Ctrl+B, a monitor and a background agent; `/tasks` | one row of each kind; Enter on the job streams its output; `x` stops it and the strip under the conversation drops it |
| BG4 | Ask for a monitor on a command that prints a line a second, leave the box empty | each batch arrives as a `↳ background · monitor` entry and wakes the idle conversation |
| BG7 | `/config`, switch the toast off, ask the model to `notify` | no toast, and the line `notification · ...` in the conversation; switch it on and a Windows toast appears |
| BG8 | Ask the model to `send_file` a small HTML page | a card with the name, size and path; `/files` Enter opens it in the browser, `r` selects it in Explorer |
| G13 | `/code-review` on a seeded bug | the panel lists the finding worst first; Ctrl+click on its `file:line` opens the editor there; `f` puts the fix request in the Message box |
| quit | Ctrl+Q with a job running; then again with "Remember my choice" ticked and "Close window"; then close the window with its X | the modal lists what runs; the second time nothing asks, the daemon keeps running with a tray icon, and the X does the same; "Stop and close agent" unloads the models before the window goes |
| all | Resize the window narrow and wide while each of the above is open | no torn rows, no stray escape text, colours are the locked palette on `#0f0e14` |
