// Shared pieces of the unit tests: a temp home, a bridge over a scripted editor, and a tiny MCP client.

import * as fs from "node:fs";
import * as http from "node:http";
import * as os from "node:os";
import * as path from "node:path";
import { startBridge, type Bridge } from "../../src/bridge";
import { FakeHost } from "../../src/testing/fakeHost";
import type { LockInfo } from "../../src/lock";

export function tempDir(tag: string): string {
  return fs.mkdtempSync(path.join(os.tmpdir(), `marlowe-vscode-${tag}-`));
}

export interface Rig {
  host: FakeHost;
  bridge: Bridge;
  home: string;
  folder: string;
  lock: LockInfo;
  rpc(method: string, params?: unknown, id?: number): Promise<any>;
  call(name: string, args?: Record<string, unknown>): Promise<{ content: any[]; isError?: boolean }>;
  stop(): Promise<void>;
}

/** What Marlowe does: read the lock file, then POST JSON-RPC with the bearer token. */
export function post(lock: Pick<LockInfo, "url" | "authToken">, body: unknown, headers: Record<string, string> = {}): Promise<{ status: number; body: string; headers: http.IncomingHttpHeaders }> {
  return new Promise((resolve, reject) => {
    const u = new URL(lock.url);
    const payload = typeof body === "string" ? body : JSON.stringify(body);
    const req = http.request(
      { host: u.hostname, port: u.port, path: u.pathname, method: "POST", headers: { "Content-Type": "application/json", Authorization: `Bearer ${lock.authToken}`, "Content-Length": Buffer.byteLength(payload), ...headers } },
      (res) => {
        const chunks: Buffer[] = [];
        res.on("data", (c) => chunks.push(c));
        res.on("end", () => resolve({ status: res.statusCode ?? 0, body: Buffer.concat(chunks).toString("utf8"), headers: res.headers }));
      },
    );
    req.on("error", reject);
    req.end(payload);
  });
}

export async function rig(): Promise<Rig> {
  const home = tempDir("home");
  const folder = tempDir("ws");
  const host = new FakeHost([folder]);
  const bridge = await startBridge({ host, home, ideName: "Visual Studio Code", version: "test" });
  const lock = JSON.parse(fs.readFileSync(bridge.lockFile, "utf8")) as LockInfo;
  let next = 1;
  const rpc = async (method: string, params?: unknown, id = next++) => {
    const r = await post(lock, { jsonrpc: "2.0", id, method, params });
    return JSON.parse(r.body);
  };
  return {
    host,
    bridge,
    home,
    folder,
    lock,
    rpc,
    call: async (name, args = {}) => {
      const r = await rpc("tools/call", { name, arguments: args });
      if (r.error) throw new Error(`rpc error ${r.error.code}: ${r.error.message}`);
      return r.result;
    },
    stop: async () => {
      await bridge.stop();
      fs.rmSync(home, { recursive: true, force: true });
      fs.rmSync(folder, { recursive: true, force: true });
    },
  };
}

export const textOf = (r: { content: any[] }): string => r.content.filter((c) => c.type === "text").map((c) => c.text).join("\n");
