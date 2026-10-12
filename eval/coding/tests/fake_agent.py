"""A stand-in for ``marlowe -p`` / ``claude -p`` that speaks the same stream-json, for the harness tests.

Behaviour comes from ``FAKE_PLAN`` (a JSON file path): a list of turn plans (the last repeats).
A turn plan: ``writes`` {path: text} applied at the end, ``tools`` [names] emitted as tool calls
(one second-long beat each unless ``beat``), ``ask`` {questions, write_answer: {path, template}},
``steer_writes`` {path: text} applied if a user message arrives mid-turn, ``result`` text,
``fail_tool`` true to make the last tool result an error, ``sleep`` seconds before anything,
``exit`` code. What it was started with is appended to ``FAKE_RECORD`` (argv, a few env vars, stdin).
"""

import json
import os
import queue
import sys
import threading
import time
from pathlib import Path


def emit(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


def load_plans():
    p = os.environ.get("FAKE_PLAN")
    plans = json.loads(Path(p).read_text(encoding="utf-8")) if p else [{}]
    return plans if isinstance(plans, list) else [plans]


def record(stdin_text):
    p = os.environ.get("FAKE_RECORD")
    if not p:
        return
    keys = ["MARLOWE_TOOL_GROUPS", "MARLOWE_HOME", "MARLOWE_EVAL_STATS", "CLAUDE_CONFIG_DIR", "ANTHROPIC_BASE_URL", "ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "OPENROUTER_API_KEY", "HTTPS_PROXY"]
    row = {"argv": sys.argv[1:], "cwd": os.getcwd(), "env": {k: os.environ.get(k) for k in keys}, "stdin": stdin_text}
    with open(p, "a", encoding="utf-8") as f:
        f.write(json.dumps(row) + "\n")


def result_line(plan, n_tools, errors):
    return {
        "type": "result",
        "subtype": plan.get("subtype", "success"),
        "is_error": bool(plan.get("is_error", False)),
        "duration_ms": 12,
        "duration_api_ms": 7,
        "num_turns": plan.get("num_turns", n_tools + 1),
        "result": plan.get("result", "done"),
        "session_id": "fake",
        "total_cost_usd": 0.0,
        "usage": {"input_tokens": 100, "output_tokens": 20, "cache_read_input_tokens": 5},
        "exit_code": plan.get("exit", 0),
    }


def apply_writes(writes):
    for rel, text in (writes or {}).items():
        p = Path(rel)
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(text, encoding="utf-8", newline="")


def one_shot(plan, prompt):
    time.sleep(plan.get("sleep", 0))
    emit({"type": "system", "subtype": "init", "session_id": "fake", "model": "fake"})
    tools = plan.get("tools", [])
    for i, name in enumerate(tools):
        emit({"type": "assistant", "message": {"role": "assistant", "content": [{"type": "tool_use", "id": f"t{i}", "name": name, "input": {}}]}})
        bad = plan.get("fail_tool") and i == len(tools) - 1
        emit({"type": "user", "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": f"t{i}", "content": "x", "is_error": bool(bad)}]}})
    apply_writes(plan.get("writes"))
    emit(result_line(plan, len(tools), 0))
    stats = os.environ.get("MARLOWE_EVAL_STATS")
    if stats:
        Path(stats).write_text(json.dumps({"calls": 3, "repaired": 1, "from_text": 1}), encoding="utf-8")
    sys.exit(plan.get("exit", 0))


class Session:
    def __init__(self, plans):
        self.plans = plans
        self.q = queue.Queue()
        self.turn = 0
        threading.Thread(target=self.read, daemon=True).start()

    def read(self):
        for line in sys.stdin:
            line = line.strip()
            if line:
                self.q.put(json.loads(line))
        self.q.put(None)

    def drain(self):
        """Control messages and mid-turn user messages that have arrived."""
        got = []
        while True:
            try:
                m = self.q.get_nowait()
            except queue.Empty:
                return got
            got.append(m)

    def handle_control(self, m):
        req = m.get("request", {})
        if req.get("subtype") in ("set_permission_mode", "initialize"):
            emit({"type": "control_response", "response": {"subtype": "success", "request_id": m["request_id"], "response": {}}})
            return None
        if req.get("subtype") == "interrupt":
            return "interrupt"
        return None

    def run(self):
        while True:
            m = self.q.get()
            if m is None:
                return
            if m.get("type") == "control_request":
                self.handle_control(m)
                continue
            if m.get("type") != "user":
                continue
            plan = self.plans[min(self.turn, len(self.plans) - 1)]
            self.turn += 1
            self.do_turn(plan, m)

    def do_turn(self, plan, msg):
        emit({"type": "system", "subtype": "init", "session_id": "fake", "model": "fake"})
        ask = plan.get("ask")
        answer = ""
        if ask:
            rid = "ask-1"
            emit({"type": "control_request", "request_id": rid, "request": {"subtype": "can_use_tool", "tool_name": "AskUserQuestion", "input": {"questions": ask["questions"]}}})
            while True:
                r = self.q.get()
                if r is None:
                    return
                if r.get("type") == "control_response" and r["response"].get("request_id") == rid:
                    upd = r["response"].get("response", {}).get("updatedInput", {})
                    a = upd.get("answers")
                    answer = ", ".join(a.values()) if isinstance(a, dict) else ", ".join(a or [])
                    break
            wa = ask.get("write_answer")
            if wa:
                apply_writes({wa["path"]: wa["template"].format(answer=answer)})
        appr = plan.get("approve")
        if appr:
            rid = "perm-1"
            emit({"type": "control_request", "request_id": rid, "request": {"subtype": "can_use_tool", "tool_name": appr["tool"], "input": {"command": "echo hi"}}})
            while True:
                r = self.q.get()
                if r is None:
                    return
                if r.get("type") == "control_response" and r["response"].get("request_id") == rid:
                    apply_writes({appr["path"]: r["response"].get("response", {}).get("behavior", "none")})
                    break
        tools = plan.get("tools", [])
        interrupted = False
        steered = False
        for i, name in enumerate(tools):
            emit({"type": "assistant", "message": {"role": "assistant", "content": [{"type": "tool_use", "id": f"t{i}", "name": name, "input": {}}]}})
            time.sleep(plan.get("beat", 0.25))
            for extra in self.drain():
                if extra.get("type") == "control_request":
                    if self.handle_control(extra) == "interrupt":
                        interrupted = True
                elif extra.get("type") == "user":
                    steered = True
            if interrupted:
                break
            emit({"type": "user", "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": f"t{i}", "content": "x", "is_error": False}]}})
        if interrupted:
            emit({"type": "result", "subtype": "error_during_execution", "is_error": True, "num_turns": 1, "result": "interrupted", "exit_code": 130, "usage": {}})
            return
        apply_writes(plan.get("writes"))
        if steered:
            apply_writes(plan.get("steer_writes"))
        emit(result_line(plan, len(tools), 0))


def main():
    plans = load_plans()
    argv = sys.argv[1:]
    streaming = "--input-format" in argv and argv[argv.index("--input-format") + 1] == "stream-json"
    if streaming:
        record("<stream>")
        Session(plans).run()
        return
    prompt = sys.stdin.read()
    record(prompt)
    n = 0
    flag = os.environ.get("FAKE_COUNTER")
    if flag and Path(flag).is_file():
        n = int(Path(flag).read_text() or 0)
    if flag:
        Path(flag).write_text(str(n + 1))
    one_shot(plans[min(n, len(plans) - 1)], prompt)


if __name__ == "__main__":
    main()
