// `VsCodeHost` against a stand-in for the parts of the VS Code API it touches. The real API is
// exercised by `npm run test:host` (an Extension Development Host); this runs anywhere.

import test from "node:test";
import assert from "node:assert/strict";
import { VsCodeHost, PROPOSED_SCHEME } from "../../src/vscodeHost";
import { SELECTION_CAP } from "../../src/host";
import { TINY_PNG } from "../../src/testing/fakeHost";

class Uri {
  constructor(
    readonly scheme: string,
    readonly path: string,
    readonly query = "",
  ) {}
  static file(p: string) {
    return new Uri("file", p);
  }
  static from(c: { scheme: string; path: string; query?: string }) {
    return new Uri(c.scheme, c.path, c.query ?? "");
  }
  get fsPath() {
    return this.path;
  }
  toString() {
    return `${this.scheme}:${this.path}?${this.query}`;
  }
}

class Range {
  start: { line: number; character: number };
  end: { line: number; character: number };
  constructor(sl: number, sc: number, el: number, ec: number) {
    this.start = { line: sl, character: sc };
    this.end = { line: el, character: ec };
  }
}

class TabInputText {
  constructor(readonly uri: Uri) {}
}
class TabInputTextDiff {
  constructor(
    readonly original: Uri,
    readonly modified: Uri,
  ) {}
}

class Emitter<T> {
  private ls: ((e: T) => void)[] = [];
  event = (l: (e: T) => void) => {
    this.ls.push(l);
    return { dispose: () => (this.ls = this.ls.filter((x) => x !== l)) };
  };
  fire(e: T) {
    for (const l of [...this.ls]) l(e);
  }
  dispose() {}
}

class TokenSource {
  token = { isCancellationRequested: false, onCancellationRequested: () => ({ dispose() {} }) };
  cancel() {
    this.token.isCancellationRequested = true;
  }
}

const S = { Error: 0, Warning: 1, Information: 2, Hint: 3 };

function doc(p: string, text: string, lang = "python") {
  const lines = text.split("\n");
  return {
    uri: Uri.file(p),
    languageId: lang,
    isClosed: false,
    lineCount: lines.length,
    lineAt: (n: number) => ({ range: { end: { character: lines[n].length } } }),
    getText: (r?: Range) => {
      if (!r) return text;
      const sel = lines.slice(r.start.line, r.end.line + 1);
      sel[sel.length - 1] = sel[sel.length - 1].slice(0, r.end.character);
      sel[0] = sel[0].slice(r.start.character);
      return sel.join("\n");
    },
  };
}

function build() {
  const state = {
    folders: [{ uri: Uri.file("C:\\ws") }] as { uri: Uri }[],
    trusted: true,
    active: undefined as any,
    tabs: [] as any[],
    docs: [] as any[],
    diagnostics: [] as [Uri, any[]][],
    existing: new Set<string>(),
    commands: [] as any[],
    infoChoice: undefined as string | undefined,
    notebook: undefined as any,
    jupyter: undefined as any,
    provider: undefined as any,
    shown: [] as any[],
  };
  const tabChange = new Emitter<void>();
  const activeChange = new Emitter<any>();
  const selChange = new Emitter<any>();
  const group = () => ({ get tabs() { return state.tabs; }, isActive: true });
  const api: any = {
    Uri,
    Range,
    TabInputText,
    TabInputTextDiff,
    DiagnosticSeverity: S,
    EventEmitter: Emitter,
    CancellationTokenSource: TokenSource,
    workspace: {
      get workspaceFolders() { return state.folders; },
      get isTrusted() { return state.trusted; },
      get textDocuments() { return state.docs; },
      registerTextDocumentContentProvider: (_s: string, p: unknown) => ((state.provider = p), { dispose() {} }),
      openTextDocument: async (u: Uri) => state.docs.find((d) => d.uri.path === u.path) ?? doc(u.path, "a\nb\nc\nd"),
      fs: { stat: async (u: Uri) => { if (!state.existing.has(u.path)) throw new Error("ENOENT"); return {}; } },
    },
    window: {
      get activeTextEditor() { return state.active; },
      get activeNotebookEditor() { return state.notebook; },
      get visibleNotebookEditors() { return state.notebook ? [state.notebook] : []; },
      onDidChangeActiveTextEditor: activeChange.event,
      onDidChangeTextEditorSelection: selChange.event,
      showTextDocument: async (d: any, o: any) => state.shown.push({ path: d.uri.path, ...o }),
      showInformationMessage: async () => state.infoChoice,
      tabGroups: {
        get all() { return [group()]; },
        onDidChangeTabs: tabChange.event,
        close: async (tabs: any[]) => {
          state.tabs = state.tabs.filter((t) => !tabs.includes(t));
          tabChange.fire();
        },
      },
    },
    languages: {
      getDiagnostics: (u?: Uri) => (u ? state.diagnostics.find(([x]) => x.path === u.path)?.[1] ?? [] : state.diagnostics),
    },
    commands: {
      executeCommand: async (...a: any[]) => {
        state.commands.push(a);
        if (a[0] === "vscode.diff") state.tabs.push({ input: new TabInputTextDiff(a[1], a[2]), isDirty: false, isActive: true });
      },
    },
    extensions: { getExtension: () => state.jupyter },
  };
  return { api, state, tabChange, activeChange, selChange };
}

