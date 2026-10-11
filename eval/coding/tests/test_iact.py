"""The interactive suite: the scenario file, the checks, and the driver against stand-in agents."""

from __future__ import annotations

import json
from pathlib import Path

from mcoding import iact
from mcoding.iact import Turn, evaluate, load_scenarios

from helpers import CODING, fake_cc, fake_marlowe, records

SCENARIOS = CODING / "iact" / "scenarios.json"


def test_the_scenario_file_has_fifteen_well_formed_scenarios():
    scns = load_scenarios(SCENARIOS)
    assert len(scns) == 15
    assert len({s["id"] for s in scns}) == 15
    assert [s["id"] for s in scns] == sorted(s["id"] for s in scns)
    assert sum(1 for s in scns if s.get("marlowe_only")) == 1, "only the rewind scenario is Marlowe-only"
    for s in scns:
        assert s["steps"] and s["checks"], s["id"]
        for step in s["steps"]:
            assert ("say" in step) != bool(step.get("restart")), f"{s['id']}: a step is a message or a restart"
        for c in s["checks"]:
            assert sum(k in c for k in ("file", "run", "final_matches", "questions")) == 1, (s["id"], c)
    kinds = {k for s in scns for st in s["steps"] for k in ("answers", "steer", "interrupt_after_tool_calls", "mode", "deny_tools", "restart") if k in st}
    assert kinds == {"answers", "steer", "interrupt_after_tool_calls", "mode", "deny_tools", "restart"}, "every kind of step is exercised"


def run(scn, adapter, tmp_path, **kw):
    return iact.run_scenario(scn, adapter, tmp_path / "out", run_id="t", idle_timeout=30, **kw)


ASK = {"questions": [{"question": "Which backend should I use?", "header": "Backend", "options": [{"label": "postgres"}, {"label": "mysql"}], "multiSelect": False}], "write_answer": {"path": "choice.txt", "template": "backend={answer}\n"}}
SCN_ASK = {
    "id": "t-ask",
    "files": {"app.cfg": "backend=sqlite\n"},
    "steps": [{"say": "pick a backend", "answers": {"backend": "mysql"}}],
    "checks": [{"file": "choice.txt", "contains": ["backend=mysql"]}, {"questions": {"min": 1, "max": 1}}, {"file": "app.cfg", "unchanged": True}],
}


def test_an_ask_user_question_is_answered_in_each_harnesss_shape(tmp_path):
    for make, shape in ((fake_marlowe, "list"), (fake_cc, "map")):
        home = tmp_path / shape
        home.mkdir()
        a, rec_file = make(home, [{"ask": ASK, "tools": ["Write"], "result": "done"}])
        rec = iact.run_scenario(SCN_ASK, a, home / "out", run_id="t", idle_timeout=30)
        assert rec.passed, (shape, rec.test_output_tail)
        assert rec.agent["questions_asked"] == 1
        argv = records(rec_file)[0]["argv"]
        assert "--input-format" in argv and "stream-json" in argv and "--permission-prompt-tool" in argv


def test_a_default_answer_is_the_first_option_and_free_text_is_taken_as_given():
    q = {"question": "Which?", "options": [{"label": "alpha"}, {"label": "beta"}]}
    assert iact.pick_answer(q, {}) == "alpha"
    assert iact.pick_answer(q, {"which": "beta"}) == "beta"
    assert iact.pick_answer(q, {"WHICH": ["a", "b"]}) == "a, b"
    assert iact.pick_answer({"question": "Name?"}, {}) == "yes"
    inp = {"questions": [q, {"question": "Second?", "options": []}]}
    assert iact.answer_input(inp, {"second": "x"}, "list")["answers"] == ["alpha", "x"]
    assert iact.answer_input(inp, {"second": "x"}, "map")["answers"] == {"Which?": "alpha", "Second?": "x"}


def test_a_steer_message_reaches_the_agent_while_the_turn_runs(tmp_path):
    scn = {
        "id": "t-steer",
        "steps": [{"say": "write a", "steer": {"after_tool_calls": 1, "say": "actually b"}}],
        "checks": [{"file": "b.txt", "contains": ["beta"]}, {"file": "a.txt", "exists": False}],
    }
    a, _ = fake_marlowe(tmp_path, [{"tools": ["Write", "Write", "Write"], "steer_writes": {"b.txt": "beta"}, "beat": 0.4}])
    rec = run(scn, a, tmp_path)
    assert rec.passed, rec.test_output_tail
    assert rec.agent["steered"] is True and rec.agent["tool_calls"] == 3


