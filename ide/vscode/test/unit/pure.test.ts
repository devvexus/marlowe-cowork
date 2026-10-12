// Paths, lock files and kernel-output conversion: the parts with no editor and no network.

import test from "node:test";
import assert from "node:assert/strict";
import * as fs from "node:fs";
import * as path from "node:path";
import { isInside, normalize, resolveHome, resolveInWorkspace } from "../../src/paths";
import { LOCK_VERSION, lockPath, removeLock, sweepStale, writeLock, type LockInfo } from "../../src/lock";
import { convertOutput, runCode, stripAnsi, type CellOutput } from "../../src/jupyter";
import { tempDir } from "./helpers";

const bytes = (s: string) => new TextEncoder().encode(s);

test("the profile home resolves like marlowe-proto: setting, MARLOWE_HOME, LOCALAPPDATA", () => {
  assert.equal(resolveHome("D:\\m", { MARLOWE_HOME: "X" }, "win32"), "D:\\m");
  assert.equal(resolveHome("", { MARLOWE_HOME: "E:\\home" }, "win32"), "E:\\home");
  assert.equal(resolveHome(undefined, { LOCALAPPDATA: "C:\\Users\\m\\AppData\\Local" }, "win32"), "C:\\Users\\m\\AppData\\Local\\MarloweCowork");
  assert.equal(resolveHome("  ", {}, "win32", "C:\\Users\\m"), "C:\\Users\\m\\AppData\\Local\\MarloweCowork");
  assert.equal(resolveHome(undefined, {}, "linux", "/home/m"), path.join("/home/m", ".local", "share", "MarloweCowork"));
});

test("Windows paths compare with separators and case folded; others exactly", () => {
  assert.equal(normalize("C:/Work/Proj/"), "c:\\work\\proj");
  assert.ok(isInside("C:\\Work\\Proj", "c:/work/proj/src/a.py"));
  assert.ok(isInside("C:\\Work\\Proj", "C:\\Work\\Proj"));
  assert.ok(!isInside("C:\\Work\\Proj", "C:\\Work\\Project\\a.py"), "a sibling with a shared prefix is outside");
  assert.ok(!isInside("/w/p", "/w/pp/a"));
  assert.ok(isInside("/w/p", "/w/p/a"));
});

test("a tool path resolves from the first folder and cannot leave the workspace", () => {
  const folders = ["C:\\Work\\Proj"];
  assert.equal(resolveInWorkspace("src\\a.py", folders), "C:\\Work\\Proj\\src\\a.py");
  assert.equal(resolveInWorkspace("c:/work/proj/b.py", folders), "c:\\work\\proj\\b.py");
  assert.throws(() => resolveInWorkspace("..\\Other\\a.py", folders), /outside the open workspace/);
  assert.throws(() => resolveInWorkspace("D:\\x.py", folders), /outside the open workspace/);
  assert.throws(() => resolveInWorkspace("a.py", []), /no folder is open/);
  assert.throws(() => resolveInWorkspace("  ", folders), /required/);
  assert.throws(() => resolveInWorkspace(undefined, folders), /required/);
  // The second of several folders is reachable by absolute path.
  assert.equal(resolveInWorkspace("E:\\Two\\a.py", ["C:\\One", "E:\\Two"]), "E:\\Two\\a.py");
});

const info = (pid: number): LockInfo => ({
  version: LOCK_VERSION,
  pid,
  ideName: "x",
  transport: "http",
  url: "http://127.0.0.1:1/mcp",
  authToken: "t",
  workspaceFolders: [],
  consequences: {},
  started: "2026-01-01T00:00:00Z",
});

test("a lock is written atomically, replaced in place and removed", () => {
  const home = tempDir("lock");
  const file = writeLock(home, 4242, info(1));
  assert.equal(file, lockPath(home, 4242));
  assert.equal(JSON.parse(fs.readFileSync(file, "utf8")).pid, 1);
  writeLock(home, 4242, info(2));
  assert.equal(JSON.parse(fs.readFileSync(file, "utf8")).pid, 2);
  assert.deepEqual(fs.readdirSync(path.dirname(file)), ["4242.lock"], "no temp file is left behind");
  removeLock(home, 4242);
  removeLock(home, 4242);
  assert.equal(fs.existsSync(file), false);
  fs.rmSync(home, { recursive: true, force: true });
});

