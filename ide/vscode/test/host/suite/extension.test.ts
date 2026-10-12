// What only a real VS Code can show: the extension starts, writes its lock file, and its tools see
// the real selection, real diagnostics and real tabs.

import * as assert from "node:assert/strict";
import * as fs from "node:fs";
import * as http from "node:http";
import * as path from "node:path";
import * as vscode from "vscode";

interface Api {
  port: number | undefined;
  lockFile: string | undefined;
}

function rpc(lock: { url: string; authToken: string }, method: string, params: unknown): Promise<any> {
  return new Promise((resolve, reject) => {
    const u = new URL(lock.url);
    const body = JSON.stringify({ jsonrpc: "2.0", id: 1, method, params });
    const req = http.request(
      { host: u.hostname, port: u.port, path: u.pathname, method: "POST", headers: { "Content-Type": "application/json", Authorization: `Bearer ${lock.authToken}`, "Content-Length": Buffer.byteLength(body) } },
      (res) => {
        const chunks: Buffer[] = [];
        res.on("data", (c) => chunks.push(c));
        res.on("end", () => resolve(JSON.parse(Buffer.concat(chunks).toString("utf8"))));
      },
    );
    req.on("error", reject);
    req.end(body);
  });
}

const textOf = (r: { content: { type: string; text?: string }[] }) => r.content.filter((c) => c.type === "text").map((c) => c.text).join("\n");

export const tests: { name: string; fn: (api: Api) => Promise<void> }[] = [];
const it = (name: string, fn: (api: Api) => Promise<void>) => tests.push({ name, fn });

function lockOf(api: Api): { url: string; authToken: string; workspaceFolders: string[] } {
  assert.ok(api.lockFile, "the extension wrote a lock file");
  return JSON.parse(fs.readFileSync(api.lockFile as string, "utf8"));
}

async function tool(api: Api, name: string, args: Record<string, unknown> = {}) {
  const r = await rpc(lockOf(api), "tools/call", { name, arguments: args });
  assert.equal(r.error, undefined, JSON.stringify(r.error));
  return r.result as { content: { type: string; text?: string }[]; isError?: boolean };
}

const workspace = () => process.env.MARLOWE_TEST_WORKSPACE as string;

it("the extension serves on loopback and names the workspace in its lock file", async (api) => {
  const lock = lockOf(api);
  assert.match(lock.url, /^http:\/\/127\.0\.0\.1:\d+\/mcp$/);
  assert.equal(path.dirname(api.lockFile as string), path.join(process.env.MARLOWE_HOME as string, "ide"));
  assert.ok(lock.workspaceFolders.some((f) => f.toLowerCase() === workspace().toLowerCase()), JSON.stringify(lock.workspaceFolders));
  const list = await rpc(lock, "tools/list", {});
  assert.equal(list.result.tools.length, 6);
});

it("the real selection and open tabs are sent", async (api) => {
  const doc = await vscode.workspace.openTextDocument(path.join(workspace(), "main.py"));
  const ed = await vscode.window.showTextDocument(doc);
  ed.selection = new vscode.Selection(1, 0, 1, 5);
  const ctx = JSON.parse(textOf(await tool(api, "get_context")));
  assert.equal(ctx.selection.text, "y = 2");
  assert.equal(ctx.selection.startLine, 2);
  assert.ok(ctx.openEditors.some((e: { path: string }) => e.path.toLowerCase().endsWith("main.py")));
});

it("diagnostics from a language server are read", async (api) => {
  const file = vscode.Uri.file(path.join(workspace(), "main.py"));
  const coll = vscode.languages.createDiagnosticCollection("marlowe-test");
  coll.set(file, [new vscode.Diagnostic(new vscode.Range(2, 6, 2, 7), "x is shadowed", vscode.DiagnosticSeverity.Warning)]);
  try {
    const out = textOf(await tool(api, "get_diagnostics"));
    assert.match(out, /main\.py:3:7 warning .*x is shadowed/);
  } finally {
    coll.dispose();
  }
});

it("a diff opens in the editor as a diff tab and nothing is written", async (api) => {
  const target = path.join(workspace(), "main.py");
  const before = fs.readFileSync(target, "utf8");
  const out = await tool(api, "open_diff", { path: "main.py", new_contents: "x = 10\n", title: "Marlowe: test" });
  assert.match(textOf(out), /is open in the editor/);
  const diffs = vscode.window.tabGroups.all.flatMap((g) => g.tabs).filter((t) => t.input instanceof vscode.TabInputTextDiff);
  assert.equal(diffs.length, 1);
  const input = diffs[0].input as vscode.TabInputTextDiff;
  assert.equal(input.modified.scheme, "marlowe-proposed");
  const proposed = await vscode.workspace.openTextDocument(input.modified);
  assert.equal(proposed.getText(), "x = 10\n");
  assert.equal(fs.readFileSync(target, "utf8"), before);
  assert.match(textOf(await tool(api, "close_diffs")), /Closed 1 diff/);
});

it("execute_code without a notebook says so", async (api) => {
  await vscode.commands.executeCommand("workbench.action.closeAllEditors");
  const out = await tool(api, "execute_code", { code: "1+1" });
  assert.equal(out.isError, true);
  assert.match(textOf(out), /No notebook is open/);
});

it("execute_code runs 1+1 in a Jupyter kernel (only when the Jupyter extension is installed)", async (api) => {
  const jupyter = vscode.extensions.getExtension("ms-toolsai.jupyter");
  if (!jupyter) {
    console.log("     skipped: the Jupyter extension is not installed in this host");
    return;
  }
  const nb = await vscode.workspace.openNotebookDocument("jupyter-notebook", new vscode.NotebookData([new vscode.NotebookCellData(vscode.NotebookCellKind.Code, "1+1", "python")]));
  await vscode.window.showNotebookDocument(nb);
  await vscode.commands.executeCommand("notebook.cell.execute");
  const out = await tool(api, "execute_code", { code: "1+1" });
  assert.match(textOf(out), /\b2\b/);
});
