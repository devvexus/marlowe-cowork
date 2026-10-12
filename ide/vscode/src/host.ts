// What the tools need from an editor. `vscodeHost.ts` implements it over the VS Code API; the unit
// tests and the Rust interop test implement it with fakes, so the protocol, the tools and the
// lock file are exercised without a window.

export interface Position {
  line: number; // 0-based, as VS Code counts
  character: number;
}

export interface Range {
  start: Position;
  end: Position;
}

export type Severity = "error" | "warning" | "information" | "hint";

export interface DiagnosticItem {
  severity: Severity;
  message: string;
  range: Range;
  source?: string;
  code?: string;
}

export interface FileDiagnostics {
  path: string;
  diagnostics: DiagnosticItem[];
}

export interface EditorInfo {
  path: string;
  language: string;
  dirty: boolean;
  active: boolean;
}

export interface SelectionInfo {
  path: string;
  language: string;
  /** The selected text, cut at `SELECTION_CAP` characters. */
  text: string;
  truncated: boolean;
  range: Range;
  /** No characters selected: only a cursor. */
  empty: boolean;
}

export interface IdeContext {
  workspaceFolders: string[];
  /** The last text editor the user focused, with its selection or cursor. */
  selection: SelectionInfo | null;
  openEditors: EditorInfo[];
}

export interface DiffRequest {
  path: string;
  newContents: string;
  title: string;
  /** Wait for the user to accept or reject. */
  wait: boolean;
  timeoutMs: number;
  signal: AbortSignal;
}

/** `opened`: shown, not waited for. The others are the user's answer to a waited diff. */
export type DiffStatus = "opened" | "accepted" | "rejected" | "closed" | "timeout";

export interface DiffOutcome {
  status: DiffStatus;
  /** The file did not exist: the diff is against an empty file. */
  newFile: boolean;
}

export type ExecOutput =
  | { kind: "text"; stream: "stdout" | "stderr" | "result" | "error"; text: string }
  | { kind: "image"; mime: string; data: string /* base64 */ };

export interface ExecRequest {
  code: string;
  timeoutMs: number;
  signal: AbortSignal;
}

export interface ExecResult {
  /** The notebook the code ran in. */
  notebook: string;
  outputs: ExecOutput[];
}

export const SELECTION_CAP = 20_000;

export interface Host {
  workspaceFolders(): string[];
  /** The workspace is trusted: diffs against files and code execution are allowed. */
  trusted(): boolean;
  context(): IdeContext;
  /** All files with diagnostics, or only `path`'s. */
  diagnostics(path?: string): FileDiagnostics[];
  openFile(path: string, opts: { line?: number; endLine?: number; preview: boolean }): Promise<void>;
  openDiff(req: DiffRequest): Promise<DiffOutcome>;
  /** Closes the diff tabs Marlowe opened; returns how many. */
  closeDiffs(): Promise<number>;
  /** Runs `code` in the active notebook's running kernel. Throws a plain Error the model can act on. */
  executeCode(req: ExecRequest): Promise<ExecResult>;
}
