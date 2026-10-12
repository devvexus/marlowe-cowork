# Marlowe for VS Code

Lets Marlowe (the terminal coworker) use this editor. It starts a small server on `127.0.0.1` and
writes a connection file, `%LOCALAPPDATA%\MarloweCowork\ide\<port>.lock`, with the address and a
random token. Marlowe reads that file when you work in one of this window's folders and adds the
tools below as `mcp__ide__*`. Nothing leaves the machine; the server refuses anything without the
token, anything a browser sends, and any Host header that is not itself.

| Tool | What it does | Asks? |
|---|---|---|
| `get_context` | the focused file's selection, the open tabs, the workspace folders | no |
| `get_diagnostics` | errors and warnings from the language servers, for all files or one | no |
| `open_file` | shows a file, optionally selecting lines | no |
| `open_diff` | shows a proposed change as a diff (nothing is written); with `wait`, reports Accept or Reject | no |
| `close_diffs` | closes the diffs Marlowe opened | no |
| `execute_code` | runs code in the running kernel of the notebook you have open (needs the Jupyter extension) | yes |

In an untrusted workspace the diff and code tools refuse.

Settings: `marlowe.enabled` (turn the connection off, which removes the file) and `marlowe.home`
(Marlowe's profile folder if it is not the default). Commands: *Marlowe: Show Connection*,
*Restart Connection*, *Close Proposed Diffs*.

Build and test: `npm install`, `npm test` (no editor needed), `npm run package` (writes
`dist/marlowe-vscode.vsix`), `npm run test:host` (opens a real VS Code, by hand).
