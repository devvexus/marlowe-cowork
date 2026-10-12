"""H6: an SDK-style client drives a turn over stream-json.

The hermetic test talks to a stand-in that speaks the protocol (so it runs anywhere); the second
runs the compiled debug binary with the scripted provider when MARLOWE_BIN names it.
"""

import json
import os
import sys
import textwrap
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
from marlowe_client import MarloweClient, MarloweError, query  # noqa: E402

FAKE = textwrap.dedent(
    '''
    import json, sys
    def out(o): print(json.dumps(o), flush=True)
    for raw in sys.stdin:
        m = json.loads(raw)
        if m["type"] == "control_request":
            sub = m["request"]["subtype"]
            if sub == "initialize":
                out({"type": "control_response", "response": {"subtype": "success", "request_id": m["request_id"], "response": {"commands": []}}})
            elif sub == "interrupt":
                out({"type": "control_response", "response": {"subtype": "success", "request_id": m["request_id"], "response": {}}})
            continue
        text = m["message"]["content"]
        out({"type": "system", "subtype": "init", "session_id": "s1", "model": "fake/m", "permissionMode": "default"})
        if "tool" in text:
            out({"type": "control_request", "request_id": "p1", "request": {"subtype": "can_use_tool", "tool_name": "Bash", "input": {"command": "rm -rf x"}}})
            answer = json.loads(sys.stdin.readline())["response"]["response"]
            out({"type": "assistant", "message": {"role": "assistant", "content": [{"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "rm -rf x"}}]}})
            text = "decision: " + answer["behavior"]
        out({"type": "assistant", "message": {"role": "assistant", "content": [{"type": "text", "text": "echo " + text}]}})
        out({"type": "result", "subtype": "success", "is_error": False, "result": "echo " + text, "session_id": "s1",
             "total_cost_usd": 0.25, "exit_code": 0, "permission_denials": [], "usage": {"input_tokens": 3, "output_tokens": 4}})
    '''
)


@pytest.fixture
def fake(tmp_path):
    script = tmp_path / "fake_marlowe.py"
    script.write_text(FAKE, encoding="utf-8")
    return script


def client(fake, **kw):
    c = MarloweClient(marlowe=sys.executable, **kw)
    # The stand-in ignores flags: run it as `python fake.py`.
    c._argv = [sys.executable, str(fake)]
    return c


def test_parity_h6_client_drives_two_turns_in_one_process(fake):
    with client(fake) as m:
        a = m.ask("hello")
        b = m.ask("again")
    assert (a.text, b.text) == ("echo hello", "echo again")
    assert a.exit_code == 0 and a.session_id == "s1" and a.cost_usd == 0.25
    assert a.usage == {"input_tokens": 3, "output_tokens": 4}
    assert any(e.get("subtype") == "init" for e in a.events)


def test_parity_h6_can_use_tool_is_answered_by_the_callback(fake):
    seen = []

    def gate(tool, tool_input):
        seen.append((tool, tool_input))
        return {"behavior": "deny", "message": "no"}

    with client(fake, can_use_tool=gate) as m:
        t = m.ask("run a tool")
    assert seen == [("Bash", {"command": "rm -rf x"})]
    assert t.text == "echo decision: deny"
    assert t.tool_uses() == [("Bash", {"command": "rm -rf x"})]


def test_parity_h6_a_handler_that_raises_is_a_refusal_not_a_hang(fake):
    def boom(tool, tool_input):
        raise ValueError("bug")

    # The stand-in reads one line for the answer; an error reply has no ["response"]["response"].
    with client(fake, can_use_tool=boom) as m:
        with pytest.raises(MarloweError):
            m.ask("run a tool")


def test_parity_h6_a_process_that_exits_early_is_an_error_with_its_stderr(tmp_path):
    script = tmp_path / "dies.py"
    script.write_text("import sys; sys.stderr.write('boom'); sys.exit(1)", encoding="utf-8")
    c = MarloweClient()
    c._argv = [sys.executable, str(script)]
    with pytest.raises(MarloweError):
        c.start()
    c.close()


BIN = os.environ.get("MARLOWE_BIN")


@pytest.mark.skipif(not BIN, reason="set MARLOWE_BIN to the debug marlowe binary")
def test_parity_h6_against_the_binary_with_the_scripted_provider(tmp_path):
    script = tmp_path / "script.json"
    script.write_text(json.dumps({"rules": [{"name": "any", "reply": "scripted hello"}]}), encoding="utf-8")
    ws = tmp_path / "ws"
    ws.mkdir()
    env = {"MARLOWE_SCRIPTED_PROVIDER": str(script), "MARLOWE_HOME": str(tmp_path / "home")}
    with MarloweClient(marlowe=BIN, cwd=str(ws), env=env, standalone=True, model="ollama/scripted", args=["--no-memory"]) as m:
        t = m.ask("hi")
    assert t.text == "scripted hello" and t.exit_code == 0 and not t.is_error
    one = query("hi", marlowe=BIN, cwd=str(ws), env=env, args=["--standalone", "--no-memory", "--model", "ollama/scripted"])
    assert one.text == "scripted hello"
