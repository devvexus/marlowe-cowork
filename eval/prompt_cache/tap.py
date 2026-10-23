"""A transparent HTTP tee: sits between the daemon and a model server and records every model call.

The daemon talks to Ollama at MARLOWE_OLLAMA_ADDR and to its own llama-server on a port it picked. This puts a
listener where the daemon connects and forwards each request, byte for byte, to the real server, keeping the
request body and the whole streamed answer. Nothing is rewritten except `Connection: close` (one request per
connection keeps the framing trivial). Standard library only.

Used two ways: in the audit process in front of Ollama, and inside llama_wrap.py in front of the engine's server.
"""

import gzip
import json
import os
import socket
import threading
import time

CRLF = b"\r\n"
# Only these POSTs are model calls worth a record; everything else is forwarded untouched.
RECORDED = ("/api/chat", "/v1/chat/completions", "/api/generate", "/completion", "/v1/completions")


def _read_head(sock, buf):
    """Reads until the blank line; returns (head bytes, rest of buf) or (None, buf) at EOF."""
    while b"\r\n\r\n" not in buf:
        chunk = sock.recv(65536)
        if not chunk:
            return None, buf
        buf += chunk
    i = buf.index(b"\r\n\r\n") + 4
    return buf[:i], buf[i:]


def _headers(head):
    lines = head.split(CRLF)
    out = []
    for ln in lines[1:]:
        if b":" in ln:
            k, v = ln.split(b":", 1)
            out.append((k.strip().decode("latin1"), v.strip().decode("latin1")))
    return lines[0].decode("latin1"), out


def dechunk(raw):
    """Decodes an HTTP chunked body (whole, already received)."""
    out, i = bytearray(), 0
    while True:
        j = raw.find(CRLF, i)
        if j < 0:
            break
        try:
            n = int(raw[i:j].split(b";")[0], 16)
        except ValueError:
            break
        if n == 0:
            break
        out += raw[j + 2:j + 2 + n]
        i = j + 2 + n + 2
    return bytes(out)


def _body_of(head_headers, raw_body):
    h = {k.lower(): v for k, v in head_headers}
    if "chunked" in h.get("transfer-encoding", "").lower():
        return dechunk(raw_body)
    return raw_body


