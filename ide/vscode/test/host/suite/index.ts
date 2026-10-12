// The runner VS Code calls inside the Extension Development Host: runs each test in order and
// fails the whole run on the first failure report.

import * as vscode from "vscode";
import { tests } from "./extension.test";

export async function run(): Promise<void> {
  const ext = vscode.extensions.getExtension("marlowe.marlowe-vscode");
  if (!ext) throw new Error("the extension under test is not installed in the host");
  const api = (await ext.activate()) as { port: number | undefined; lockFile: string | undefined };
  const failures: string[] = [];
  for (const t of tests) {
    try {
      await t.fn(api);
      console.log(`ok   ${t.name}`);
    } catch (e) {
      failures.push(`${t.name}: ${e instanceof Error ? e.stack ?? e.message : String(e)}`);
      console.log(`FAIL ${t.name}`);
    }
  }
  if (failures.length > 0) throw new Error(`${failures.length} failed:\n${failures.join("\n\n")}`);
}
