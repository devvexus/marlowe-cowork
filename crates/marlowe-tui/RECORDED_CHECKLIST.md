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
| all | Resize the window narrow and wide while each of the above is open | no torn rows, no stray escape text, colours are the locked palette on `#0f0e14` |
