// `Host` over the VS Code API. The API object is passed in (`new VsCodeHost(vscode)`) so the unit
// tests can hand it a faithful stand-in; this file imports only the types of "vscode".

import type * as vscode from "vscode";
import {
  SELECTION_CAP,
  type DiagnosticItem,
  type DiffOutcome,
  type DiffRequest,
  type EditorInfo,
  type ExecRequest,
  type ExecResult,
  type FileDiagnostics,
  type Host,
  type IdeContext,
  type Severity,
} from "./host";
import { JUPYTER_EXTENSION, runCode, type Kernel } from "./jupyter";

type Api = typeof vscode;

export const PROPOSED_SCHEME = "marlowe-proposed";
const MAX_FILES_WITH_PROBLEMS = 200;
const MAX_PROBLEMS_PER_FILE = 100;
const MAX_PROPOSALS_KEPT = 40;

interface JupyterApi {
  kernels: { getKernel(uri: vscode.Uri): Thenable<Kernel | undefined> };
}

export class VsCodeHost implements Host, vscode.TextDocumentContentProvider {
  private readonly proposals = new Map<string, string>();
  private nextProposal = 1;
  private last: vscode.TextEditor | undefined;
  private readonly subs: vscode.Disposable[] = [];
  private readonly changed: vscode.EventEmitter<vscode.Uri>;

  constructor(private readonly api: Api) {
    this.changed = new api.EventEmitter<vscode.Uri>();
    this.last = api.window.activeTextEditor;
    this.subs.push(
      api.workspace.registerTextDocumentContentProvider(PROPOSED_SCHEME, this),
      api.window.onDidChangeActiveTextEditor((e) => {
        if (e) this.last = e;
      }),
      api.window.onDidChangeTextEditorSelection((e) => {
        this.last = e.textEditor;
      }),
    );
  }

  dispose(): void {
    for (const s of this.subs) s.dispose();
    this.changed.dispose();
  }

  // ---- TextDocumentContentProvider: the proposed side of every diff
  get onDidChange(): vscode.Event<vscode.Uri> {
    return this.changed.event;
  }

  provideTextDocumentContent(uri: vscode.Uri): string {
    return this.proposals.get(uri.query) ?? "";
  }

  // ---- Host
  workspaceFolders(): string[] {
    return (this.api.workspace.workspaceFolders ?? []).filter((f) => f.uri.scheme === "file").map((f) => f.uri.fsPath);
  }

  trusted(): boolean {
    return this.api.workspace.isTrusted;
  }

  context(): IdeContext {
    return { workspaceFolders: this.workspaceFolders(), selection: this.selection(), openEditors: this.openEditors() };
  }

  private selection(): IdeContext["selection"] {
    const candidates = [this.last, this.api.window.activeTextEditor];
    const ed = candidates.find((e) => e !== undefined && !e.document.isClosed && e.document.uri.scheme === "file");
    if (!ed) return null;
    const sel = ed.selection;
    const full = sel.isEmpty ? "" : ed.document.getText(sel);
    const truncated = full.length > SELECTION_CAP;
    return {
      path: ed.document.uri.fsPath,
      language: ed.document.languageId,
      text: truncated ? full.slice(0, SELECTION_CAP) : full,
      truncated,
      range: { start: { line: sel.start.line, character: sel.start.character }, end: { line: sel.end.line, character: sel.end.character } },
      empty: sel.isEmpty,
    };
  }

  private openEditors(): EditorInfo[] {
    const { TabInputText } = this.api;
    const out: EditorInfo[] = [];
    for (const group of this.api.window.tabGroups.all) {
      for (const tab of group.tabs) {
        if (!(tab.input instanceof TabInputText) || tab.input.uri.scheme !== "file") continue;
        const uri = tab.input.uri;
        const doc = this.api.workspace.textDocuments.find((d) => d.uri.toString() === uri.toString());
        out.push({ path: uri.fsPath, language: doc?.languageId ?? "", dirty: tab.isDirty, active: tab.isActive && group.isActive });
      }
    }
    return out;
  }

  diagnostics(path?: string): FileDiagnostics[] {
    const api = this.api;
    const pairs: [vscode.Uri, readonly vscode.Diagnostic[]][] =
      path === undefined ? api.languages.getDiagnostics() : [[api.Uri.file(path), api.languages.getDiagnostics(api.Uri.file(path))]];
    const out: FileDiagnostics[] = [];
    for (const [uri, list] of pairs) {
      if (uri.scheme !== "file") continue;
      const items = list
        .map((d) => this.diagnostic(d))
        .filter((d): d is DiagnosticItem => d !== null)
        .sort((a, b) => a.range.start.line - b.range.start.line || a.range.start.character - b.range.start.character)
        .slice(0, MAX_PROBLEMS_PER_FILE);
      if (items.length > 0) out.push({ path: uri.fsPath, diagnostics: items });
    }
    const errors = (f: FileDiagnostics) => f.diagnostics.filter((d) => d.severity === "error").length;
    out.sort((a, b) => errors(b) - errors(a) || a.path.localeCompare(b.path));
    return out.slice(0, MAX_FILES_WITH_PROBLEMS);
  }

  private diagnostic(d: vscode.Diagnostic): DiagnosticItem | null {
    const S = this.api.DiagnosticSeverity;
    const severity: Severity | null =
      d.severity === S.Error ? "error" : d.severity === S.Warning ? "warning" : d.severity === S.Information ? "information" : null;
    if (severity === null) return null; // hints are the editor's styling, not problems
    const code = typeof d.code === "object" && d.code !== null ? String(d.code.value) : d.code === undefined ? undefined : String(d.code);
    return {
      severity,
      message: d.message,
      range: { start: { line: d.range.start.line, character: d.range.start.character }, end: { line: d.range.end.line, character: d.range.end.character } },
      source: d.source,
      code,
    };
  }

