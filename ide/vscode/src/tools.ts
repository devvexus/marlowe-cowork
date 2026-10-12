// The tools Marlowe sees as `mcp__ide__<name>`. Each takes validated arguments, asks the `Host`,
// and answers in MCP result content. Names are snake_case like Marlowe's own tools.

import type { DiagnosticItem, Host } from "./host";
import { resolveInWorkspace } from "./paths";

export type Content = { type: "text"; text: string } | { type: "image"; data: string; mimeType: string };

export interface ToolResult {
  content: Content[];
  isError?: boolean;
}

export interface ToolDef {
  name: string;
  description: string;
  inputSchema: Record<string, unknown>;
  /** Read-only tools carry the hint so a client may run them without asking. */
  readOnly: boolean;
  run(args: Record<string, unknown>, host: Host, signal: AbortSignal): Promise<ToolResult>;
}

const text = (t: string): ToolResult => ({ content: [{ type: "text", text: t }] });
const fail = (t: string): ToolResult => ({ content: [{ type: "text", text: t }], isError: true });

/** Bounds on what one answer carries. */
export const MAX_DIAGNOSTIC_LINES = 300;
export const MAX_OUTPUT_CHARS = 100_000;
export const MAX_IMAGES = 8;
export const MAX_IMAGE_BASE64 = 6_000_000;
const MAX_CODE_CHARS = 100_000;
const MAX_DIFF_CHARS = 2_000_000;

function str(args: Record<string, unknown>, key: string, required = false): string | undefined {
  const v = args[key];
  if (v === undefined || v === null) {
    if (required) throw new Error(`\`${key}\` is required`);
    return undefined;
  }
  if (typeof v !== "string") throw new Error(`\`${key}\` must be a string`);
  return v;
}

function int(args: Record<string, unknown>, key: string, min: number, max: number): number | undefined {
  const v = args[key];
  if (v === undefined || v === null) return undefined;
  if (typeof v !== "number" || !Number.isInteger(v) || v < min || v > max) {
    throw new Error(`\`${key}\` must be a whole number from ${min} to ${max}`);
  }
  return v;
}

function bool(args: Record<string, unknown>, key: string): boolean | undefined {
  const v = args[key];
  if (v === undefined || v === null) return undefined;
  if (typeof v !== "boolean") throw new Error(`\`${key}\` must be true or false`);
  return v;
}

export function diagnosticLine(path: string, d: DiagnosticItem): string {
  const where = `${path}:${d.range.start.line + 1}:${d.range.start.character + 1}`;
  const tag = [d.source, d.code].filter((x) => x !== undefined && x !== "").join(" ");
  return `${where} ${d.severity}${tag ? ` [${tag}]` : ""} ${d.message.replace(/\s*\n\s*/g, " ")}`;
}

const getContext: ToolDef = {
  name: "get_context",
  description:
    "What the user has in the editor: the workspace folders, the file they last focused with its selected text (1-based lines), and the open tabs.",
  inputSchema: { type: "object", properties: {}, additionalProperties: false },
  readOnly: true,
  async run(_args, host) {
    const c = host.context();
    const s = c.selection;
    const out = {
      workspaceFolders: c.workspaceFolders,
      selection: s && {
        path: s.path,
        language: s.language,
        empty: s.empty,
        text: s.text,
        truncated: s.truncated,
        startLine: s.range.start.line + 1,
        startColumn: s.range.start.character + 1,
        endLine: s.range.end.line + 1,
        endColumn: s.range.end.character + 1,
      },
      openEditors: c.openEditors,
    };
    return text(JSON.stringify(out));
  },
};

const getDiagnostics: ToolDef = {
  name: "get_diagnostics",
  description:
    "Problems the editor's language servers report (errors, warnings), one per line as `path:line:col severity [source code] message`. Give `path` for one file, or none for every file with problems.",
  inputSchema: {
    type: "object",
    properties: { path: { type: "string", description: "absolute, or relative to the first workspace folder" } },
    additionalProperties: false,
  },
  readOnly: true,
  async run(args, host) {
    const p = str(args, "path");
    const path = p === undefined ? undefined : resolveInWorkspace(p, host.workspaceFolders());
    const files = host.diagnostics(path);
    const lines: string[] = [];
    let total = 0;
    for (const f of files) {
      for (const d of f.diagnostics) {
        total += 1;
        if (lines.length < MAX_DIAGNOSTIC_LINES) lines.push(diagnosticLine(f.path, d));
      }
    }
    if (total === 0) return text(path ? `No problems reported for ${path}.` : "No problems reported.");
    if (total > lines.length) lines.push(`[${total - lines.length} more problems not shown; ask for one file with \`path\`]`);
    return text(lines.join("\n"));
  },
};

const openFile: ToolDef = {
  name: "open_file",
  description: "Show a file in the editor, optionally selecting lines (1-based, inclusive). Changes nothing on disk.",
  inputSchema: {
    type: "object",
    properties: {
      path: { type: "string" },
      line: { type: "integer", minimum: 1 },
      end_line: { type: "integer", minimum: 1 },
      preview: { type: "boolean", description: "open as a preview tab (default true)" },
    },
    required: ["path"],
    additionalProperties: false,
  },
  readOnly: false,
  async run(args, host) {
    const path = resolveInWorkspace(str(args, "path", true), host.workspaceFolders());
    const line = int(args, "line", 1, 10_000_000);
    const endLine = int(args, "end_line", 1, 10_000_000);
    if (line !== undefined && endLine !== undefined && endLine < line) throw new Error("`end_line` is before `line`");
    await host.openFile(path, { line: line === undefined ? undefined : line - 1, endLine: endLine === undefined ? undefined : endLine - 1, preview: bool(args, "preview") ?? true });
    return text(`Opened ${path}${line ? ` at line ${line}${endLine && endLine !== line ? `-${endLine}` : ""}` : ""}.`);
  },
};

