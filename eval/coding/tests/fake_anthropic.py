"""A tiny Anthropic Messages API on loopback, so the real ``claude`` binary can run with no model.

It answers ``POST /v1/messages`` (streaming or not) with a fixed text, ``count_tokens`` with a number,
and anything else with an empty JSON object, and logs every request (method, path, the credential
headers) so a test can prove where Claude Code sent what.
"""

from __future__ import annotations

import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class FakeAnthropic:
    def __init__(self, reply: str = "FAKE-REPLY") -> None:
        self.reply = reply
        self.requests: list = []
        outer = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *a):  # silence
                return

            def _body(self) -> bytes:
                n = int(self.headers.get("Content-Length") or 0)
                return self.rfile.read(n) if n else b""

            def _send(self, code: int, body: bytes, ctype="application/json") -> None:
                self.send_response(code)
                self.send_header("Content-Type", ctype)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def do_POST(self):
                raw = self._body()
                try:
                    data = json.loads(raw or b"{}")
                except json.JSONDecodeError:
                    data = {}
                outer.requests.append(
                    {
                        "method": "POST",
                        "path": self.path,
                        "x-api-key": self.headers.get("x-api-key"),
                        "authorization": self.headers.get("Authorization"),
                        "model": data.get("model"),
                        "stream": data.get("stream"),
                        "tools": len(data.get("tools") or []),
                        "thinking": data.get("thinking"),
                    }
                )
                path = self.path.split("?")[0]
                if path.endswith("/count_tokens"):
                    self._send(200, json.dumps({"input_tokens": 12}).encode())
                elif path.endswith("/v1/messages"):
                    model = data.get("model", "fake")
                    if data.get("stream"):
                        self._stream(model)
                    else:
                        msg = {
                            "id": "msg_fake",
                            "type": "message",
                            "role": "assistant",
                            "model": model,
                            "content": [{"type": "text", "text": outer.reply}],
                            "stop_reason": "end_turn",
                            "stop_sequence": None,
                            "usage": {"input_tokens": 12, "output_tokens": 4},
                        }
                        self._send(200, json.dumps(msg).encode())
                else:
                    self._send(200, b"{}")

            def do_GET(self):
                outer.requests.append({"method": "GET", "path": self.path, "x-api-key": self.headers.get("x-api-key"), "authorization": self.headers.get("Authorization")})
                self._send(200, b"{}")

            def _stream(self, model):
                def ev(name, obj):
                    return f"event: {name}\ndata: {json.dumps(obj)}\n\n".encode()

                body = b"".join(
                    [
                        ev("message_start", {"type": "message_start", "message": {"id": "msg_fake", "type": "message", "role": "assistant", "model": model, "content": [], "stop_reason": None, "usage": {"input_tokens": 12, "output_tokens": 1}}}),
                        ev("content_block_start", {"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
                        ev("content_block_delta", {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": outer.reply}}),
                        ev("content_block_stop", {"type": "content_block_stop", "index": 0}),
                        ev("message_delta", {"type": "message_delta", "delta": {"stop_reason": "end_turn", "stop_sequence": None}, "usage": {"output_tokens": 4}}),
                        ev("message_stop", {"type": "message_stop"}),
                    ]
                )
                self._send(200, body, "text/event-stream")

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.port = self.server.server_address[1]
        self.url = f"http://127.0.0.1:{self.port}"
        self._t = threading.Thread(target=self.server.serve_forever, daemon=True)

    def __enter__(self) -> "FakeAnthropic":
        self._t.start()
        return self

    def __exit__(self, *exc) -> None:
        self.server.shutdown()
        self.server.server_close()
        self._t.join(timeout=5)
