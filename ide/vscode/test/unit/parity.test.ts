// The parity rows for the editor integration, driven the way Marlowe drives it: read the lock file,
// POST JSON-RPC with the token. The editor behind the server is scripted (FakeHost).
//   U14  diffs in the editor, selection and open-file context, diagnostics, over loopback MCP with a lock-file token
//   U18  execute_code in the active notebook's kernel; text and image outputs

import test, { afterEach, beforeEach } from "node:test";
import assert from "node:assert/strict";
import * as path from "node:path";
import { post, rig, textOf, type Rig } from "./helpers";
import { TINY_PNG } from "../../src/testing/fakeHost";

let r: Rig;
beforeEach(async () => {
  r = await rig();
});
afterEach(async () => {
  await r.stop();
});

const file = () => path.join(r.folder, "main.py");

test("parity_u14_the_selection_and_open_files_are_sent", async () => {
  r.host.ctx.selection = {
    path: file(),
    language: "python",
    text: "x = 1\ny = 2",
    truncated: false,
    range: { start: { line: 4, character: 0 }, end: { line: 5, character: 5 } },
    empty: false,
  };
  r.host.ctx.openEditors = [
    { path: file(), language: "python", dirty: true, active: true },
    { path: path.join(r.folder, "util.py"), language: "python", dirty: false, active: false },
  ];
  const ctx = JSON.parse(textOf(await r.call("get_context")));
  assert.equal(ctx.selection.path, file());
  assert.equal(ctx.selection.text, "x = 1\ny = 2");
  assert.deepEqual([ctx.selection.startLine, ctx.selection.endLine], [5, 6], "lines are 1-based on the wire");
  assert.equal(ctx.openEditors.length, 2);
  assert.equal(ctx.openEditors[0].dirty, true);
  assert.deepEqual(ctx.workspaceFolders, [r.folder]);
});

test("parity_u14_diagnostics_are_read_for_all_files_or_one", async () => {
  const other = path.join(r.folder, "other.ts");
  r.host.problems = [
    { path: file(), diagnostics: [{ severity: "error", message: "Name `y` is not defined", range: { start: { line: 6, character: 2 }, end: { line: 6, character: 3 } }, source: "pyright", code: "reportUndefinedVariable" }] },
    { path: other, diagnostics: [{ severity: "warning", message: "unused\nimport", range: { start: { line: 0, character: 0 }, end: { line: 0, character: 4 } }, source: "eslint" }] },
  ];
  const all = textOf(await r.call("get_diagnostics"));
  assert.match(all, /main\.py:7:3 error \[pyright reportUndefinedVariable\] Name `y` is not defined/);
  assert.match(all, /other\.ts:1:1 warning \[eslint\] unused import/, "a message is one line");
  const one = textOf(await r.call("get_diagnostics", { path: "other.ts" }));
  assert.doesNotMatch(one, /main\.py/);
  assert.match(textOf(await r.call("get_diagnostics", { path: "clean.py" })), /No problems reported for/);
  r.host.problems = [];
  assert.equal(textOf(await r.call("get_diagnostics")), "No problems reported.");
});

test("parity_u14_a_diff_opens_in_the_editor_and_nothing_is_written", async () => {
  const out = await r.call("open_diff", { path: "main.py", new_contents: "print('hi')\n", title: "Marlowe: fix" });
  assert.match(textOf(out), /diff of .*main\.py is open/);
  assert.equal(r.host.diffs.length, 1);
  assert.equal(r.host.diffs[0].path, file());
  assert.equal(r.host.diffs[0].newContents, "print('hi')\n");
  assert.equal(r.host.diffs[0].wait, false);
  assert.match(textOf(await r.call("close_diffs")), /Closed 1 diff/);
});

test("parity_u14_a_waited_diff_reports_the_users_answer", async () => {
  for (const [answer, re] of [
    ["accepted", /accepted the change/],
    ["rejected", /rejected the change/],
    ["closed", /closed the diff .* without answering/],
  ] as const) {
    r.host.diffAnswer = answer;
    const out = await r.call("open_diff", { path: "main.py", new_contents: "a", wait: true });
    assert.match(textOf(out), re);
    assert.match(textOf(out), answer === "accepted" ? /Nothing was written/ : /./);
  }
  r.host.diffAnswer = "never";
  const t = await r.call("open_diff", { path: "main.py", new_contents: "a", wait: true, timeout_seconds: 5 });
  // The scripted editor honours the same deadline the real one does; use the shortest allowed.
  assert.match(textOf(t), /No answer about .* within 5 s/);
});

test("parity_u14_open_file_selects_lines", async () => {
  const out = await r.call("open_file", { path: "main.py", line: 3, end_line: 5 });
  assert.match(textOf(out), /at line 3-5/);
  assert.deepEqual(r.host.opened[0], { path: file(), line: 2, endLine: 4, preview: true });
  const bad = await r.call("open_file", { path: "main.py", line: 5, end_line: 2 });
  assert.equal(bad.isError, true);
});