def test_an_interrupt_ends_the_turn_and_the_next_message_goes_on_in_the_same_session(tmp_path):
    scn = {
        "id": "t-int",
        "steps": [{"say": "make thirty files", "interrupt_after_tool_calls": 2}, {"say": "carry on"}],
        "checks": [{"file": "after.txt", "contains": ["resumed"]}],
    }
    a, rec_file = fake_marlowe(tmp_path, [{"tools": ["Write"] * 10, "beat": 0.2}, {"writes": {"after.txt": "resumed"}}])
    rec = run(scn, a, tmp_path)
    assert rec.passed, rec.test_output_tail
    assert rec.agent["interrupted"] is True
    assert len(records(rec_file)) == 1, "one process served both turns"


def test_a_denied_tool_is_answered_with_a_deny_and_an_allowed_one_with_an_allow(tmp_path):
    scn = {
        "id": "t-deny",
        "steps": [{"say": "run it", "deny_tools": ["Bash"]}, {"say": "run the editor"}],
        "checks": [{"file": "d1.txt", "contains": ["deny"]}, {"file": "d2.txt", "contains": ["allow"]}],
    }
    a, _ = fake_marlowe(tmp_path, [{"approve": {"tool": "Bash", "path": "d1.txt"}}, {"approve": {"tool": "Edit", "path": "d2.txt"}}])
    rec = run(scn, a, tmp_path)
    assert rec.passed, rec.test_output_tail
    assert rec.agent["denials"] == 1


def test_a_restart_step_opens_a_new_process_that_continues_the_session(tmp_path):
    scn = {
        "id": "t-restart",
        "steps": [{"say": "one"}, {"restart": True}, {"say": "two"}],
        "checks": [{"file": "x.txt", "contains": ["1"]}],
    }
    a, rec_file = fake_marlowe(tmp_path, [{"writes": {"x.txt": "1"}}])
    rec = run(scn, a, tmp_path)
    assert rec.passed, rec.test_output_tail
    rows = records(rec_file)
    assert len(rows) == 2 and "-c" not in rows[0]["argv"] and "-c" in rows[1]["argv"]


def test_the_rewind_scenario_is_not_scored_against_claude_code(tmp_path):
    scn = next(s for s in load_scenarios(SCENARIOS) if s.get("marlowe_only"))
    a, _ = fake_cc(tmp_path, [{}])
    rec = iact.run_scenario(scn, a, tmp_path / "out", run_id="t")
    assert rec.graded is False and "Marlowe-only" in rec.skip_reason


def test_a_harness_that_dies_mid_conversation_fails_the_scenario_without_hanging(tmp_path):
    scn = {"id": "t-die", "steps": [{"say": "go"}], "checks": [{"file": "x.txt"}]}
    a, _ = fake_marlowe(tmp_path, [{"tools": ["Write"], "exit": 3, "beat": 0.1}])
    # The session fake finishes the turn normally; make the binary itself vanish instead.
    a.exe = str(tmp_path / "no-such-binary.exe")
    a.prefix = []
    rec = run(scn, a, tmp_path)
    assert rec.passed is False and rec.agent["error"]


def test_the_checks(tmp_path):
    (tmp_path / "a.txt").write_text("one\ntwo\n", encoding="utf-8")
    (tmp_path / "s.py").write_text("print('x')\nprint('y')\n", encoding="utf-8")
    turns = [Turn(result={"result": "3 passed in 0.1s"}, asked=["q1", "q2"])]
    originals = {"a.txt": "one\ntwo\n"}
    checks = [
        {"file": "a.txt", "contains": ["one"], "not_contains": ["three"], "lines": ["one", "two"], "unchanged": True},
        {"file": "missing.txt", "exists": False},
        {"file": "missing.txt"},
        {"run": ["python", "s.py"], "stdout_lines": ["x", "y"]},
        {"run": ["python", "s.py"], "stdout_lines": ["x"]},
        {"final_matches": r"\d+ passed"},
        {"questions": {"min": 2, "max": 2}},
        {"questions": {"max": 1}},
    ]
    got = [ok for _, ok in evaluate(checks, tmp_path, turns, originals)]
    assert got == [True, True, True, True, True, False, True, False, True, True, False]
