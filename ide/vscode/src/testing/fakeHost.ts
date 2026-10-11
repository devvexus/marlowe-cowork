// A scripted editor for tests and for the Rust interop test (`devserver.ts`). Not shipped.

import type { DiffOutcome, DiffRequest, ExecRequest, ExecResult, FileDiagnostics, Host, IdeContext } from "../host";
import { runCode, type CellOutput, type Kernel } from "../jupyter";

/** A 1x1 PNG. */
export const TINY_PNG = Buffer.from(
  "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==",
  "base64",
);

const bytes = (s: string) => new TextEncoder().encode(s);

/** A kernel that understands a handful of snippets, enough to show the plumbing. */
export class FakeKernel implements Kernel {
  async *executeCode(code: string, token: { isCancellationRequested: boolean }): AsyncIterable<CellOutput> {
    const c = code.trim();
    if (c === "1+1") yield { items: [{ mime: "text/plain", data: bytes("2") }] };
    else if (c.startsWith("print(")) yield { items: [{ mime: "application/vnd.code.notebook.stdout", data: bytes(`${c.slice(6, -1).replace(/^["']|["']$/g, "")}\n`) }] };
    else if (c === "plot()")
      yield {
        items: [
          { mime: "text/plain", data: bytes("<Figure size 640x480 with 1 Axes>") },
          { mime: "image/png", data: new Uint8Array(TINY_PNG) },
        ],
      };
    else if (c === "boom()")
      yield { items: [{ mime: "application/vnd.code.notebook.error", data: bytes(JSON.stringify({ name: "ValueError", message: "bad", stack: "\u001b[31mValueError\u001b[0m: bad" })) }] };
    else if (c === "hang()") {
      while (!token.isCancellationRequested) await new Promise((r) => setTimeout(r, 10));
    } else yield { items: [{ mime: "application/vnd.code.notebook.stderr", data: bytes(`unknown snippet: ${c}\n`) }] };
  }
}

export class FakeHost implements Host {
  folders: string[];
  isTrusted = true;
  ctx: IdeContext;
  problems: FileDiagnostics[] = [];
  /** The user's answer to a waited diff; `"never"` leaves it open until the timeout. */
  diffAnswer: DiffOutcome["status"] | "never" = "accepted";
  opened: { path: string; line?: number; endLine?: number; preview: boolean }[] = [];
  diffs: { path: string; newContents: string; title: string; wait: boolean }[] = [];
  closed = 0;
  kernel: Kernel | undefined = new FakeKernel();
  kernelCancelled = 0;

  constructor(folders: string[]) {
    this.folders = folders;
    this.ctx = { workspaceFolders: folders, selection: null, openEditors: [] };
  }

  workspaceFolders(): string[] {
    return this.folders;
  }
  trusted(): boolean {
    return this.isTrusted;
  }
  context(): IdeContext {
    return { ...this.ctx, workspaceFolders: this.folders };
  }
  diagnostics(path?: string): FileDiagnostics[] {
    return path === undefined ? this.problems : this.problems.filter((p) => p.path.toLowerCase() === path.toLowerCase());
  }
  async openFile(path: string, opts: { line?: number; endLine?: number; preview: boolean }): Promise<void> {
    this.opened.push({ path, ...opts });
  }
  async openDiff(req: DiffRequest): Promise<DiffOutcome> {
    this.diffs.push({ path: req.path, newContents: req.newContents, title: req.title, wait: req.wait });
    if (!req.wait) return { status: "opened", newFile: false };
    if (this.diffAnswer !== "never") return { status: this.diffAnswer, newFile: false };
    return new Promise((resolve) => {
      const t = setTimeout(() => resolve({ status: "timeout", newFile: false }), req.timeoutMs);
      req.signal.addEventListener("abort", () => {
        clearTimeout(t);
        resolve({ status: "closed", newFile: false });
      });
    });
  }
  async closeDiffs(): Promise<number> {
    const n = this.diffs.length - this.closed;
    this.closed = this.diffs.length;
    return n;
  }
  async executeCode(req: ExecRequest): Promise<ExecResult> {
    if (!this.kernel) throw new Error("no running kernel");
    const outputs = await runCode(this.kernel, req.code, {
      timeoutMs: req.timeoutMs,
      signal: req.signal,
      makeToken: () => {
        const token = { isCancellationRequested: false, onCancellationRequested: () => ({ dispose() {} }) };
        return {
          token,
          cancel: () => {
            token.isCancellationRequested = true;
            this.kernelCancelled += 1;
          },
        };
      },
    });
    return { notebook: "fake.ipynb", outputs };
  }
}