test("the selection is the last focused file editor, cut at the cap", () => {
  const { api, state, selChange } = build();
  const host = new VsCodeHost(api);
  assert.equal(host.context().selection, null);
  const d = doc("C:\\ws\\a.py", "alpha\nbeta\ngamma");
  state.active = { document: d, selection: { isEmpty: false, start: { line: 0, character: 2 }, end: { line: 1, character: 2 } } };
  selChange.fire({ textEditor: state.active });
  const s = host.context().selection!;
  assert.equal(s.path, "C:\\ws\\a.py");
  assert.equal(s.text, "pha\nbe");
  assert.equal(s.empty, false);
  // The user moves to the terminal: no active editor, the last one is remembered.
  state.active = undefined;
  assert.equal(host.context().selection?.path, "C:\\ws\\a.py");
  // A cursor with nothing selected carries no text.
  const big = doc("C:\\ws\\big.py", "x".repeat(SELECTION_CAP + 50));
  const ed = { document: big, selection: { isEmpty: false, start: { line: 0, character: 0 }, end: { line: 0, character: SELECTION_CAP + 50 } } };
  selChange.fire({ textEditor: ed });
  const cut = host.context().selection!;
  assert.equal(cut.text.length, SELECTION_CAP);
  assert.equal(cut.truncated, true);
  const cursor = { document: d, selection: { isEmpty: true, start: { line: 2, character: 1 }, end: { line: 2, character: 1 } } };
  selChange.fire({ textEditor: cursor });
  assert.deepEqual([host.context().selection!.empty, host.context().selection!.text], [true, ""]);
  host.dispose();
});

test("only file: workspace folders and tabs are reported; dirty and active flags carry", () => {
  const { api, state } = build();
  state.folders.push({ uri: new Uri("vscode-remote", "/x") });
  state.docs.push(doc("C:\\ws\\a.py", "x"));
  state.tabs.push(
    { input: new TabInputText(Uri.file("C:\\ws\\a.py")), isDirty: true, isActive: true },
    { input: new TabInputText(new Uri("untitled", "Untitled-1")), isDirty: false, isActive: false },
    { input: {}, isDirty: false, isActive: false },
  );
  const host = new VsCodeHost(api);
  const c = host.context();
  assert.deepEqual(c.workspaceFolders, ["C:\\ws"]);
  assert.deepEqual(c.openEditors, [{ path: "C:\\ws\\a.py", language: "python", dirty: true, active: true }]);
  host.dispose();
});

test("diagnostics: hints are dropped, files with most errors come first, one file by path", () => {
  const { api, state } = build();
  const mk = (sev: number, msg: string, line: number, code?: unknown) => ({ severity: sev, message: msg, range: new Range(line, 0, line, 3), source: "ls", code });
  state.diagnostics.push(
    [Uri.file("C:\\ws\\warn.py"), [mk(S.Warning, "w", 3)]],
    [Uri.file("C:\\ws\\err.py"), [mk(S.Error, "late", 9, { value: "E9", target: {} }), mk(S.Error, "early", 1, 42), mk(S.Hint, "style", 0)]],
    [Uri.file("C:\\ws\\none.py"), [mk(S.Hint, "only a hint", 0)]],
    [new Uri("output", "log"), [mk(S.Error, "channel", 0)]],
  );
  const host = new VsCodeHost(api);
  const all = host.diagnostics();
  assert.deepEqual(all.map((f) => f.path), ["C:\\ws\\err.py", "C:\\ws\\warn.py"]);
  assert.deepEqual(all[0].diagnostics.map((d) => [d.message, d.severity, d.code]), [["early", "error", "42"], ["late", "error", "E9"]]);
  assert.deepEqual(host.diagnostics("C:\\ws\\warn.py").map((f) => f.path), ["C:\\ws\\warn.py"]);
  assert.deepEqual(host.diagnostics("C:\\ws\\missing.py"), []);
  host.dispose();
});

test("open_file selects the lines and clamps to the document", async () => {
  const { api, state } = build();
  const host = new VsCodeHost(api);
  await host.openFile("C:\\ws\\a.py", { line: 1, endLine: 2, preview: false });
  assert.equal(state.shown[0].preview, false);
  assert.deepEqual([state.shown[0].selection.start.line, state.shown[0].selection.end.line], [1, 2]);
  await host.openFile("C:\\ws\\a.py", { line: 99, preview: true });
  assert.equal(state.shown[1].selection.start.line, 3, "clamped to the last line");
  await host.openFile("C:\\ws\\a.py", { preview: true });
  assert.equal(state.shown[2].selection, undefined);
  host.dispose();
});