test("parity_u14_paths_outside_the_workspace_are_refused", async () => {
  const outside = process.platform === "win32" ? "C:\\Windows\\win.ini" : "/etc/passwd";
  for (const [tool, args] of [
    ["open_file", { path: outside }],
    ["open_diff", { path: outside, new_contents: "x" }],
    ["get_diagnostics", { path: outside }],
    ["open_file", { path: path.join("..", "..", "elsewhere.txt") }],
  ] as const) {
    const res = await r.call(tool, args as Record<string, unknown>);
    assert.equal(res.isError, true, `${tool} ${JSON.stringify(args)}`);
    assert.match(textOf(res), /outside the open workspace/);
  }
  assert.equal(r.host.opened.length + r.host.diffs.length, 0);
});

test("parity_u14_an_untrusted_workspace_gets_no_diffs_and_no_code", async () => {
  r.host.isTrusted = false;
  const d = await r.call("open_diff", { path: "main.py", new_contents: "x" });
  const c = await r.call("execute_code", { code: "1+1" });
  assert.equal(d.isError, true);
  assert.equal(c.isError, true);
  assert.match(textOf(d), /not trusted/);
  assert.match(textOf(c), /not trusted/);
  assert.equal(r.host.diffs.length, 0);
  // Reading is still allowed.
  assert.equal((await r.call("get_context")).isError, undefined);
});

test("parity_u14_the_lock_file_names_the_server_the_token_and_the_consequences", async () => {
  const lock = r.lock;
  assert.equal(lock.version, 1);
  assert.equal(lock.pid, process.pid);
  assert.match(lock.url, /^http:\/\/127\.0\.0\.1:\d+\/mcp$/);
  assert.match(lock.authToken, /^[0-9a-f]{64}$/);
  assert.deepEqual(lock.workspaceFolders, [r.folder]);
  assert.equal(lock.consequences.get_context, "inert");
  assert.equal(lock.consequences.open_diff, "reversible");
  assert.equal(lock.consequences.execute_code, "irreversible", "running code asks");
  assert.equal(path.basename(r.bridge.lockFile), `${r.bridge.port}.lock`);
  assert.equal(path.dirname(r.bridge.lockFile), path.join(r.home, "ide"));
  await r.bridge.stop();
  const fs = await import("node:fs");
  assert.equal(fs.existsSync(r.bridge.lockFile), false, "stopping removes the lock file");
  // stop() runs again in afterEach; it must be harmless.
});

test("parity_u18_execute_code_runs_in_the_notebook_kernel", async () => {
  const out = await r.call("execute_code", { code: "1+1" });
  assert.equal(out.isError, undefined);
  assert.equal(textOf(out), "2");
  const printed = await r.call("execute_code", { code: 'print("hello")' });
  assert.equal(textOf(printed).trim(), "hello");
});

test("parity_u18_an_image_output_becomes_an_image_part_not_its_text_repr", async () => {
  const out = await r.call("execute_code", { code: "plot()" });
  const images = out.content.filter((c) => c.type === "image");
  assert.equal(images.length, 1);
  assert.equal(images[0].mimeType, "image/png");
  assert.equal(Buffer.from(images[0].data, "base64").equals(TINY_PNG), true, "the bytes arrive intact");
  assert.doesNotMatch(textOf(out), /Figure size/, "the figure's repr adds nothing once the picture is there");
});

test("parity_u18_a_kernel_error_is_an_error_result_without_escape_codes", async () => {
  const out = await r.call("execute_code", { code: "boom()" });
  assert.equal(out.isError, true);
  assert.match(textOf(out), /\[error\]\nValueError: bad/);
  assert.doesNotMatch(textOf(out), /\u001b/);
});

test("parity_u18_a_hung_kernel_is_interrupted_at_the_deadline", async () => {
  const t0 = Date.now();
  const out = await r.call("execute_code", { code: "hang()", timeout_seconds: 1 });
  assert.ok(Date.now() - t0 < 5000);
  assert.match(textOf(out), /stopped after 1 s; the kernel was asked to interrupt/);
  assert.equal(r.host.kernelCancelled, 1);
});

test("parity_u18_no_kernel_says_how_to_get_one", async () => {
  r.host.kernel = undefined;
  const out = await r.call("execute_code", { code: "1+1" });
  assert.equal(out.isError, true);
  assert.match(textOf(out), /no running kernel/);
});

test("parity_u18_a_cancelled_request_interrupts_the_kernel", async () => {
  const sent = r.rpc("tools/call", { name: "execute_code", arguments: { code: "hang()", timeout_seconds: 60 } }, 900);
  await new Promise((res) => setTimeout(res, 150));
  await post(r.lock, { jsonrpc: "2.0", method: "notifications/cancelled", params: { requestId: 900 } });
  const answer = await Promise.race([sent, new Promise((res) => setTimeout(() => res("still waiting"), 3000))]);
  assert.notEqual(answer, "still waiting");
  assert.equal(r.host.kernelCancelled, 1);
});