const openDiff: ToolDef = {
  name: "open_diff",
  description:
    "Show the user a diff of a file against `new_contents` in the editor (the file is not written). With `wait`, block until they accept or reject it (default 300 s) and report which.",
  inputSchema: {
    type: "object",
    properties: {
      path: { type: "string" },
      new_contents: { type: "string", description: "the whole proposed file" },
      title: { type: "string" },
      wait: { type: "boolean", description: "wait for the user's answer (default false)" },
      timeout_seconds: { type: "integer", minimum: 5, maximum: 900 },
    },
    required: ["path", "new_contents"],
    additionalProperties: false,
  },
  readOnly: false,
  async run(args, host, signal) {
    if (!host.trusted()) return fail("The workspace is not trusted, so Marlowe cannot open diffs against its files. Trust the workspace in VS Code first.");
    const path = resolveInWorkspace(str(args, "path", true), host.workspaceFolders());
    const contents = str(args, "new_contents", true) as string;
    if (contents.length > MAX_DIFF_CHARS) throw new Error(`\`new_contents\` is over ${MAX_DIFF_CHARS} characters`);
    const wait = bool(args, "wait") ?? false;
    const timeout = int(args, "timeout_seconds", 5, 900) ?? 300;
    const out = await host.openDiff({
      path,
      newContents: contents,
      title: str(args, "title") ?? `Marlowe: ${path.split(/[\\/]/).pop()}`,
      wait,
      timeoutMs: timeout * 1000,
      signal,
    });
    const what = out.newFile ? `new file ${path}` : path;
    switch (out.status) {
      case "opened":
        return text(`The diff of ${what} is open in the editor.`);
      case "accepted":
        return text(`The user accepted the change to ${what}. Nothing was written; apply it yourself.`);
      case "rejected":
        return text(`The user rejected the change to ${what}.`);
      case "closed":
        return text(`The user closed the diff of ${what} without answering.`);
      case "timeout":
        return text(`No answer about ${what} within ${timeout} s; the diff stays open.`);
    }
  },
};

const closeDiffs: ToolDef = {
  name: "close_diffs",
  description: "Close the diffs Marlowe opened in the editor.",
  inputSchema: { type: "object", properties: {}, additionalProperties: false },
  readOnly: false,
  async run(_args, host) {
    const n = await host.closeDiffs();
    return text(n === 0 ? "No Marlowe diffs were open." : `Closed ${n} diff${n === 1 ? "" : "s"}.`);
  },
};

const executeCode: ToolDef = {
  name: "execute_code",
  description:
    "Run Python (or the notebook's language) in the running kernel of the notebook the user has open in the editor. The kernel keeps its variables, so the code sees what earlier cells defined. Returns the text output and any images. Runs code on the user's machine.",
  inputSchema: {
    type: "object",
    properties: {
      code: { type: "string" },
      timeout_seconds: { type: "integer", minimum: 1, maximum: 900, description: "default 120" },
    },
    required: ["code"],
    additionalProperties: false,
  },
  readOnly: false,
  async run(args, host, signal) {
    if (!host.trusted()) return fail("The workspace is not trusted, so Marlowe cannot run code in a notebook kernel. Trust the workspace in VS Code first.");
    const code = str(args, "code", true) as string;
    if (code.trim() === "") throw new Error("`code` is empty");
    if (code.length > MAX_CODE_CHARS) throw new Error(`\`code\` is over ${MAX_CODE_CHARS} characters`);
    const timeout = int(args, "timeout_seconds", 1, 900) ?? 120;
    const r = await host.executeCode({ code, timeoutMs: timeout * 1000, signal });
    const content: Content[] = [];
    let chars = 0;
    let images = 0;
    let dropped = 0;
    for (const o of r.outputs) {
      if (o.kind === "text") {
        if (o.text === "") continue;
        const room = MAX_OUTPUT_CHARS - chars;
        if (room <= 0) {
          dropped += 1;
          continue;
        }
        const t = o.text.length > room ? `${o.text.slice(0, room)}\n[output cut at ${MAX_OUTPUT_CHARS} characters]` : o.text;
        chars += t.length;
        content.push({ type: "text", text: o.stream === "stderr" ? `[stderr]\n${t}` : o.stream === "error" ? `[error]\n${t}` : t });
      } else if (images >= MAX_IMAGES || o.data.length > MAX_IMAGE_BASE64) {
        dropped += 1;
      } else {
        images += 1;
        content.push({ type: "image", data: o.data, mimeType: o.mime });
      }
    }
    if (dropped > 0) content.push({ type: "text", text: `[${dropped} output${dropped === 1 ? "" : "s"} left out: too many or too large]` });
    if (content.length === 0) content.push({ type: "text", text: "(the code ran and printed nothing)" });
    const failed = r.outputs.some((o) => o.kind === "text" && o.stream === "error");
    return { content, isError: failed || undefined };
  },
};

export const TOOLS: ToolDef[] = [getContext, getDiagnostics, openFile, openDiff, closeDiffs, executeCode];

/**
 * The consequence a Marlowe client should give each tool (`inert`, `reversible`, `irreversible`):
 * published in the lock file so the connecting side does not have to guess. Reads are inert,
 * showing something in the editor is reversible, and running code asks.
 */
export const CONSEQUENCES: Record<string, "inert" | "reversible" | "irreversible"> = {
  get_context: "inert",
  get_diagnostics: "inert",
  open_file: "reversible",
  open_diff: "reversible",
  close_diffs: "inert",
  execute_code: "irreversible",
};
