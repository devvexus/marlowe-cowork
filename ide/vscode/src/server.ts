// The loopback HTTP face of the MCP server (streamable HTTP, POST only, plus DELETE to end a
// session). `initialize` mints an `Mcp-Session-Id`; later requests carry it. Three checks come before
// any JSON is read, because any web page can make a browser send a request to 127.0.0.1:
//   1. the bearer token from the lock file (constant-time compare),
//   2. no `Origin` header (a browser always sends one; the Marlowe client never does),
//   3. a `Host` header that names this server (defeats DNS rebinding).

import * as crypto from "node:crypto";
import * as http from "node:http";
import type { AddressInfo } from "node:net";
import { McpServer, ERR } from "./mcp";

export const ENDPOINT = "/mcp";
export const MAX_BODY_BYTES = 4 * 1024 * 1024;

export interface RunningServer {
  port: number;
  url: string;
  token: string;
  close(): Promise<void>;
}

export function newToken(): string {
  return crypto.randomBytes(32).toString("hex");
}

function sameToken(given: string, expected: string): boolean {
  const a = crypto.createHash("sha256").update(given).digest();
  const b = crypto.createHash("sha256").update(expected).digest();
  return crypto.timingSafeEqual(a, b);
}

function reply(res: http.ServerResponse, status: number, body: unknown, headers: Record<string, string> = {}): void {
  const payload = body === undefined ? "" : JSON.stringify(body);
  res.writeHead(status, { "Content-Type": "application/json", "Content-Length": Buffer.byteLength(payload), "Cache-Control": "no-store", ...headers });
  res.end(payload);
}

async function readBody(req: http.IncomingMessage): Promise<Buffer | "too-large"> {
  const chunks: Buffer[] = [];
  let n = 0;
  for await (const c of req) {
    n += (c as Buffer).length;
    if (n > MAX_BODY_BYTES) return "too-large";
    chunks.push(c as Buffer);
  }
  return Buffer.concat(chunks);
}

/** Sessions kept at once; the oldest is forgotten past this. */
const MAX_SESSIONS = 256;

export function startServer(mcp: McpServer, token: string = newToken(), port = 0): Promise<RunningServer> {
  const sessions = new Set<string>();
  const server = http.createServer(async (req, res) => {
    try {
      const addr = server.address() as AddressInfo;
      const hostOk = [`127.0.0.1:${addr.port}`, `localhost:${addr.port}`].includes((req.headers.host ?? "").toLowerCase());
      if (!hostOk) return reply(res, 403, { error: "host not allowed" });
      if (req.headers.origin !== undefined) return reply(res, 403, { error: "browser requests are not allowed" });
      const auth = req.headers.authorization ?? "";
      const given = auth.startsWith("Bearer ") ? auth.slice(7) : "";
      if (!sameToken(given, token)) return reply(res, 401, { error: "unauthorized" }, { "WWW-Authenticate": "Bearer" });
      const path = (req.url ?? "").split("?")[0];
      if (path !== ENDPOINT) return reply(res, 404, { error: "not found" });
      const sidHeader = req.headers["mcp-session-id"];
      const sid = typeof sidHeader === "string" ? sidHeader : "";
      if (req.method === "DELETE") {
        if (sid === "" || !sessions.delete(sid)) return reply(res, 404, { error: "no such session" });
        return reply(res, 204, undefined);
      }
      if (req.method !== "POST") return reply(res, 405, { error: "POST only" }, { Allow: "POST" });

      const body = await readBody(req);
      if (body === "too-large") return reply(res, 413, { error: "body too large" });
      let message: unknown;
      try {
        message = JSON.parse(body.toString("utf8"));
      } catch {
        return reply(res, 400, { jsonrpc: "2.0", id: null, error: { code: ERR.parse, message: "not valid JSON" } });
      }

      // A client that names a session this server did not hand out is told so (it starts over
      // with `initialize`); one that names none shares the unnamed scope.
      const opens = !Array.isArray(message) && typeof message === "object" && message !== null && (message as { method?: unknown }).method === "initialize";
      if (sid !== "" && !opens && !sessions.has(sid)) return reply(res, 404, { jsonrpc: "2.0", id: null, error: { code: -32001, message: "unknown session" } });
      let scope = sid;
      const headers: Record<string, string> = {};
      if (opens) {
        scope = crypto.randomBytes(16).toString("hex");
        sessions.add(scope);
        if (sessions.size > MAX_SESSIONS) sessions.delete(sessions.values().next().value as string);
        headers["Mcp-Session-Id"] = scope;
      }

      // The call stops when the client goes away (a cancelled turn, a killed process).
      const gone = new AbortController();
      res.on("close", () => {
        if (!res.writableEnded) gone.abort();
      });

      if (Array.isArray(message)) {
        const answers = (await Promise.all(message.map((m) => mcp.handle(m, gone.signal, scope)))).filter((a) => a !== undefined);
        return answers.length === 0 ? reply(res, 202, undefined) : reply(res, 200, answers, headers);
      }
      const answer = await mcp.handle(message, gone.signal, scope);
      return answer === undefined ? reply(res, 202, undefined) : reply(res, 200, answer, headers);
    } catch {
      if (!res.headersSent) reply(res, 500, { error: "internal error" });
      else res.destroy();
    }
  });
  // A tool such as `open_diff` with `wait` holds its request open; no request timeout of our own.
  server.requestTimeout = 0;
  server.headersTimeout = 30_000;
  return new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(port, "127.0.0.1", () => {
      const actual = (server.address() as AddressInfo).port;
      resolve({
        port: actual,
        url: `http://127.0.0.1:${actual}${ENDPOINT}`,
        token,
        close: () =>
          new Promise((done) => {
            server.closeAllConnections();
            server.close(() => done());
          }),
      });
    });
  });
}
