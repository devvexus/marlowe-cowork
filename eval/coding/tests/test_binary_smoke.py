"""The harness against the real ``marlowe`` binary on its scripted provider: no model, no GPU, no network.

Set ``MARLOWE_BIN`` to a debug build of this tree (a release build does not compile the scripted
provider) to run these; they are skipped otherwise.
"""

from __future__ import annotations

import json
import os
import subprocess
from pathlib import Path

import pytest

from mcoding import cli, iact
from mcoding.arms import ArmConfig, make_adapter, seed_marlowe_home
from mcoding.model import load_records

MARLOWE = os.environ.get("MARLOWE_BIN")
pytestmark = pytest.mark.skipif(not MARLOWE or not Path(MARLOWE).is_file(), reason="MARLOWE_BIN is not set to a debug build")


def write_script(tmp: Path, rules: list) -> str:
    p = tmp / "script.json"
    p.write_text(json.dumps({"log": str(tmp / "requests.jsonl"), "rules": rules}), encoding="utf-8")
    return str(p)


CLAMP_RULES = [
    {"name": "read", "tool_results": 0, "calls": [{"name": "read", "args": {"path": "util.py"}}]},
    {
        "name": "edit",
        "tool_results": 1,
        "calls": [{"name": "edit", "args": {"path": "util.py", "old_string": "    return min(low, max(x, high))", "new_string": "    return max(low, min(x, high))"}}],
    },
    {"name": "done", "tool_results": 2, "reply": "fixed it"},
]


def test_the_marlowe_cli_command_resolves_to_the_eval_harness():
    """`marlowe eval where` finds the checkout and an interpreter; `marlowe eval tools` lists tools."""
    out = subprocess.run([MARLOWE, "eval", "where"], capture_output=True, text=True, timeout=120)
    assert out.returncode == 0 and "eval dir:" in out.stdout and "run.py" not in out.stderr
    tools = subprocess.run([MARLOWE, "eval", "tools"], capture_output=True, text=True, timeout=120).stdout.split()
    assert "bash" in tools and "read" in tools and "edit" in tools
    env = {**os.environ, "MARLOWE_TOOL_GROUPS": "shell"}
    shell = subprocess.run([MARLOWE, "eval", "tools"], capture_output=True, text=True, timeout=120, env=env).stdout.split()
    assert "bash" in shell and not {"read", "edit", "write", "grep", "glob"} & set(shell)
    forwarded = subprocess.run([MARLOWE, "eval", "arms"], capture_output=True, text=True, timeout=120)
    assert forwarded.returncode == 0 and "marlowe-shell" in forwarded.stdout, forwarded.stderr


def test_a_micro_task_is_solved_end_to_end_through_the_real_binary(tmp_path):
    script = write_script(tmp_path, CLAMP_RULES)
    out = tmp_path / "out"
    rc = cli.main(
        ["run", "--suite", "micro", "--arm", "marlowe-full", "--ids", "micro-clamp", "--model", "ollama/scripted", "--marlowe", MARLOWE, "--out", str(out), "--env", f"MARLOWE_SCRIPTED_PROVIDER={script}", "--summary-json", str(tmp_path / "s.json")]
    )
    assert rc == 0
    rec = load_records(out / "records.jsonl")[0]
    assert rec.passed, (rec.test_output_tail, rec.agent.get("error"), rec.agent.get("stderr_tail"))
    assert rec.agent["tool_calls"] == 2 and rec.agent["tool_errors"] == 0
    assert rec.agent["steps"] and rec.agent["exit_code"] == 0
    assert json.loads((tmp_path / "s.json").read_text(encoding="utf-8"))["passed"] == 1
    assert (out / "runs" / "marlowe-full" / "micro-clamp-s0" / "transcript-a1.jsonl").is_file()


def test_the_shell_only_arm_really_has_no_file_tools(tmp_path):
    script = write_script(tmp_path, CLAMP_RULES)
    out = tmp_path / "out"
    cfg = ArmConfig(out_dir=out, marlowe=MARLOWE, model="ollama/scripted", env_extra={"MARLOWE_SCRIPTED_PROVIDER": script})
    a = make_adapter("marlowe-shell", cfg)
    assert a.preflight() == [], "marlowe eval tools under MARLOWE_TOOL_GROUPS=shell lists only shell tools"
    rc = cli.main(["run", "--suite", "micro", "--arm", "marlowe-shell", "--ids", "micro-clamp", "--model", "ollama/scripted", "--marlowe", MARLOWE, "--out", str(out), "--env", f"MARLOWE_SCRIPTED_PROVIDER={script}"])
    assert rc == 0
    rec = load_records(out / "records.jsonl")[0]
    assert not rec.passed, "the scripted model asks for read and edit; the shell arm has neither"
    assert rec.agent["tool_errors"] >= 1 or rec.agent["violations"], rec.agent


def test_an_ask_user_scenario_runs_through_the_stream_json_control_protocol(tmp_path):
    ask = {"questions": [{"question": "Which backend?", "header": "Backend", "options": [{"label": "postgres", "description": "p"}, {"label": "mysql", "description": "m"}], "multi_select": False}]}
    rules = [
        {"name": "ask", "tool_results": 0, "calls": [{"name": "ask_user", "args": ask}]},
        {"name": "mysql", "tool_results": 1, "last": ["mysql"], "calls": [{"name": "write", "args": {"path": "choice.txt", "content": "backend=mysql\n"}}]},
        {"name": "other", "tool_results": 1, "calls": [{"name": "write", "args": {"path": "choice.txt", "content": "backend=other\n"}}]},
        {"name": "done", "tool_results": 2, "reply": "done"},
    ]
    script = write_script(tmp_path, rules)
    scn = {
        "id": "smoke-ask",
        "steps": [{"say": "pick a backend", "answers": {"backend": "mysql"}}],
        "checks": [{"file": "choice.txt", "contains": ["backend=mysql"]}, {"questions": {"min": 1, "max": 1}}],
    }
    cfg = ArmConfig(out_dir=tmp_path / "out", marlowe=MARLOWE, model="ollama/scripted", env_extra={"MARLOWE_SCRIPTED_PROVIDER": script})
    a = make_adapter("marlowe-full", cfg)
    warnings = seed_marlowe_home(a, thinking=False)
    rec = iact.run_scenario(scn, a, tmp_path / "out", run_id="smoke", idle_timeout=120)
    assert rec.passed, (rec.test_output_tail, rec.agent, warnings)
    assert rec.agent["questions_asked"] == 1
