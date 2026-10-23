"""The prompt-cache audit: does any request rewrite what the model already read?

  python -I eval/prompt_cache/audit.py --go --models marlowe-dawn:9b-super --engines ollama
  python -I eval/prompt_cache/audit.py --go --models marlowe-dusk:22b-r1.5_Q4KM,marlowe-red:9b --engines ollama,llama
  python -I eval/prompt_cache/audit.py --templates                  every model's own chat template and what it does with old reasoning
  python -I eval/prompt_cache/audit.py --analyze DIR                judge the records of an earlier run again

Each (model, engine) is one run of the REAL daemon (throwaway home, a temp git repo as the workspace) through a scripted
conversation built to exercise everything that can rewrite history: thinking, tool calls with several arguments (nested JSON,
numbers, quotes, newlines, unicode), a tool error, a tool result with trailing whitespace, a memory write, an interrupt that
leaves a partial answer, a steer mid-turn, an outside file change, the Teams toggle, more user messages, and a daemon restart.
A tap between the daemon and the model server records every request; the server's own numbers say how much of each prompt it
served from cache. See README.md for how to read the report.

It loads models onto the GPU, so nothing starts without `--go`. Engines: `ollama` (the daemon with MARLOWE_LLAMA_ENGINE=0, every
call through Ollama) and `llama` (Marlowe's own llama-server, only for models Ollama hands to llama-server whole).
"""

import argparse
import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent
sys.path.insert(0, str(HERE))
import gguf  # noqa: E402
import report as rp  # noqa: E402
import tap as tp  # noqa: E402

NO_WINDOW = 0x08000000
QUIET = ROOT / "target" / "cargo-quiet"
OLLAMA = ("127.0.0.1", 11434)
DEFAULT_MODELS = "marlowe-dawn:9b-super,marlowe-mini:4b-super,marlowe-dusk:22b-r1.5_Q4KM,marlowe-red:9b"
OLLAMA_LOG = Path(os.environ.get("LOCALAPPDATA", "")) / "Ollama" / "server.log"
OLLAMA_LIB = Path(os.environ.get("LOCALAPPDATA", "")) / "Programs" / "Ollama" / "lib" / "ollama"


class Quiet(Exception):
    """cargo-quiet appeared: GPU work must stop and wait."""


def log(*a):
    print(time.strftime("%H:%M:%S"), *a, flush=True)


def check_quiet():
    if QUIET.exists():
        raise Quiet()


