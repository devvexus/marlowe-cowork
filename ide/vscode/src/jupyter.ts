// Running code in a notebook's kernel through the Jupyter extension's API, and turning what the
// kernel prints into tool output. Nothing here imports `vscode`: the API objects arrive as the
// small interfaces below, so the unit tests drive them with fakes.

import type { ExecOutput } from "./host";

export interface OutputItem {
  mime: string;
  data: Uint8Array;
}

export interface CellOutput {
  items: OutputItem[];
}

export interface CancelToken {
  readonly isCancellationRequested: boolean;
  onCancellationRequested(listener: () => void): { dispose(): void };
}

export interface Kernel {
  executeCode(code: string, token: CancelToken): AsyncIterable<CellOutput>;
}

export const JUPYTER_EXTENSION = "ms-toolsai.jupyter";

const IMAGE_MIMES = ["image/png", "image/jpeg", "image/gif", "image/webp"];
const ANSI = /\u001b\[[0-9;?]*[ -/]*[@-~]/g;

export const stripAnsi = (s: string): string => s.replace(ANSI, "");

const utf8 = (b: Uint8Array): string => Buffer.from(b).toString("utf8");

/** One cell output to what the model gets. Images beat their text repr (`<Figure size ...>`). */
export function convertOutput(out: CellOutput): ExecOutput[] {
  const by = new Map<string, Uint8Array>();
  for (const i of out.items) by.set(i.mime.toLowerCase(), i.data);
  const result: ExecOutput[] = [];
  const image = IMAGE_MIMES.find((m) => by.has(m));
  const stdout = by.get("application/vnd.code.notebook.stdout");
  const stderr = by.get("application/vnd.code.notebook.stderr");
  const error = by.get("application/vnd.code.notebook.error");
  if (stdout) result.push({ kind: "text", stream: "stdout", text: stripAnsi(utf8(stdout)) });
  if (stderr) result.push({ kind: "text", stream: "stderr", text: stripAnsi(utf8(stderr)) });
  if (error) {
    let t = utf8(error);
    try {
      const e = JSON.parse(t) as { name?: string; message?: string; stack?: string };
      t = e.stack ? e.stack : `${e.name ?? "Error"}: ${e.message ?? ""}`;
    } catch {
      // not JSON: show it as it came
    }
    result.push({ kind: "text", stream: "error", text: stripAnsi(t) });
  }
  if (image) {
    result.push({ kind: "image", mime: image, data: Buffer.from(by.get(image) as Uint8Array).toString("base64") });
  } else if (by.has("text/plain")) {
    result.push({ kind: "text", stream: "result", text: stripAnsi(utf8(by.get("text/plain") as Uint8Array)) });
  } else if (by.has("text/html") || by.has("text/markdown")) {
    result.push({ kind: "text", stream: "result", text: "[a rich output with no plain-text form was left out]" });
  }
  return result;
}

export interface RunOpts {
  timeoutMs: number;
  signal: AbortSignal;
  /** Makes the token the kernel watches, and a function that cancels it. */
  makeToken(): { token: CancelToken; cancel(): void };
}

/**
 * Runs `code` and gathers every output. The call ends on the deadline or an abort even if the
 * kernel never finishes the stream; the kernel is asked to stop (interrupt) either way.
 */
export async function runCode(kernel: Kernel, code: string, o: RunOpts): Promise<ExecOutput[]> {
  const { token, cancel } = o.makeToken();
  const outputs: ExecOutput[] = [];
  let stop: (r: "timeout" | "aborted") => void = () => {};
  const stopped = new Promise<"timeout" | "aborted">((r) => (stop = r));
  const timer = setTimeout(() => {
    cancel();
    stop("timeout");
  }, o.timeoutMs);
  const onAbort = () => {
    cancel();
    stop("aborted");
  };
  if (o.signal.aborted) onAbort();
  else o.signal.addEventListener("abort", onAbort, { once: true });
  const consume = (async () => {
    for await (const out of kernel.executeCode(code, token)) outputs.push(...convertOutput(out));
    return "done" as const;
  })();
  consume.catch(() => {});
  try {
    const how = await Promise.race([consume, stopped]);
    if (how === "timeout") {
      outputs.push({ kind: "text", stream: "error", text: `[stopped after ${Math.round(o.timeoutMs / 1000)} s; the kernel was asked to interrupt. Check the notebook if it is still busy.]` });
    } else if (how === "aborted") {
      outputs.push({ kind: "text", stream: "error", text: "[cancelled; the kernel was asked to interrupt]" });
    }
    return outputs;
  } finally {
    clearTimeout(timer);
    o.signal.removeEventListener("abort", onAbort);
  }
}