class Tap:
    """Listens on `listen_port` and forwards to `upstream` = (host, port).

    on_request(method, path, body_bytes) -> dict | None runs before forwarding a recorded call (the wrapper uses it to
    render the prompt); its dict lands in the record under "extra". on_record(record) gets each finished call.
    on_done(record, request_bytes, answer_bytes) -> dict | None runs once the answer is in; its dict is merged into "extra".
    log_path, when given, is a server log whose size is noted before and after each call so the lines the server wrote
    for it can be read back (Ollama's server.log).
    """

    def __init__(self, listen_port, upstream, on_record, on_request=None, on_done=None, log_path=None, raw_dir=None, settle=0.4):
        self.listen_port, self.upstream = listen_port, upstream
        self.on_record, self.on_request, self.on_done = on_record, on_request, on_done
        self.log_path, self.raw_dir, self.settle = log_path, raw_dir, settle
        self.seq = 0
        self.lock = threading.Lock()
        self.sock = None
        self.stopped = False
        if raw_dir:
            os.makedirs(raw_dir, exist_ok=True)

    def start(self):
        s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        s.bind(("127.0.0.1", self.listen_port))
        s.listen(64)
        self.sock = s
        threading.Thread(target=self._accept, daemon=True).start()
        return self

    def stop(self):
        self.stopped = True
        try:
            self.sock.close()
        except OSError:
            pass

    def _accept(self):
        while not self.stopped:
            try:
                c, _ = self.sock.accept()
            except OSError:
                return
            threading.Thread(target=self._serve, args=(c,), daemon=True).start()

    def _log_size(self):
        if not self.log_path:
            return None
        try:
            return os.path.getsize(self.log_path)
        except OSError:
            return None

    def _log_slice(self, a, b):
        """The server log lines written while this call ran (at most 8 MB)."""
        if a is None or b is None or b < a:
            return ""
        try:
            with open(self.log_path, "rb") as f:
                f.seek(a)
                return f.read(min(b - a, 8 << 20)).decode("utf-8", "replace")
        except OSError:
            return ""

    def _serve(self, client):
        up = None
        try:
            head, rest = _read_head(client, b"")
            if head is None:
                return
            line, hdrs = _headers(head)
            method, path = line.split(" ")[0], line.split(" ")[1]
            h = {k.lower(): v for k, v in hdrs}
            body = b""
            if "chunked" in h.get("transfer-encoding", "").lower():
                raw = rest
                while not raw.endswith(b"0\r\n\r\n"):
                    chunk = client.recv(65536)
                    if not chunk:
                        break
                    raw += chunk
                body_wire, body = raw, dechunk(raw)
            else:
                n = int(h.get("content-length", "0") or 0)
                while len(rest) < n:
                    chunk = client.recv(65536)
                    if not chunk:
                        break
                    rest += chunk
                body_wire = body = rest[:n]
            # one request per connection; no 100-continue dance
            keep = [(k, v) for k, v in hdrs if k.lower() not in ("connection", "keep-alive", "proxy-connection", "expect")]
            new_head = line.encode("latin1") + CRLF + b"".join(f"{k}: {v}".encode("latin1") + CRLF for k, v in keep) + b"Connection: close" + CRLF + CRLF
            recorded = method == "POST" and path.split("?")[0] in RECORDED
            rec = None
            if recorded:
                with self.lock:
                    self.seq += 1
                    seq = self.seq
                rec = {"seq": seq, "method": method, "path": path, "t_start": time.time(), "log0": self._log_size(), "extra": None}
                if self.on_request:
                    try:
                        rec["extra"] = self.on_request(method, path, body)
                    except Exception as e:  # a failed side render must never stop the call
                        rec["extra"] = {"error": repr(e)}
            up = socket.create_connection(self.upstream, timeout=30)
            up.settimeout(None)
            up.sendall(new_head + body_wire)
            raw = bytearray()
            aborted = False
            first = None
            while True:
                try:
                    chunk = up.recv(65536)
                except OSError:
                    break
                if not chunk:
                    break
                if first is None:
                    first = time.time()
                raw += chunk
                try:
                    client.sendall(chunk)
                except OSError:
                    aborted = True
                    break
            if rec is not None:
                rec["t_first"], rec["t_end"], rec["aborted"] = first, time.time(), aborted
                head2, rest2 = _read_head_bytes(bytes(raw))
                status_line, rh = _headers(head2) if head2 else ("", [])
                rec["status"] = int(status_line.split(" ")[1]) if status_line.startswith("HTTP/") else None
                rbody = _body_of(rh, rest2)
                if self.log_path:
                    time.sleep(self.settle)
                    rec["log1"] = self._log_size()
                    rec["log_text"] = self._log_slice(rec["log0"], rec["log1"])
                self._save(rec, body, rbody)
                if self.on_done:
                    try:
                        rec["extra"] = {**(rec["extra"] or {}), **(self.on_done(rec, body, rbody) or {})}
                    except Exception as e:
                        rec["extra"] = {**(rec["extra"] or {}), "done_error": repr(e)[:300]}
                self.on_record(rec)
        except Exception as e:  # keep the tap alive whatever one connection does
            try:
                client.sendall(b"HTTP/1.1 502 Bad Gateway\r\nConnection: close\r\nContent-Length: 0\r\n\r\n")
            except OSError:
                pass
            if os.environ.get("TAP_DEBUG"):
                print("tap error", repr(e), flush=True)
        finally:
            for s in (up, client):
                try:
                    s and s.close()
                except OSError:
                    pass

    def _save(self, rec, req, resp):
        """Request and answer bodies go to files beside the log; the record keeps their names and sizes."""
        rec["req_bytes"], rec["resp_bytes"] = len(req), len(resp)
        rec["raw_dir"] = self.raw_dir
        if not self.raw_dir:
            rec["req_text"] = req.decode("utf-8", "replace")
            rec["resp_text"] = resp.decode("utf-8", "replace")
            return
        base = os.path.join(self.raw_dir, f"{rec['seq']:04d}")
        with open(base + ".req.json", "wb") as f:
            f.write(req)
        with gzip.open(base + ".resp.gz", "wb") as f:
            f.write(resp)
        rec["req_file"], rec["resp_file"] = os.path.basename(base) + ".req.json", os.path.basename(base) + ".resp.gz"


def _read_head_bytes(raw):
    if b"\r\n\r\n" not in raw:
        return None, b""
    i = raw.index(b"\r\n\r\n") + 4
    return raw[:i], raw[i:]


def load_record_bodies(fallback_dir, rec):
    """The (request text, answer text) of a record; its files are where it says, else under fallback_dir/<same last name>."""
    if "req_text" in rec:
        return rec["req_text"], rec["resp_text"]
    raw_dir = rec.get("raw_dir") or ""
    if not os.path.isdir(raw_dir):
        raw_dir = os.path.join(fallback_dir, os.path.basename(raw_dir.rstrip("\/"))) if raw_dir else fallback_dir
    with open(os.path.join(raw_dir, rec["req_file"]), "rb") as f:
        req = f.read().decode("utf-8", "replace")
    with gzip.open(os.path.join(raw_dir, rec["resp_file"]), "rb") as f:
        resp = f.read().decode("utf-8", "replace")
    return req, resp


class JsonlSink:
    """on_record that appends one JSON line per call (safe across threads)."""

    def __init__(self, path):
        self.path, self.lock = path, threading.Lock()

    def __call__(self, rec):
        with self.lock, open(self.path, "a", encoding="utf-8") as f:
            f.write(json.dumps(rec, ensure_ascii=False) + "\n")