  async openFile(path: string, opts: { line?: number; endLine?: number; preview: boolean }): Promise<void> {
    const api = this.api;
    const doc = await api.workspace.openTextDocument(api.Uri.file(path));
    let selection: vscode.Range | undefined;
    if (opts.line !== undefined) {
      const last = Math.max(0, doc.lineCount - 1);
      const from = Math.min(opts.line, last);
      const to = Math.min(opts.endLine ?? opts.line, last);
      selection = new api.Range(from, 0, to, opts.endLine === undefined ? 0 : doc.lineAt(to).range.end.character);
    }
    await api.window.showTextDocument(doc, { preview: opts.preview, selection });
  }

  async openDiff(req: DiffRequest): Promise<DiffOutcome> {
    const api = this.api;
    const fileUri = api.Uri.file(req.path);
    let exists = true;
    try {
      await api.workspace.fs.stat(fileUri);
    } catch {
      exists = false;
    }
    const id = String(this.nextProposal++);
    const base = req.path.split(/[\\/]/).pop() ?? "file";
    this.proposals.set(id, req.newContents);
    this.proposals.set(`empty-${id}`, "");
    while (this.proposals.size > MAX_PROPOSALS_KEPT * 2) {
      const oldest = this.proposals.keys().next().value as string;
      this.proposals.delete(oldest);
    }
    const right = api.Uri.from({ scheme: PROPOSED_SCHEME, path: `/${base}`, query: id });
    const left = exists ? fileUri : api.Uri.from({ scheme: PROPOSED_SCHEME, path: `/${base}`, query: `empty-${id}` });
    await api.commands.executeCommand("vscode.diff", left, right, req.title, { preview: false });
    if (!req.wait) return { status: "opened", newFile: !exists };

    const answer = await this.waitForAnswer(right, base, req);
    if (answer !== "timeout") await this.closeTabsFor(right);
    return { status: answer, newFile: !exists };
  }

  private waitForAnswer(right: vscode.Uri, base: string, req: DiffRequest): Promise<"accepted" | "rejected" | "closed" | "timeout"> {
    const api = this.api;
    return new Promise((resolve) => {
      const subs: vscode.Disposable[] = [];
      let timer: ReturnType<typeof setTimeout> | undefined;
      const done = (r: "accepted" | "rejected" | "closed" | "timeout") => {
        if (timer) clearTimeout(timer);
        req.signal.removeEventListener("abort", onAbort);
        for (const s of subs) s.dispose();
        resolve(r);
      };
      const onAbort = () => done("closed");
      timer = setTimeout(() => done("timeout"), req.timeoutMs);
      if (req.signal.aborted) return done("closed");
      req.signal.addEventListener("abort", onAbort, { once: true });
      subs.push(
        api.window.tabGroups.onDidChangeTabs(() => {
          if (this.diffTabs(right).length === 0) done("closed");
        }),
      );
      void api.window.showInformationMessage(`Marlowe proposes a change to ${base}. Review the diff, then answer.`, "Accept", "Reject").then((choice) => {
        done(choice === "Accept" ? "accepted" : choice === "Reject" ? "rejected" : "closed");
      });
    });
  }

  private diffTabs(modified?: vscode.Uri): vscode.Tab[] {
    const { TabInputTextDiff } = this.api;
    const found: vscode.Tab[] = [];
    for (const g of this.api.window.tabGroups.all) {
      for (const t of g.tabs) {
        if (!(t.input instanceof TabInputTextDiff) || t.input.modified.scheme !== PROPOSED_SCHEME) continue;
        if (modified === undefined || t.input.modified.toString() === modified.toString()) found.push(t);
      }
    }
    return found;
  }

  private async closeTabsFor(right: vscode.Uri): Promise<void> {
    const tabs = this.diffTabs(right);
    if (tabs.length > 0) await this.api.window.tabGroups.close(tabs);
    this.proposals.delete(right.query);
    this.proposals.delete(`empty-${right.query}`);
  }

  async closeDiffs(): Promise<number> {
    const tabs = this.diffTabs();
    if (tabs.length > 0) await this.api.window.tabGroups.close(tabs);
    this.proposals.clear();
    return tabs.length;
  }

  async executeCode(req: ExecRequest): Promise<ExecResult> {
    const api = this.api;
    const notebook = api.window.activeNotebookEditor?.notebook ?? api.window.visibleNotebookEditors[0]?.notebook;
    if (!notebook) throw new Error("No notebook is open in the editor. Ask the user to open one and run a cell, so its kernel starts.");
    const ext = api.extensions.getExtension<JupyterApi>(JUPYTER_EXTENSION);
    if (!ext) throw new Error(`The Jupyter extension (${JUPYTER_EXTENSION}) is not installed in this editor.`);
    const jupyter = ext.isActive ? ext.exports : await ext.activate();
    if (!jupyter?.kernels?.getKernel) throw new Error("This version of the Jupyter extension does not offer the kernel API. Update it.");
    const kernel = await jupyter.kernels.getKernel(notebook.uri);
    if (!kernel) throw new Error(`${notebook.uri.fsPath} has no running kernel. Ask the user to run a cell in it, or pick a kernel.`);
    const outputs = await runCode(kernel, req.code, {
      timeoutMs: req.timeoutMs,
      signal: req.signal,
      makeToken: () => {
        const src = new api.CancellationTokenSource();
        return { token: src.token, cancel: () => src.cancel() };
      },
    });
    return { notebook: notebook.uri.fsPath, outputs };
  }
}
