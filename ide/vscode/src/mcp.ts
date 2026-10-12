// JSON-RPC 2.0 for the MCP server: initialize, ping, tools/list, tools/call. No server-to-client
// messages, so a plain POST-and-answer HTTP server is enough. The HTTP layer gives each client a
// session at `initialize`; calls in flight are keyed by session and request id, because every
// client numbers its requests from 1 and one client's cancel must not stop another's call.

import type { Host } from "./host";
import { TOOLS, CONSEQUENCES, type ToolDef, type ToolResult } from "./tools";

export const SUPPORTED_PROTOCOLS = ["2025-06-18", "2025-03-26", "2024-11-05"];

export type Id = string | number;

export interface Request {
  jsonrpc?: string;
  id?: Id | null;
  method?: unknown;
  params?: unknown;
}

export interface Response {
  jsonrpc: "2.0";
  id: Id | null;
  result?: unknown;
  error?: { code: number; message: string; data?: unknown };
}

const ERR = { parse: -32700, invalid: -32600, method: -32601, params: -32602, internal: -32603 } as const;

const flightKey = (session: string, id: Id): string => `${session}|${typeof id}:${id}`;

const error = (id: Id | null, code: number, message: string): Response => ({ jsonrpc: "2.0", id, error: { code, message } });

export class McpServer {
  /** Calls in flight by session and request id, so `notifications/cancelled` can stop one. */
  private inflight = new Map<string, AbortController>();

  constructor(
    private readonly host: Host,
    private readonly info: { name: string; version: string },
    private readonly tools: ToolDef[] = TOOLS,
  ) {}

  /**
   * Handles one message. `undefined` means a notification: nothing to answer. `signal` aborts when
   * the HTTP request goes away.
   */
  async handle(message: unknown, signal: AbortSignal, session = ""): Promise<Response | undefined> {
    if (typeof message !== "object" || message === null || Array.isArray(message)) {
      return error(null, ERR.invalid, "expected a JSON-RPC object");
    }
    const req = message as Request;
    const hasId = req.id !== undefined && req.id !== null;
    if (typeof req.method !== "string") {
      return hasId ? error(req.id as Id, ERR.invalid, "`method` must be a string") : undefined;
    }
    if (!hasId) {
      this.notification(req.method, req.params, session);
      return undefined;
    }
    const id = req.id as Id;
    try {
      return { jsonrpc: "2.0", id, result: await this.request(id, req.method, req.params, signal, session) };
    } catch (e) {
      if (e instanceof RpcError) return error(id, e.code, e.message);
      return error(id, ERR.internal, e instanceof Error ? e.message : String(e));
    }
  }

  private notification(method: string, params: unknown, session: string): void {
    if (method === "notifications/cancelled" && typeof params === "object" && params !== null) {
      const rid = (params as { requestId?: Id }).requestId;
      if (rid !== undefined) this.inflight.get(flightKey(session, rid))?.abort();
    }
  }

  private async request(id: Id, method: string, params: unknown, signal: AbortSignal, session: string): Promise<unknown> {
    const p = (typeof params === "object" && params !== null ? params : {}) as Record<string, unknown>;
    switch (method) {
      case "initialize": {
        const asked = typeof p.protocolVersion === "string" ? p.protocolVersion : "";
        return {
          protocolVersion: SUPPORTED_PROTOCOLS.includes(asked) ? asked : SUPPORTED_PROTOCOLS[0],
          capabilities: { tools: { listChanged: false } },
          serverInfo: this.info,
          instructions:
            "This server is the user's code editor. get_context says what they are looking at; get_diagnostics lists the language servers' problems; open_file and open_diff show things to them; execute_code runs in their notebook kernel.",
        };
      }
      case "ping":
        return {};
      case "tools/list":
        return {
          tools: this.tools.map((t) => ({
            name: t.name,
            description: t.description,
            inputSchema: t.inputSchema,
            annotations: { readOnlyHint: t.readOnly, destructiveHint: false, openWorldHint: false },
          })),
        };
      case "tools/call":
        return this.call(id, p, signal, session);
      default:
        throw new RpcError(ERR.method, `method not found: ${method}`);
    }
  }

  private async call(id: Id, p: Record<string, unknown>, signal: AbortSignal, session: string): Promise<ToolResult> {
    const tool = this.tools.find((t) => t.name === p.name);
    if (!tool) throw new RpcError(ERR.params, `unknown tool: ${String(p.name)}`);
    const args = p.arguments === undefined || p.arguments === null ? {} : p.arguments;
    if (typeof args !== "object" || Array.isArray(args)) throw new RpcError(ERR.params, "`arguments` must be an object");
    const ctl = new AbortController();
    const key = flightKey(session, id);
    this.inflight.set(key, ctl);
    const onAbort = () => ctl.abort();
    signal.addEventListener("abort", onAbort);
    try {
      return await tool.run(args as Record<string, unknown>, this.host, ctl.signal);
    } catch (e) {
      // A tool's own failure is a result the model reads, not a protocol error.
      return { content: [{ type: "text", text: e instanceof Error ? e.message : String(e) }], isError: true };
    } finally {
      if (this.inflight.get(key) === ctl) this.inflight.delete(key);
      signal.removeEventListener("abort", onAbort);
    }
  }
}

export class RpcError extends Error {
  constructor(
    readonly code: number,
    message: string,
  ) {
    super(message);
  }
}

export { ERR, CONSEQUENCES };
