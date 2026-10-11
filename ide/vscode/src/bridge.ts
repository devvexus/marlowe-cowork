// Starts the MCP server for a `Host` and publishes it in the lock file.

import type { Host } from "./host";
import { LOCK_VERSION, removeLock, sweepStale, writeLock, type LockInfo } from "./lock";
import { CONSEQUENCES, McpServer } from "./mcp";
import { startServer, type RunningServer } from "./server";

export interface Bridge {
  readonly port: number;
  readonly url: string;
  readonly lockFile: string;
  /** Rewrites the lock file with the host's current workspace folders. */
  refresh(): void;
  stop(): Promise<void>;
}

export interface BridgeOpts {
  host: Host;
  home: string;
  ideName: string;
  version: string;
}

export async function startBridge(o: BridgeOpts): Promise<Bridge> {
  sweepStale(o.home);
  const mcp = new McpServer(o.host, { name: "marlowe-vscode", version: o.version });
  const server: RunningServer = await startServer(mcp);
  const started = new Date().toISOString();
  const info = (): LockInfo => ({
    version: LOCK_VERSION,
    pid: process.pid,
    ideName: o.ideName,
    transport: "http",
    url: server.url,
    authToken: server.token,
    workspaceFolders: o.host.workspaceFolders(),
    consequences: CONSEQUENCES,
    started,
  });
  let lockFile = "";
  try {
    lockFile = writeLock(o.home, server.port, info());
  } catch (e) {
    await server.close();
    throw e;
  }
  return {
    port: server.port,
    url: server.url,
    lockFile,
    refresh: () => {
      try {
        writeLock(o.home, server.port, info());
      } catch {
        // the profile folder went away; the next refresh retries
      }
    },
    stop: async () => {
      removeLock(o.home, server.port);
      await server.close();
    },
  };
}
