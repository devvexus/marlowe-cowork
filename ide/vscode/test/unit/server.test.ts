// The loopback server's three gates (token, Origin, Host) and the JSON-RPC surface.

import test, { afterEach, beforeEach } from "node:test";
import assert from "node:assert/strict";
import * as http from "node:http";
import { post, rig, type Rig } from "./helpers";
import { MAX_BODY_BYTES } from "../../src/server";
import { SUPPORTED_PROTOCOLS } from "../../src/mcp";

let r: Rig;
beforeEach(async () => {
  r = await rig();
});
afterEach(async () => {
  await r.stop();
});

const ping = { jsonrpc: "2.0", id: 1, method: "ping" };

test("a request without the token, or with another, is refused before any JSON is read", async () => {
  const none = await post({ url: r.lock.url, authToken: "" }, "not json at all");
  assert.equal(none.status, 401);
  const wrong = await post({ url: r.lock.url, authToken: "0".repeat(64) }, ping);
  assert.equal(wrong.status, 401);
  const ok = await post(r.lock, ping);
  assert.equal(ok.status, 200);
});

test("a browser request is refused even with the right token", async () => {
  const res = await post(r.lock, ping, { Origin: "https://evil.example" });
  assert.equal(res.status, 403);
});

test("a Host header that is not this server is refused (DNS rebinding)", async () => {
  const res = await post(r.lock, ping, { Host: "evil.example" });
  assert.equal(res.status, 403);
  const alsoOk = await post(r.lock, ping, { Host: `localhost:${r.bridge.port}` });
  assert.equal(alsoOk.status, 200);
});

test("only POST /mcp answers", async () => {
  const u = new URL(r.lock.url);
  const status = (method: string, p: string) =>
    new Promise<number>((resolve, reject) => {
      const req = http.request({ host: u.hostname, port: u.port, path: p, method, headers: { Authorization: `Bearer ${r.lock.authToken}` } }, (res) => {
        res.resume();
        resolve(res.statusCode ?? 0);
      });
      req.on("error", reject);
      req.end();
    });
  assert.equal(await status("GET", "/mcp"), 405);
  assert.equal(await status("POST", "/elsewhere"), 404);
});

test("a body over the cap is refused, and bad JSON is a parse error", async () => {
  const big = await post(r.lock, "x".repeat(MAX_BODY_BYTES + 10)).catch(() => ({ status: 413, body: "" }));
  assert.equal(big.status, 413);
  const bad = await post(r.lock, "{nope");
  assert.equal(bad.status, 400);
  assert.equal(JSON.parse(bad.body).error.code, -32700);
});

test("initialize negotiates the protocol and lists tools", async () => {
  const init = await r.rpc("initialize", { protocolVersion: "2024-11-05", capabilities: {}, clientInfo: { name: "t", version: "0" } });
  assert.equal(init.result.protocolVersion, "2024-11-05");
  assert.equal(init.result.serverInfo.name, "marlowe-vscode");
  const unknown = await r.rpc("initialize", { protocolVersion: "1999-01-01" });
  assert.equal(unknown.result.protocolVersion, SUPPORTED_PROTOCOLS[0]);
  const tools = (await r.rpc("tools/list")).result.tools as { name: string; annotations: { readOnlyHint: boolean }; inputSchema: { type: string } }[];
  assert.deepEqual(
    tools.map((t) => t.name).sort(),
    ["close_diffs", "execute_code", "get_context", "get_diagnostics", "open_diff", "open_file"],
  );
  assert.ok(tools.every((t) => t.inputSchema.type === "object"));
  assert.equal(tools.find((t) => t.name === "get_context")?.annotations.readOnlyHint, true);
  assert.equal(tools.find((t) => t.name === "execute_code")?.annotations.readOnlyHint, false);
});

test("notifications get 202 and no body; unknown methods and tools are errors", async () => {
  const note = await post(r.lock, { jsonrpc: "2.0", method: "notifications/initialized" });
  assert.equal(note.status, 202);
  assert.equal(note.body, "");
  const m = await r.rpc("resources/list");
  assert.equal(m.error.code, -32601);
  const t = await r.rpc("tools/call", { name: "nope", arguments: {} });
  assert.equal(t.error.code, -32602);
  const a = await r.rpc("tools/call", { name: "get_context", arguments: [] });
  assert.equal(a.error.code, -32602);
});

test("a batch is answered in one array, notifications left out", async () => {
  const res = await post(r.lock, [ping, { jsonrpc: "2.0", method: "notifications/initialized" }, { jsonrpc: "2.0", id: 2, method: "ping" }]);
  assert.equal(res.status, 200);
  const body = JSON.parse(res.body) as { id: number }[];
  assert.deepEqual(body.map((b) => b.id), [1, 2]);
});

test("a tool's bad argument is a result the model can read, not a protocol error", async () => {
  const res = await r.call("open_file", { path: 7 });
  assert.equal(res.isError, true);
  assert.match(res.content[0].text, /`path` must be a string/);
  const lines = await r.call("open_file", { path: "a.py", line: 0 });
  assert.equal(lines.isError, true);
});

test("two bridges on one home get two lock files, and stale locks are swept at the next start", async () => {
  const fs = await import("node:fs");
  const path = await import("node:path");
  const { FakeHost } = await import("../../src/testing/fakeHost");
  const { startBridge } = await import("../../src/bridge");
  const second = await startBridge({ host: new FakeHost([r.folder]), home: r.home, ideName: "Cursor", version: "t" });
  assert.notEqual(second.port, r.bridge.port);
  assert.equal(fs.readdirSync(path.join(r.home, "ide")).filter((n) => n.endsWith(".lock")).length, 2);
  // A lock left by a process that is gone.
  const dead = path.join(r.home, "ide", "1.lock");
  fs.writeFileSync(dead, JSON.stringify({ version: 1, pid: 2147483000 }));
  const third = await startBridge({ host: new FakeHost([r.folder]), home: r.home, ideName: "VS Code", version: "t" });
  assert.equal(fs.existsSync(dead), false, "swept");
  assert.equal(fs.existsSync(r.bridge.lockFile), true, "a live editor's lock is left alone");
  await second.stop();
  await third.stop();
});