test("the sweep removes dead and unreadable locks and keeps live ones", () => {
  const home = tempDir("sweep");
  writeLock(home, 1, info(111));
  writeLock(home, 2, info(222));
  fs.writeFileSync(path.join(home, "ide", "3.lock"), "{truncated");
  fs.writeFileSync(path.join(home, "ide", "notes.txt"), "keep");
  const removed = sweepStale(home, (pid) => pid === 111);
  assert.equal(removed, 2);
  assert.deepEqual(fs.readdirSync(path.join(home, "ide")).sort(), ["1.lock", "notes.txt"]);
  assert.equal(sweepStale(path.join(home, "missing")), 0);
  fs.rmSync(home, { recursive: true, force: true });
});

test("kernel output: stdout and stderr keep their stream, ANSI is stripped", () => {
  const out = convertOutput({
    items: [
      { mime: "application/vnd.code.notebook.stdout", data: bytes("\u001b[1mhi\u001b[0m\n") },
      { mime: "application/vnd.code.notebook.stderr", data: bytes("warn\n") },
    ],
  });
  assert.deepEqual(out, [
    { kind: "text", stream: "stdout", text: "hi\n" },
    { kind: "text", stream: "stderr", text: "warn\n" },
  ]);
  assert.equal(stripAnsi("\u001b[31;1mred\u001b[0m"), "red");
});

test("kernel output: an image wins over its text repr; rich output with no text is named", () => {
  const png = convertOutput({ items: [{ mime: "text/plain", data: bytes("<Figure>") }, { mime: "image/png", data: new Uint8Array([1, 2, 3]) }] });
  assert.deepEqual(png, [{ kind: "image", mime: "image/png", data: "AQID" }]);
  const html = convertOutput({ items: [{ mime: "text/html", data: bytes("<table></table>") }] });
  assert.match((html[0] as { text: string }).text, /rich output/);
  const both = convertOutput({ items: [{ mime: "text/html", data: bytes("<b>x</b>") }, { mime: "text/plain", data: bytes("x") }] });
  assert.deepEqual(both, [{ kind: "text", stream: "result", text: "x" }]);
});

test("kernel output: an error without a stack shows name and message", () => {
  const out = convertOutput({ items: [{ mime: "application/vnd.code.notebook.error", data: bytes(JSON.stringify({ name: "KeyError", message: "'a'" })) }] });
  assert.deepEqual(out, [{ kind: "text", stream: "error", text: "KeyError: 'a'" }]);
  const raw = convertOutput({ items: [{ mime: "application/vnd.code.notebook.error", data: bytes("plain text") }] });
  assert.deepEqual(raw, [{ kind: "text", stream: "error", text: "plain text" }]);
});

test("runCode gathers outputs and stops a kernel that never ends at the deadline", async () => {
  let cancelled = 0;
  const token = { isCancellationRequested: false, onCancellationRequested: () => ({ dispose() {} }) };
  const make = () => ({ token, cancel: () => ((token.isCancellationRequested = true), (cancelled += 1)) });
  const quick = {
    async *executeCode(): AsyncIterable<CellOutput> {
      yield { items: [{ mime: "text/plain", data: bytes("1") }] };
      yield { items: [{ mime: "text/plain", data: bytes("2") }] };
    },
  };
  const ok = await runCode(quick, "x", { timeoutMs: 1000, signal: new AbortController().signal, makeToken: make });
  assert.equal(ok.length, 2);
  assert.equal(cancelled, 0);
  const stuck = {
    async *executeCode(): AsyncIterable<CellOutput> {
      yield { items: [{ mime: "text/plain", data: bytes("partial") }] };
      await new Promise(() => {});
    },
  };
  const t = await runCode(stuck, "x", { timeoutMs: 100, signal: new AbortController().signal, makeToken: make });
  assert.equal(cancelled, 1);
  assert.equal((t[0] as { text: string }).text, "partial", "what arrived before the deadline is kept");
  assert.match((t[1] as { text: string }).text, /stopped after/);
});
