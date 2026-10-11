// The connection file `<home>\ide\<port>.lock`. Marlowe reads it to find this editor: the URL and
// token of the loopback server, the workspace folders it serves, and the consequence each tool
// should carry. It is written atomically, removed when the extension stops, and a lock left by a
// process that died is swept at the next start.

import * as fs from "node:fs";
import * as path from "node:path";

export const LOCK_VERSION = 1;

export interface LockInfo {
  version: number;
  pid: number;
  ideName: string;
  transport: "http";
  url: string;
  authToken: string;
  workspaceFolders: string[];
  consequences: Record<string, string>;
  started: string;
}

export const ideDir = (home: string) => path.join(home, "ide");
export const lockPath = (home: string, port: number) => path.join(ideDir(home), `${port}.lock`);

export function writeLock(home: string, port: number, info: LockInfo): string {
  const dir = ideDir(home);
  fs.mkdirSync(dir, { recursive: true });
  const file = lockPath(home, port);
  const tmp = `${file}.${process.pid}.tmp`;
  fs.writeFileSync(tmp, JSON.stringify(info, null, 2), { encoding: "utf8", mode: 0o600 });
  fs.renameSync(tmp, file);
  return file;
}

export function removeLock(home: string, port: number): void {
  try {
    fs.unlinkSync(lockPath(home, port));
  } catch {
    // already gone
  }
}

export function processAlive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch (e) {
    // EPERM: the process exists but is not ours to signal.
    return (e as NodeJS.ErrnoException).code === "EPERM";
  }
}

/** Removes locks whose process is gone, and any lock file that is not valid JSON. Returns how many. */
export function sweepStale(home: string, alive: (pid: number) => boolean = processAlive): number {
  let removed = 0;
  let names: string[];
  try {
    names = fs.readdirSync(ideDir(home));
  } catch {
    return 0;
  }
  for (const n of names) {
    if (!n.endsWith(".lock")) continue;
    const file = path.join(ideDir(home), n);
    try {
      const info = JSON.parse(fs.readFileSync(file, "utf8")) as Partial<LockInfo>;
      if (typeof info.pid === "number" && alive(info.pid)) continue;
    } catch {
      // unreadable: treat as stale
    }
    try {
      fs.unlinkSync(file);
      removed += 1;
    } catch {
      // someone else removed it
    }
  }
  return removed;
}
