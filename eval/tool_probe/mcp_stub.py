"""A stdio MCP server for the probe: a notes server with two real tools and, with
`--extra-tools N`, N filler tools (enough of them and the host lists a `tool_search` instead).
Only what the probe needs: initialize, tools/list, tools/call, resources/list, resources/read."""

import json
import sys

TOOLS = [
    {
        "name": "search_notes",
        "description": "Search the user's notes by text.",
        "inputSchema": {
            "type": "object",
            "properties": {"query": {"type": "string"}, "limit": {"type": "integer"}},
            "required": ["query"],
        },
    },
    {
        "name": "add_note",
        "description": "Save a note with a title.",
        "inputSchema": {
            "type": "object",
            "properties": {"title": {"type": "string"}, "text": {"type": "string"}},
            "required": ["title", "text"],
        },
    },
]


def filler(n):
    topics = ["invoice", "ticket", "calendar", "contact", "budget", "recipe", "playlist", "route", "sensor", "backup"]
    return [
        {
            "name": f"{topics[i % len(topics)]}_op_{i}",
            "description": f"Operate on {topics[i % len(topics)]} records (variant {i}).",
            "inputSchema": {"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]},
        }
        for i in range(n)
    ]


def main(argv):
    extra = int(argv[argv.index("--extra-tools") + 1]) if "--extra-tools" in argv else 0
    tools = TOOLS + filler(extra)
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        msg = json.loads(line)
        if "id" not in msg:
            continue
        method, params = msg.get("method"), msg.get("params") or {}
        if method == "initialize":
            result = {
                "protocolVersion": params.get("protocolVersion", "2025-06-18"),
                "capabilities": {"tools": {}, "resources": {}},
                "serverInfo": {"name": "notes", "version": "1"},
            }
        elif method == "tools/list":
            result = {"tools": tools}
        elif method == "resources/list":
            result = {"resources": [{"uri": "notes://weekly", "name": "weekly", "mimeType": "text/markdown"}]}
        elif method == "resources/read":
            result = {"contents": [{"uri": "notes://weekly", "mimeType": "text/markdown", "text": "# Weekly\n- probe stub"}]}
        elif method == "tools/call":
            result = {"content": [{"type": "text", "text": "probe stub: nothing stored"}]}
        else:
            result = {}
        sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": msg["id"], "result": result}) + "\n")
        sys.stdout.flush()


if __name__ == "__main__":
    main(sys.argv[1:])
