"""The interactive suite (``iact``, design section 6): scripted multi-turn conversations on both harnesses.

Each scenario is a small workspace, a list of steps and a list of checks (``iact/scenarios.json``).
A step sends a user message and waits for the turn to end; while it runs the driver answers the
harness the way a person would: ``ask_user`` questions with the scenario's answers, tool approvals
with allow or deny, and, when a step says so, a steering message partway through, an interrupt, or a
permission-mode change. Both harnesses speak Claude Code's stream-json control protocol, so one
driver serves both; the one thing that differs is the shape of an answer (Marlowe takes a list in
question order, Claude Code a map from the question's text).

A scenario passes when every check holds. Scenarios marked ``marlowe_only`` (rewind) run on Marlowe
and are not scored against Claude Code.
"""

from __future__ import annotations

import json
import queue
import re
import subprocess
import threading
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Optional

from .adapters import Adapter
from .cc import Invocation
from .model import RunRecord
from .stream import tool_uses
from .util import kill_tree, popen, read_json, rmtree, run_cmd
from .workspace import init_baseline

MODE_SPELLINGS = {"default": "default", "plan": "plan", "accept_edits": "acceptEdits", "bypass": "bypassPermissions", "dont_ask": "dontAsk"}


class Session:
    """One ``-p --input-format stream-json`` process and its two directions of JSON lines."""

    def __init__(self, inv: Invocation, *, idle_timeout: float = 900.0) -> None:
        self.idle_timeout = idle_timeout
        self.p = popen(inv.argv, cwd=inv.cwd, env=inv.env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        self.q: "queue.Queue[Optional[dict]]" = queue.Queue()
        self.stderr = ""
        self._lock = threading.Lock()
        self._rid = 0
        self._threads = [
            threading.Thread(target=self._read_out, daemon=True),
            threading.Thread(target=self._read_err, daemon=True),
        ]
        for t in self._threads:
            t.start()

    def _read_out(self) -> None:
        assert self.p.stdout
        for raw in self.p.stdout:
            raw = raw.strip()
            if raw.startswith("{"):
                try:
                    self.q.put(json.loads(raw))
                except json.JSONDecodeError:
                    pass
        self.q.put(None)

    def _read_err(self) -> None:
        assert self.p.stderr
        for raw in self.p.stderr:
            self.stderr += raw

    def alive(self) -> bool:
        return self.p.poll() is None

    def write(self, obj: dict) -> bool:
        assert self.p.stdin
        with self._lock:
            try:
                self.p.stdin.write(json.dumps(obj) + "\n")
                self.p.stdin.flush()
                return True
            except (BrokenPipeError, OSError, ValueError):
                return False

    def send_user(self, text: str) -> bool:
        return self.write({"type": "user", "message": {"role": "user", "content": text}})

    def control(self, request: dict) -> str:
        self._rid += 1
        rid = f"h{self._rid}"
        self.write({"type": "control_request", "request_id": rid, "request": request})
        return rid

    def respond(self, rid: str, response: dict) -> None:
        self.write({"type": "control_response", "response": {"subtype": "success", "request_id": rid, "response": response}})

    def next_event(self) -> Optional[dict]:
        """The next line, or None at the end of the stream; raises TimeoutError after the idle limit."""
        try:
            return self.q.get(timeout=self.idle_timeout)
        except queue.Empty:
            raise TimeoutError(f"no output for {self.idle_timeout:.0f}s") from None

    def close(self) -> int:
        try:
            if self.p.stdin and not self.p.stdin.closed:
                self.p.stdin.close()
        except OSError:
            pass
        try:
            code = self.p.wait(timeout=60)
        except subprocess.TimeoutExpired:
            kill_tree(self.p.pid)
            code = self.p.wait()
        for t in self._threads:
            t.join(timeout=5)
        for pipe in (self.p.stdout, self.p.stderr):
            try:
                if pipe:
                    pipe.close()
            except OSError:
                pass
        return code


@dataclass
class Turn:
    events: list = field(default_factory=list)
    result: Optional[dict] = None
    asked: list = field(default_factory=list)
    denied: list = field(default_factory=list)
    steered: bool = False
    interrupted: bool = False
    died: bool = False
    error: str = ""

    @property
    def text(self) -> str:
        return str((self.result or {}).get("result", ""))


def pick_answer(question: dict, answers: dict) -> str:
    """The scenario's answer for ``question``: the first key found in its text, else its first option."""
    text = str(question.get("question", "")).lower()
    for key, value in answers.items():
        if key.lower() in text:
            return ", ".join(value) if isinstance(value, list) else str(value)
    opts = question.get("options") or []
    if opts:
        first = opts[0]
        return str(first.get("label", first)) if isinstance(first, dict) else str(first)
    return "yes"


def answer_input(inp: dict, answers: dict, fmt: str) -> dict:
    qs = inp.get("questions") or []
    picked = [pick_answer(q, answers) for q in qs]
    if fmt == "map":
        return {**inp, "answers": {str(q.get("question", "")): a for q, a in zip(qs, picked)}}
    return {**inp, "answers": picked}


def run_turn(sess: Session, step: dict, *, answer_fmt: str, mode: Optional[str] = None) -> Turn:
    """Sends the step's message and drives the harness until the turn's ``result``."""
    t = Turn()
    answers = step.get("answers", {})
    denied_tools = {n.lower() for n in step.get("deny_tools", [])}
    steer = step.get("steer")
    interrupt_after = step.get("interrupt_after_tool_calls")
    pending_mode: Optional[str] = None
    if mode:
        pending_mode = sess.control({"subtype": "set_permission_mode", "mode": MODE_SPELLINGS.get(mode, mode)})
    if not sess.send_user(step["say"]):
        t.died = True
        t.error = "the harness is not reading stdin: " + sess.stderr.strip()[-200:]
        return t
    calls = 0
    while True:
        try:
            ev = sess.next_event()
        except TimeoutError as e:
            t.error = str(e)
            kill_tree(sess.p.pid)
            t.died = True
            return t
        if ev is None:
            t.died = True
            t.error = t.error or ("the harness ended before the turn finished: " + sess.stderr.strip()[-200:])
            return t
        kind = ev.get("type")
        if kind == "control_request":
            req = ev.get("request", {})
            rid = ev.get("request_id", "")
            if req.get("subtype") == "can_use_tool":
                tool, inp = str(req.get("tool_name", "")), req.get("input", {}) or {}
                if tool == "AskUserQuestion":
                    t.asked += [str(q.get("question", "")) for q in inp.get("questions", [])]
                    sess.respond(rid, {"behavior": "allow", "updatedInput": answer_input(inp, answers, answer_fmt)})
                elif tool.lower() in denied_tools:
                    t.denied.append(tool)
                    sess.respond(rid, {"behavior": "deny", "message": f"{tool} is not available in this session"})
                else:
                    sess.respond(rid, {"behavior": "allow", "updatedInput": inp})
            else:
                sess.write({"type": "control_response", "response": {"subtype": "error", "request_id": rid, "error": f"unsupported {req.get('subtype')!r}"}})
            continue
        t.events.append(ev)
        if kind == "assistant":
            calls += len(tool_uses([ev]))
            if steer and not t.steered and calls >= int(steer.get("after_tool_calls", 1)):
                sess.send_user(steer["say"])
                t.steered = True
            if interrupt_after is not None and not t.interrupted and calls >= int(interrupt_after):
                sess.control({"subtype": "interrupt"})
                t.interrupted = True
        if kind == "control_response" and pending_mode and ev.get("response", {}).get("request_id") == pending_mode:
            pending_mode = None
        if kind == "result":
            t.result = ev
            return t


# ---- checks -----------------------------------------------------------------------------


def _read(ws: Path, rel: str) -> Optional[str]:
    p = ws / rel
    return p.read_text(encoding="utf-8", errors="replace") if p.is_file() else None


def evaluate(checks: list, ws: Path, turns: list, originals: dict) -> list:
    """``[(description, ok)]`` for each check of the scenario."""
    out = []
    asked = sum(len(t.asked) for t in turns)
    final = turns[-1].text if turns else ""
    for c in checks:
        if "file" in c:
            text = _read(ws, c["file"])
            if c.get("exists") is False:
                out.append((f"{c['file']} absent", text is None))
                continue
            if text is None:
                out.append((f"{c['file']} exists", False))
                continue
            for needle in c.get("contains", []):
                out.append((f"{c['file']} contains {needle!r}", needle in text))
            for needle in c.get("not_contains", []):
                out.append((f"{c['file']} lacks {needle!r}", needle not in text))
            if "lines" in c:
                out.append((f"{c['file']} lines {c['lines']}", [l.strip() for l in text.splitlines() if l.strip()] == c["lines"]))
            if c.get("unchanged"):
                out.append((f"{c['file']} unchanged", text == originals.get(c["file"])))
            if "exists" not in c and not any(k in c for k in ("contains", "not_contains", "lines", "unchanged")):
                out.append((f"{c['file']} exists", True))
        elif "run" in c:
            r = run_cmd(c["run"], cwd=ws, timeout=60)
            want = c.get("stdout_lines")
            ok = r.code == 0 and (want is None or [l.strip() for l in r.out.splitlines() if l.strip()] == want)
            out.append((f"{' '.join(c['run'])} -> {want if want is not None else 'exit 0'}", ok))
        elif "final_matches" in c:
            out.append((f"final answer matches /{c['final_matches']}/", bool(re.search(c["final_matches"], final, re.I | re.S))))
        elif "questions" in c:
            lo, hi = c["questions"].get("min", 0), c["questions"].get("max", 99)
            out.append((f"asked {lo}..{hi} questions (asked {asked})", lo <= asked <= hi))
    return out


def load_scenarios(path) -> list:
    d = read_json(path)
    return d["scenarios"] if isinstance(d, dict) else d


def run_scenario(scn: dict, adapter: Adapter, out_dir: Path, *, run_id: str, seed: int = 0, idle_timeout: float = 900.0, keep: bool = False) -> RunRecord:
    base = dict(run=run_id, task=scn["id"], tier="iact", arm=adapter.name, seed=seed)
    if scn.get("marlowe_only") and not adapter.supports_rewind:
        return RunRecord(**base, passed=False, graded=False, skip_reason="Marlowe-only step (rewind); not scored against Claude Code")
    fmt = "list" if adapter.supports_rewind else "map"
    ws = out_dir / "work" / adapter.name / f"{scn['id']}-s{seed}"
    run_dir = out_dir / "runs" / adapter.name / f"{scn['id']}-s{seed}"
    rmtree(ws)
    ws.mkdir(parents=True)
    originals = {}
    for rel, text in scn.get("files", {}).items():
        (ws / rel).parent.mkdir(parents=True, exist_ok=True)
        (ws / rel).write_text(text, encoding="utf-8", newline="")
        originals[rel] = text
    init_baseline(ws)
    run_dir.mkdir(parents=True, exist_ok=True)
    turns: list = []
    sess: Optional[Session] = None
    started = time.monotonic()
    error = ""
    try:
        for i, step in enumerate(scn["steps"]):
            if step.get("restart") or sess is None or not sess.alive():
                if sess is not None:
                    sess.close()
                sess = Session(adapter.session_invocation(ws, run_dir=run_dir, tag=f"s{i}", continue_session=bool(turns)), idle_timeout=idle_timeout)
                if step.get("restart"):
                    continue
            turn = run_turn(sess, step, answer_fmt=fmt, mode=step.get("mode"))
            turns.append(turn)
            if turn.error:
                error = turn.error
                break
    except Exception as e:  # a scenario that crashes the driver is a failed scenario
        error = f"{type(e).__name__}: {e}"
    finally:
        if sess is not None:
            sess.close()
    checks = evaluate(scn.get("checks", []), ws, turns, originals) if not error else [("the conversation ran to the end", False)]
    passed = bool(checks) and all(ok for _, ok in checks)
    (run_dir / "checks.json").write_text(json.dumps([{"check": d, "ok": ok} for d, ok in checks], indent=2), encoding="utf-8")
    (run_dir / "events.jsonl").write_text("\n".join(json.dumps(e) for t in turns for e in t.events), encoding="utf-8")
    agent = {
        "seconds": time.monotonic() - started,
        "steps": sum(int((t.result or {}).get("num_turns") or 0) for t in turns),
        "tool_calls": sum(len(tool_uses(t.events)) for t in turns),
        "questions_asked": sum(len(t.asked) for t in turns),
        "denials": sum(len(t.denied) for t in turns),
        "steered": any(t.steered for t in turns),
        "interrupted": any(t.interrupted for t in turns),
        "error": error,
    }
    if not keep:
        rmtree(ws)
    return RunRecord(
        **base,
        passed=passed,
        test_output_tail="\n".join(f"{'ok  ' if ok else 'FAIL'} {d}" for d, ok in checks),
        agent=agent,
        meta={"turns": len(turns)},
    )
