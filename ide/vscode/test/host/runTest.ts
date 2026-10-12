// Starts a real VS Code (an Extension Development Host) with this extension and a scratch
// workspace, and runs `suite/index.js` inside it. It opens a window, so it is run by hand:
//   npm run test:host
// The first run downloads a VS Code build into `.vscode-test/`.

import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { runTests } from "@vscode/test-electron";

async function main(): Promise<void> {
  const extensionDevelopmentPath = path.resolve(__dirname, "..", "..");
  const extensionTestsPath = path.resolve(__dirname, "suite", "index");
  const scratch = fs.mkdtempSync(path.join(os.tmpdir(), "marlowe-host-"));
  const workspace = path.join(scratch, "ws");
  const home = path.join(scratch, "home");
  fs.mkdirSync(workspace);
  fs.mkdirSync(home);
  fs.writeFileSync(path.join(workspace, "main.py"), "x = 1\ny = 2\nprint(x + y)\n");
  try {
    await runTests({
      extensionDevelopmentPath,
      extensionTestsPath,
      launchArgs: [workspace, "--disable-extensions", "--disable-workspace-trust"],
      extensionTestsEnv: { MARLOWE_HOME: home, MARLOWE_TEST_WORKSPACE: workspace },
    });
  } finally {
    fs.rmSync(scratch, { recursive: true, force: true });
  }
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
