"""The fixture app's dev server: static files plus one JSON endpoint, on 127.0.0.1.

PORT=0 (the default) picks a free port and prints it, so a test can wait on the log line.
"""
import http.server
import json
import os
import socketserver

ROOT = os.path.dirname(os.path.abspath(__file__))


class Handler(http.server.SimpleHTTPRequestHandler):
    def __init__(self, *args, **kwargs):
        super().__init__(*args, directory=ROOT, **kwargs)

    def do_GET(self):
        if self.path.startswith("/api/items"):
            body = json.dumps({"items": [{"id": 1, "name": "alpha"}, {"id": 2, "name": "beta"}, {"id": 3, "name": "gamma"}]}).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        super().do_GET()

    def log_message(self, *args):
        pass


class Server(socketserver.ThreadingTCPServer):
    daemon_threads = True
    allow_reuse_address = False


with Server(("127.0.0.1", int(os.environ.get("PORT", "0"))), Handler) as server:
    print("serving on http://127.0.0.1:%d" % server.server_address[1], flush=True)
    server.serve_forever()