def http_json(url, body=None, timeout=10):
    req = urllib.request.Request(url, data=None if body is None else json.dumps(body).encode(), headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.loads(r.read())


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


def unload_ollama(model):
    """keep_alive 0 on the model I loaded; leaves anything else alone."""
    try:
        http_json("http://127.0.0.1:11434/api/generate", {"model": model, "keep_alive": 0}, 60)
    except Exception as e:
        log("unload", model, "->", repr(e)[:120])
    for _ in range(30):
        try:
            if not any(m.get("name") == model or m.get("model") == model for m in http_json("http://127.0.0.1:11434/api/ps").get("models", [])):
                return
        except Exception:
            return
        time.sleep(1)


# ---------------------------------------------------------------- the daemon

class Daemon:
    """The real `marlowe daemon` on a throwaway home, spoken to over its own JSON-lines socket."""

    def __init__(self, exe, home, env, out_prefix):
        self.exe, self.home, self.env = exe, Path(home), env
        self.events, self.lock, self.wlock = [], threading.Lock(), threading.Lock()
        self.auto_approve = True
        self.out = open(str(out_prefix) + ".out", "ab")
        self.err = open(str(out_prefix) + ".err", "ab")
        info_path = self.home / "daemon.json"
        info_path.unlink(missing_ok=True)
        self.p = subprocess.Popen([str(exe), "daemon"], env=env, stdout=self.out, stderr=self.err, creationflags=NO_WINDOW)
        for _ in range(240):
            if info_path.exists():
                try:
                    self.info = json.loads(info_path.read_text())
                    if self.info.get("pid") == self.p.pid:
                        break
                except ValueError:
                    pass
            time.sleep(0.5)
        else:
            raise RuntimeError("the daemon did not write daemon.json")
        self.sock = socket.create_connection(("127.0.0.1", self.info["port"]))
        self.f = self.sock.makefile("rwb")
        self.send({"op": "hello", "token": self.info["token"], "client": "prompt-cache-audit"})
        threading.Thread(target=self._reader, daemon=True).start()

    def send(self, req):
        with self.wlock:
            self.f.write((json.dumps(req) + "\n").encode())
            self.f.flush()

    def _reader(self):
        try:
            for line in self.f:
                try:
                    env = json.loads(line)
                except ValueError:
                    continue
                ev = env.get("event", {})
                with self.lock:
                    self.events.append(env)
                try:
                    if ev.get("ev") == "approval_requested" and self.auto_approve:
                        self.send({"op": "approve", "approval": ev["approval"], "decision": "allow"})
                    elif ev.get("ev") == "question_asked":
                        self.send({"op": "answer_question", "session": ev["session"], "id": ev["id"], "answers": ["yes, go ahead"] * max(1, len(ev.get("questions", [])))})
                except OSError:
                    return
        except (OSError, ValueError):
            return

    def mark(self):
        with self.lock:
            return len(self.events)

    def since(self, i):
        with self.lock:
            return [e["event"] for e in self.events[i:]]

    def wait(self, pred, timeout, what, start=0):
        end = time.time() + timeout
        seen = start
        while time.time() < end:
            check_quiet()
            with self.lock:
                evs = self.events[seen:]
                seen += len(evs)
            for e in evs:
                if pred(e["event"]):
                    return e["event"]
            if self.p.poll() is not None:
                raise RuntimeError(f"the daemon exited while waiting for {what}")
            time.sleep(0.15)
        raise TimeoutError(f"timed out waiting for {what}")

    def shutdown(self):
        try:
            self.send({"op": "shutdown"})
        except OSError:
            pass
        for _ in range(60):
            if self.p.poll() is not None:
                break
            time.sleep(0.5)
        if self.p.poll() is None:
            self.p.terminate()
            time.sleep(2)
        if self.p.poll() is None:
            self.p.kill()
        for f in (self.out, self.err):
            f.close()


# ---------------------------------------------------------------- the conversation

LONG_ESSAY = "Write a long, detailed essay (at least 700 words, no lists, no tools) on how KV-cache reuse works in transformer inference and what breaks it."

STEPS = [
    ("ask", "Hi, I am Matthew. First use the glob tool with pattern `**/*` to list this workspace, then use the read tool on README.md and tell me in one sentence what this project is."),
    ("ask", "Use the write tool to create notes/quotes.txt whose content is exactly these three lines: `He said \"hello\" and left.`, `tab\\there and a backslash \\ in it`, `naïve café — 東京 ✓ 100%`. Then use multi_edit on notes/quotes.txt with an edits list of two edits applied in one call: replace `hello` with `goodbye`, and replace `naïve` with `naive`. Then read the file back."),
    ("ask", "Use the read tool on notes/missing.txt (it does not exist, tell me what the tool said). Then use the read tool on notes/trailing.txt with offset 1 and limit 50. Then use grep with pattern `line`, path `notes`, output_mode `content`, -n true, -C 1, -i true and limit 20."),
    ("ask", "Please remember this for later using your remember tool: my favourite dataset is LongMemEval-S, and my GPU is a 16 GB card. Then confirm in one sentence."),
    ("interrupt", LONG_ESSAY, 40),
    ("write", "notes/quotes.txt", "He said \"goodbye\" and left. (edited outside)\nA second line added by hand.\n"),
    ("teams", True),
    ("ask", "I changed notes/quotes.txt myself. Read it again and tell me what is different from before."),
    ("restart",),
    ("ask", "What is my favourite dataset and what GPU do I have? Use your recall tool to check, then list the notes directory with glob pattern `notes/*`."),
    ("steer", "Read README.md, then src/lib.py, then notes/trailing.txt, one tool call at a time, and say what each contains in a few words.", "Also put the word banana in your final answer."),
    ("teams", False),
    ("ask", "In one sentence, summarize everything we did in this conversation."),
]

FILES = {
    "README.md": "# Demo project\n\nA tiny workspace for a cache audit. It has a Python module in src/ and notes in notes/.\n",
    "src/lib.py": "def add(a, b):\n    \"\"\"Add two numbers.\"\"\"\n    return a + b\n\n\ndef greet(name):\n    return f\"hello, {name}\"\n",
    "notes/trailing.txt": "line one   \nline two\t\n\n  \nline five has trailing spaces   \n\n\n",
}


def make_workspace(path):
    shutil.rmtree(path, ignore_errors=True)
    for rel, text in FILES.items():
        p = Path(path) / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_bytes(text.encode("utf-8"))
    env = dict(os.environ, GIT_AUTHOR_NAME="audit", GIT_AUTHOR_EMAIL="audit@example.invalid", GIT_COMMITTER_NAME="audit", GIT_COMMITTER_EMAIL="audit@example.invalid")
    for args in (["init", "-q"], ["add", "-A"], ["commit", "-q", "-m", "start"]):
        subprocess.run(["git", "-C", str(path)] + args, env=env, check=True, capture_output=True, creationflags=NO_WINDOW)


class Run:
    """One (model, engine) run of the whole conversation."""

    def __init__(self, args, model, engine, out_dir, work):
        self.args, self.model, self.engine = args, model, engine
        self.out = Path(out_dir)
        self.work = Path(work)
        self.home = self.work / "home"
        self.ws = self.work / "ws"
        self.tap_log = self.out / "tap.jsonl"
        self.raw = self.out / "raw"
        self.notes = []  # what the script did, in order, with times
        self.restarts = []
        self.turns = []
        self.daemon = None
        self.tap = None
        self.sid = None

    # -- environment
    def env(self):
        e = {k: v for k, v in os.environ.items() if not k.startswith("MARLOWE_")}
        e.update(MARLOWE_HOME=str(self.home), MARLOWE_NO_DESKTOP="1")
        if self.engine == "ollama":
            e["MARLOWE_LLAMA_ENGINE"] = "0"
            e["MARLOWE_OLLAMA_ADDR"] = f"http://127.0.0.1:{self.tap.listen_port}"
        else:
            wrap_dir = self.work / "bin"
            wrap_dir.mkdir(parents=True, exist_ok=True)
            cmd = wrap_dir / "llama-server.cmd"
            cmd.write_text(f'@"{sys.executable}" -I "{HERE / "llama_wrap.py"}" %*\r\n')
            for sub in ("cuda_v13", "cuda_v12"):  # the engine finds its backend DLL beside the binary it starts
                src, dst = OLLAMA_LIB / sub, wrap_dir / sub
                if src.is_dir() and not dst.exists():
                    subprocess.run(["cmd", "/c", "mklink", "/J", str(dst), str(src)], check=True, capture_output=True, creationflags=NO_WINDOW)
            e.update(MARLOWE_LLAMA_SERVER=str(cmd), AUDIT_REAL_SERVER=str(OLLAMA_LIB / "llama-server.exe"), AUDIT_TAP_LOG=str(self.tap_log), AUDIT_TAP_RAW=str(self.raw))
        return e

    def start_daemon(self):
        self.daemon = Daemon(self.args.exe, self.home, self.env(), self.out / f"daemon{len(self.restarts)}")
        log("daemon up on", self.daemon.info["port"], "pid", self.daemon.p.pid)

    # -- script steps
    def note(self, kind, **kw):
        self.notes.append({"t": time.time(), "kind": kind, **kw})

    def open_session(self):
        d = self.daemon
        d.send({"op": "add_model", "provider": "ollama", "model_id": self.model})
        d.wait(lambda e: e.get("ev") == "library_state", 60, "library state")
        time.sleep(1)
        d.send({"op": "set_slot", "slot": "conductor", "model": f"ollama/{self.model}", "effort": "medium"})
        time.sleep(1)
        m = d.mark()
        d.send({"op": "new_session", "workspace": str(self.ws)})
        ev = d.wait(lambda e: e.get("ev") == "session_opened", 60, "session opened", m)
        self.sid = ev["session"]
        d.send({"op": "set_mode", "session": self.sid, "mode": "bypass"})
        time.sleep(0.5)
        log("session", self.sid)

    def finished_after(self, mark, timeout):
        sid = self.sid
        return self.daemon.wait(lambda e: e.get("ev") == "turn_finished" and e.get("session") == sid, timeout, "turn end", mark)

    def tools_since(self, mark):
        evs = self.daemon.since(mark)
        return [(e["tool"], e.get("ok")) for e in evs if e.get("ev") == "tool_started"] and \
               [{"tool": e["tool"], "summary": e.get("summary", "")[:100]} for e in evs if e.get("ev") == "tool_started"]

    def do_ask(self, text, n):
        d = self.daemon
        m = d.mark()
        t0 = time.time()
        d.send({"op": "ask", "session": self.sid, "text": text, "workspace": str(self.ws)})
        end = self.finished_after(m, self.args.turn_timeout)
        evs = self.settle_turns(m, t0)
        self.turns.append({"step": n, "kind": "ask", "text": text[:100], "stop": end.get("stop"), "seconds": round(time.time() - t0, 1),
                           "tools": [{"tool": e["tool"], "summary": e.get("summary", "")[:100]} for e in evs if e.get("ev") == "tool_started"],
                           "tool_results": [bool(e.get("ok")) for e in evs if e.get("ev") == "tool_finished"],
                           "errors": [e.get("message", "")[:160] for e in evs if e.get("ev") == "error"],
                           "model_calls": sum(1 for e in evs if e.get("ev") == "context_usage"),
                           "answer": "".join(e.get("text", "") for e in evs if e.get("ev") == "text_delta")[:160]})
        log(f"step {n}: {len(self.turns[-1]['tools'])} tool calls, {self.turns[-1]['model_calls']} model calls, stop {end.get('stop')}, {self.turns[-1]['seconds']}s")

    def settle_turns(self, mark, t0):
        """Events since `mark` once every turn that started has finished (a queued message can start one more)."""
        time.sleep(1.5)
        evs = self.daemon.since(mark)
        while sum(e.get("ev") == "turn_started" for e in evs) > sum(e.get("ev") == "turn_finished" for e in evs) and time.time() - t0 < self.args.turn_timeout:
            check_quiet()
            time.sleep(0.5)
            evs = self.daemon.since(mark)
        return evs

    def do_interrupt(self, text, after, n):
        d = self.daemon
        m = d.mark()
        t0 = time.time()
        d.send({"op": "ask", "session": self.sid, "text": text, "workspace": str(self.ws)})
        sent = False
        end_t = time.time() + self.args.turn_timeout
        while time.time() < end_t:
            check_quiet()
            evs = d.since(m)
            if any(e.get("ev") == "turn_finished" and e.get("session") == self.sid for e in evs):
                break
            deltas = sum(1 for e in evs if e.get("ev") in ("text_delta", "reasoning_delta"))
            if not sent and deltas >= after:
                d.send({"op": "interrupt", "session": self.sid})
                sent = True
                self.note("interrupt", deltas=deltas)
            time.sleep(0.1)
        evs = d.since(m)
        end = next((e for e in evs if e.get("ev") == "turn_finished"), {})
        self.turns.append({"step": n, "kind": "interrupt", "text": text[:100], "interrupt_sent": sent, "stop": end.get("stop"), "seconds": round(time.time() - t0, 1),
                           "partial_chars": len("".join(e.get("text", "") for e in evs if e.get("ev") == "text_delta")),
                           "model_calls": sum(1 for e in evs if e.get("ev") == "context_usage")})
        log(f"step {n}: interrupt sent={sent}, stop {end.get('stop')}")

    def do_steer(self, text, steer, n):
        d = self.daemon
        m = d.mark()
        t0 = time.time()
        d.send({"op": "ask", "session": self.sid, "text": text, "workspace": str(self.ws)})
        d.wait(lambda e: e.get("ev") == "tool_started", self.args.turn_timeout, "first tool call", m)
        d.send({"op": "ask", "session": self.sid, "text": steer})
        self.note("steer", text=steer)
        end = self.finished_after(m, self.args.turn_timeout)
        evs = self.settle_turns(m, t0)  # the steer may have started its own turn
        self.turns.append({"step": n, "kind": "steer", "text": text[:100], "steer": steer, "stop": end.get("stop"), "seconds": round(time.time() - t0, 1),
                           "turns_started": sum(1 for e in evs if e.get("ev") == "turn_started"),
                           "tools": [{"tool": e["tool"], "summary": e.get("summary", "")[:100]} for e in evs if e.get("ev") == "tool_started"],
                           "model_calls": sum(1 for e in evs if e.get("ev") == "context_usage"),
                           "answer": "".join(e.get("text", "") for e in evs if e.get("ev") == "text_delta")[-160:]})
        log(f"step {n}: steer, {self.turns[-1]['turns_started']} turn(s), {len(self.turns[-1]['tools'])} tool calls")

    def do_restart(self, n):
        t = time.time()
        self.daemon.shutdown()
        self.restarts.append(time.time())
        self.note("restart", step=n)
        time.sleep(2)
        self.start_daemon()
        self.daemon.send({"op": "open_session", "session": self.sid})
        time.sleep(2)
        self.daemon.send({"op": "set_mode", "session": self.sid, "mode": "bypass"})
        time.sleep(0.5)
        log(f"step {n}: daemon restarted in {round(time.time() - t, 1)}s")

    def script(self):
        for n, step in enumerate(STEPS, 1):
            check_quiet()
            kind = step[0]
            if kind == "ask":
                self.do_ask(step[1], n)
            elif kind == "interrupt":
                self.do_interrupt(step[1], step[2], n)
            elif kind == "steer":
                self.do_steer(step[1], step[2], n)
            elif kind == "write":
                p = self.ws / step[1]
                p.write_bytes(step[2].encode("utf-8"))
                self.note("file_change", path=step[1])
            elif kind == "teams":
                self.daemon.send({"op": "set_teams", "session": self.sid, "on": step[1]})
                self.note("teams", on=step[1])
                time.sleep(1)
            elif kind == "restart":
                self.do_restart(n)

    # -- the whole run
    def execute(self):
        for p in (self.out, self.raw):
            p.mkdir(parents=True, exist_ok=True)
        shutil.rmtree(self.home, ignore_errors=True)
        self.home.mkdir(parents=True)
        make_workspace(self.ws)
        self.tap_log.unlink(missing_ok=True)
        if self.engine == "ollama":
            self.tap = tp.Tap(free_port(), OLLAMA, tp.JsonlSink(self.tap_log), log_path=str(OLLAMA_LOG), raw_dir=str(self.raw)).start()
        t0 = time.time()
        try:
            self.start_daemon()
            self.open_session()
            self.script()
        finally:
            try:
                if self.daemon:
                    self.daemon.shutdown()
            finally:
                if self.tap:
                    time.sleep(0.5)
                    self.tap.stop()
                if self.engine == "ollama":
                    unload_ollama(self.model)
        try:
            ollama_version = http_json("http://127.0.0.1:11434/api/version", timeout=5).get("version")
        except Exception:
            ollama_version = None
        meta = {"model": self.model, "engine": self.engine, "ollama_version": ollama_version, "started": t0, "seconds": round(time.time() - t0, 1), "turns": self.turns, "notes": self.notes,
                "restarts": self.restarts, "exe": str(self.args.exe), "ollama_log": str(OLLAMA_LOG)}
        (self.out / "meta.json").write_text(json.dumps(meta, indent=1, default=str), encoding="utf-8")
        return meta


# ---------------------------------------------------------------- go

def run_dir(base, model, engine):
    return Path(base) / f"{model.replace(':', '_').replace('/', '_')}.{engine}"


def engines_for(model, wanted):
    facts = gguf.manifest_facts(model)
    out = []
    for e in wanted:
        if e == "llama" and not gguf.engine_eligible(facts):
            continue
        out.append(e)
    return out


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--go", action="store_true", help="the GPU go-ahead: without it nothing is started")
    ap.add_argument("--models", default=DEFAULT_MODELS)
    ap.add_argument("--engines", default="ollama,llama")
    ap.add_argument("--out", default="", help="results directory (default eval/prompt_cache/out/<stamp>)")
    ap.add_argument("--work", default="", help="throwaway homes and workspaces (default a temp dir)")
    ap.add_argument("--exe", default=str(ROOT / "target" / "release" / "marlowe.exe"))
    ap.add_argument("--turn-timeout", type=int, default=900)
    ap.add_argument("--analyze", metavar="DIR", help="judge the runs under DIR again; nothing is started")
    ap.add_argument("--templates", action="store_true", help="read each model's chat template and test what it does with old reasoning (CPU only)")
    args = ap.parse_args()

    if args.analyze:
        rp.write_all(Path(args.analyze))
        return 0
    models = [m for m in args.models.split(",") if m]
    if args.templates:
        out = Path(args.out) if args.out else HERE / "out" / "templates"
        out.mkdir(parents=True, exist_ok=True)
        import templates as tm
        tm.run(models, out)
        return 0
    wanted = [e for e in args.engines.split(",") if e]
    plan = [(m, e) for m in models for e in engines_for(m, wanted)]
    for m, e in plan:
        print(f"  {m} on {e}")
    if not args.go:
        print("not started: this loads the models onto the GPU; run again with --go once it is free")
        return 0
    base = Path(args.out) if args.out else HERE / "out" / time.strftime("%Y%m%d-%H%M%S")
    work_base = Path(args.work) if args.work else Path(tempfile.gettempdir()) / "marlowe-prompt-cache"
    base.mkdir(parents=True, exist_ok=True)
    for model, engine in plan:
        for attempt in range(3):
            try:
                check_quiet()
                r = Run(args, model, engine, run_dir(base, model, engine), work_base / f"{model.replace(':', '_')}.{engine}")
                log(f"=== {model} on {engine}")
                r.execute()
                break
            except Quiet:
                log("cargo-quiet present: GPU work stopped; waiting for it to go")
                while QUIET.exists():
                    time.sleep(10)
                log("cargo-quiet gone; starting this run again")
            except Exception as ex:
                log("RUN FAILED", model, engine, repr(ex)[:300])
                (run_dir(base, model, engine) / "failed.txt").write_text(repr(ex), encoding="utf-8")
                break
        unload_ollama(model)
    rp.write_all(base)
    log("report:", base / "report.md")
    return 0


if __name__ == "__main__":
    sys.exit(main())
