import * as vscode from "vscode";
import { startBridge, type Bridge } from "./bridge";
import { resolveHome } from "./paths";
import { VsCodeHost } from "./vscodeHost";

let host: VsCodeHost | undefined;
let bridge: Bridge | undefined;
let status: vscode.StatusBarItem | undefined;
let starting: Promise<void> = Promise.resolve();

function show(): void {
  if (!status) return;
  if (bridge) {
    status.text = "$(plug) Marlowe";
    status.tooltip = `Marlowe can reach this editor on 127.0.0.1:${bridge.port}`;
  } else {
    status.text = "$(debug-disconnect) Marlowe";
    status.tooltip = "Marlowe is not connected to this editor";
  }
  status.show();
}

async function stop(): Promise<void> {
  const b = bridge;
  bridge = undefined;
  if (b) await b.stop();
}

async function start(ctx: vscode.ExtensionContext): Promise<void> {
  await stop();
  const cfg = vscode.workspace.getConfiguration("marlowe");
  if (cfg.get<boolean>("enabled", true) && host) {
    try {
      bridge = await startBridge({
        host,
        home: resolveHome(cfg.get<string>("home")),
        ideName: vscode.env.appName,
        version: String(ctx.extension.packageJSON.version ?? "0"),
      });
    } catch (e) {
      void vscode.window.showWarningMessage(`Marlowe could not start its editor connection: ${e instanceof Error ? e.message : String(e)}`);
    }
  }
  show();
}

/** Runs restarts one after another, so two quick changes cannot interleave. */
function restart(ctx: vscode.ExtensionContext): Promise<void> {
  starting = starting.then(() => start(ctx));
  return starting;
}

export async function activate(ctx: vscode.ExtensionContext): Promise<{ port: number | undefined; lockFile: string | undefined }> {
  host = new VsCodeHost(vscode);
  status = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Right, 10);
  status.command = "marlowe.showConnection";
  ctx.subscriptions.push(
    host,
    status,
    vscode.commands.registerCommand("marlowe.showConnection", () => {
      const msg = bridge
        ? `Marlowe can reach this editor on 127.0.0.1:${bridge.port}. Connection file: ${bridge.lockFile}`
        : "Marlowe is not connected to this editor. Check the marlowe.enabled setting.";
      void vscode.window.showInformationMessage(msg);
    }),
    vscode.commands.registerCommand("marlowe.restart", () => restart(ctx)),
    vscode.commands.registerCommand("marlowe.closeDiffs", async () => {
      await host?.closeDiffs();
    }),
    vscode.workspace.onDidChangeWorkspaceFolders(() => bridge?.refresh()),
    vscode.workspace.onDidChangeConfiguration((e) => {
      if (e.affectsConfiguration("marlowe")) void restart(ctx);
    }),
  );
  await restart(ctx);
  // What the extension tests read to find the server; no secret in it.
  return {
    get port() {
      return bridge?.port;
    },
    get lockFile() {
      return bridge?.lockFile;
    },
  };
}

export async function deactivate(): Promise<void> {
  await starting.catch(() => {});
  await stop();
}
