import * as os from "node:os";
import * as path from "node:path";

/** Marlowe's profile home, as `marlowe-proto::paths::cowork_home` resolves it. */
export function resolveHome(
  setting: string | undefined,
  env: NodeJS.ProcessEnv = process.env,
  platform: NodeJS.Platform = process.platform,
  homedir: string = os.homedir(),
): string {
  const pick = (v: string | undefined) => (v && v.trim() !== "" ? v.trim() : undefined);
  const set = pick(setting);
  if (set) return set;
  const fromEnv = pick(env.MARLOWE_HOME);
  if (fromEnv) return fromEnv;
  const local = pick(env.LOCALAPPDATA);
  if (local) return path.win32.join(local, "MarloweCowork");
  if (platform === "win32") return path.win32.join(homedir, "AppData", "Local", "MarloweCowork");
  return path.join(homedir, ".local", "share", "MarloweCowork");
}

const isWin = (p: string) => /^[a-zA-Z]:[\\/]/.test(p) || p.startsWith("\\\\");

/** A path the way Windows compares them: separators and case folded. Other systems compare exactly. */
export function normalize(p: string): string {
  if (isWin(p)) return path.win32.normalize(p).replace(/\\+$/, "").toLowerCase();
  return path.posix.normalize(p).replace(/\/+$/, "") || "/";
}

export function isAbsolute(p: string): boolean {
  return isWin(p) || p.startsWith("/");
}

/** Whether `child` is `root` or below it. */
export function isInside(root: string, child: string): boolean {
  const r = normalize(root);
  const c = normalize(child);
  if (c === r) return true;
  const sep = isWin(root) ? "\\" : "/";
  return c.startsWith(r.endsWith(sep) ? r : r + sep);
}

/**
 * Resolves a tool's `path` argument to an absolute path inside a workspace folder. A relative path
 * is taken from the first folder. Throws a message the model can act on.
 */
export function resolveInWorkspace(arg: unknown, folders: string[]): string {
  if (typeof arg !== "string" || arg.trim() === "") throw new Error("`path` is required");
  if (folders.length === 0) throw new Error("no folder is open in the editor");
  const joined = isAbsolute(arg) ? arg : (isWin(folders[0]) ? path.win32 : path.posix).join(folders[0], arg);
  const full = isWin(joined) ? path.win32.normalize(joined) : path.posix.normalize(joined);
  if (!folders.some((f) => isInside(f, full))) {
    throw new Error(`${arg} is outside the open workspace folders (${folders.join(", ")})`);
  }
  return full;
}
