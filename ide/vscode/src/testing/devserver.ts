// A real bridge over a scripted editor, for the Rust side's interop test:
//   node out/src/testing/devserver.js <home> <workspace-folder>
// Prints one JSON line `{"lockFile":..., "port":...}` once it listens, and exits when stdin closes.

import { startBridge } from "../bridge";
import { FakeHost } from "./fakeHost";

async function main(): Promise<void> {
  const [home, folder] = process.argv.slice(2);
  if (!home || !folder) {
    process.stderr.write("usage: devserver <home> <workspace-folder>\n");
    process.exit(2);
  }
  const host = new FakeHost([folder]);
  const file = `${folder.replace(/[\\/]+$/, "")}${folder.includes("\\") ? "\\" : "/"}main.py`;
  host.ctx.selection = {
    path: file,
    language: "python",
    text: "x = 1",
    truncated: false,
    range: { start: { line: 4, character: 0 }, end: { line: 4, character: 5 } },
    empty: false,
  };
  host.ctx.openEditors = [{ path: file, language: "python", dirty: false, active: true }];
  host.problems = [
    {
      path: file,
      diagnostics: [{ severity: "error", message: "Name `y` is not defined", range: { start: { line: 6, character: 0 }, end: { line: 6, character: 1 } }, source: "pyright", code: "reportUndefinedVariable" }],
    },
  ];
  const bridge = await startBridge({ host, home, ideName: "Visual Studio Code", version: "test" });
  process.stdout.write(`${JSON.stringify({ lockFile: bridge.lockFile, port: bridge.port })}\n`);
  process.stdin.resume();
  process.stdin.on("end", () => void bridge.stop().then(() => process.exit(0)));
  process.on("SIGTERM", () => void bridge.stop().then(() => process.exit(0)));
}

void main();