test("a diff compares the file with a virtual document holding the proposal", async () => {
  const { api, state } = build();
  state.existing.add("C:\\ws\\a.py");
  const host = new VsCodeHost(api);
  const out = await host.openDiff({ path: "C:\\ws\\a.py", newContents: "new text", title: "T", wait: false, timeoutMs: 1000, signal: new AbortController().signal });
  assert.deepEqual(out, { status: "opened", newFile: false });
  const [cmd, left, right, title] = state.commands[0];
  assert.equal(cmd, "vscode.diff");
  assert.equal(left.path, "C:\\ws\\a.py");
  assert.equal(right.scheme, PROPOSED_SCHEME);
  assert.equal(title, "T");
  assert.equal(state.provider.provideTextDocumentContent(right), "new text");
  // A file that does not exist yet diffs against an empty document.
  const created = await host.openDiff({ path: "C:\\ws\\new.py", newContents: "n", title: "T", wait: false, timeoutMs: 1000, signal: new AbortController().signal });
  assert.equal(created.newFile, true);
  const [, emptyLeft] = state.commands[1];
  assert.equal(emptyLeft.scheme, PROPOSED_SCHEME);
  assert.equal(state.provider.provideTextDocumentContent(emptyLeft), "");
  assert.equal(await host.closeDiffs(), 2);
  assert.equal(await host.closeDiffs(), 0);
  host.dispose();
});

test("a waited diff ends on Accept, Reject, dismissal, a closed tab or the deadline", async () => {
  for (const [choice, want] of [["Accept", "accepted"], ["Reject", "rejected"], [undefined, "closed"]] as const) {
    const { api, state } = build();
    state.infoChoice = choice;
    const host = new VsCodeHost(api);
    const out = await host.openDiff({ path: "C:\\ws\\a.py", newContents: "n", title: "T", wait: true, timeoutMs: 2000, signal: new AbortController().signal });
    assert.equal(out.status, want);
    assert.equal(state.tabs.length, 0, "the tab is closed once answered");
    host.dispose();
  }
  // The user closes the tab themselves while the question is up.
  {
    const { api, state } = build();
    api.window.showInformationMessage = () => new Promise(() => {});
    const host = new VsCodeHost(api);
    const p = host.openDiff({ path: "C:\\ws\\a.py", newContents: "n", title: "T", wait: true, timeoutMs: 5000, signal: new AbortController().signal });
    await new Promise((r) => setTimeout(r, 20));
    await api.window.tabGroups.close([...state.tabs]);
    assert.equal((await p).status, "closed");
    host.dispose();
  }
  // Nobody answers.
  {
    const { api, state } = build();
    api.window.showInformationMessage = () => new Promise(() => {});
    const host = new VsCodeHost(api);
    const out = await host.openDiff({ path: "C:\\ws\\a.py", newContents: "n", title: "T", wait: true, timeoutMs: 60, signal: new AbortController().signal });
    assert.equal(out.status, "timeout");
    assert.equal(state.tabs.length, 1, "the diff stays open");
    host.dispose();
  }
  // The request goes away (the turn was cancelled).
  {
    const { api } = build();
    api.window.showInformationMessage = () => new Promise(() => {});
    const host = new VsCodeHost(api);
    const ctl = new AbortController();
    const p = host.openDiff({ path: "C:\\ws\\a.py", newContents: "n", title: "T", wait: true, timeoutMs: 5000, signal: ctl.signal });
    setTimeout(() => ctl.abort(), 20);
    assert.equal((await p).status, "closed");
    host.dispose();
  }
});

test("execute_code finds the notebook's kernel through the Jupyter extension", async () => {
  const { api, state } = build();
  const host = new VsCodeHost(api);
  const req = (code: string) => ({ code, timeoutMs: 1000, signal: new AbortController().signal });
  await assert.rejects(host.executeCode(req("1")), /No notebook is open/);
  state.notebook = { notebook: { uri: Uri.file("C:\\ws\\n.ipynb") } };
  await assert.rejects(host.executeCode(req("1")), /Jupyter extension .* not installed/);
  state.jupyter = { isActive: true, exports: { kernels: { getKernel: async () => undefined } } };
  await assert.rejects(host.executeCode(req("1")), /no running kernel/);
  state.jupyter = { isActive: false, activate: async () => ({}), exports: undefined };
  await assert.rejects(host.executeCode(req("1")), /does not offer the kernel API/);
  const seen: string[] = [];
  const kernel = {
    async *executeCode(code: string) {
      seen.push(code);
      yield { items: [{ mime: "text/plain", data: new TextEncoder().encode("2") }] };
      yield { items: [{ mime: "image/png", data: new Uint8Array(TINY_PNG) }] };
    },
  };
  state.jupyter = { isActive: false, activate: async () => ({ kernels: { getKernel: async () => kernel } }), exports: undefined };
  const r = await host.executeCode(req("1+1"));
  assert.deepEqual(seen, ["1+1"]);
  assert.equal(r.notebook, "C:\\ws\\n.ipynb");
  assert.deepEqual(r.outputs.map((o) => o.kind), ["text", "image"]);
  host.dispose();
});
