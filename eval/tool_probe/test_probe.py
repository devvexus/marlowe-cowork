"""Offline checks for the probe: the cases against the shipped specs, the grader, the report.
Nothing here starts a model or the binary.  python -I -m pytest eval/tool_probe"""

import json
import re
import subprocess
import sys
from pathlib import Path

import grade as g
import probe

HERE = Path(__file__).parent
SPECS = {s["name"]: s for s in json.loads((HERE.parent.parent / "crates/marlowe-agent/src/compact_specs.json").read_text(encoding="utf-8"))}
CASES = probe.load_cases(HERE / "cases.jsonl")
# Not on a production host: bench-only and the workflow agents' own finaliser.
NOT_PROBED = {"edit_lines", "structured_output"}


def case(**kw):
    base = {"id": "t", "accept": [{"name": "web_fetch", "required_keys": ["url"], "value_checks": {"url": "^https://"}}]}
    return {**base, **kw}


def call(raw, layer="same", refused=False, result=None, validated=None):
    layer = {"name": raw[0], "args": raw[1]} if layer == "same" else layer
    c = {"raw": {"name": raw[0], "args": raw[1]}, "prepared": {"name": (layer or {}).get("name", raw[0]), "args": raw[1]}, "layer": layer, "refused": refused, "result": result}
    if validated is not None:
        c["validated"] = validated
    return c


def run(*calls, rewrites=()):
    return {"case": "t", "run": 0, "surface": "conductor", "calls": list(calls), "rewrites": list(rewrites)}


SURFACES = {"conductor": {"specs": [SPECS["web_fetch"], SPECS["read"], SPECS["git"], SPECS["task_list"], SPECS["task_stop"]]}}


# ---------------------------------------------------------------- the cases

def test_every_case_names_real_tools_and_keys():
    ids = [c["id"] for c in CASES]
    assert len(ids) == len(set(ids))
    for c in CASES:
        for entry in c["accept"] + c.get("first_ok", []):
            for rx in (entry.get("value_checks") or {}).values():
                re.compile(rx)
            if entry["name"].startswith("mcp__") or entry["name"] == "tool_search":
                continue
            assert entry["name"] in SPECS, f"{c['id']}: no tool {entry['name']}"
            props = SPECS[entry["name"]]["input_schema"].get("properties", {})
            for k in entry.get("required_keys", []) + list((entry.get("value_checks") or {})):
                assert k in props, f"{c['id']}: {entry['name']} has no argument {k}"


def test_every_probed_tool_has_a_case_and_the_scenarios_are_there():
    covered = {c["tool"] for c in CASES}
    missing = set(SPECS) - NOT_PROBED - covered
    assert not missing, missing
    scen = {c["scenario"]: c for c in CASES if c.get("scenario")}
    assert {1, 2, 3, 4, 5} <= set(scen)
    assert scen[1]["request"].startswith("fetch https://") and scen[1]["accept"][0]["name"] == "web_fetch"
    assert "arxiv.org/abs/" in scen[2]["request"] and scen[2]["accept"][0]["name"] == "web_fetch"
    assert scen[3]["request"] == "cancel the workflow" and scen[3]["max_calls"] == 2 and scen[3]["first_ok"][0]["name"] == "task_list"
    assert "wf_7" in scen[3]["canned"]["task_list"] and scen[3]["accept"][0]["value_checks"]["id"] == "^wf_7$"
    assert scen[4]["accept"][0]["validated"] is True
    assert any(c["mcp"] for c in CASES if c.get("scenario") == 5)


# ---------------------------------------------------------------- the schema check

def test_schema_problems_are_exact():
    sch = SPECS["web_fetch"]["input_schema"]
    assert g.schema_problems(sch, {"url": "https://x"}) == []
    assert g.schema_problems(sch, {}) == ["missing_key(url)"]
    assert g.schema_problems(sch, {"url": "https://x", "query": "y"}) == ["unknown_key(query)"]
    assert g.schema_problems(sch, {"url": 5}) == ["type(url)"], "an untyped property is a string"
    assert g.schema_problems(sch, {"url": "u", "offset": "3"}) == ["type(offset)"], "no string for an integer"
    assert g.schema_problems(sch, {"url": "u", "offset": True}) == ["type(offset)"], "a boolean is not an integer"
    assert g.schema_problems(sch, {"url": "u", "part": "Text"}) == ["enum(part)"]
    assert g.schema_problems(SPECS["git"]["input_schema"], {"op": "pull"}) == ["enum(op)"]
    nested = SPECS["multi_edit"]["input_schema"]
    assert g.schema_problems(nested, {"edits": [{"old_string": "a"}]}) == ["missing_key(edits[0].new_string)"]


# ---------------------------------------------------------------- grading

def test_a_clean_call_passes_both_ways():
    r = g.grade(case(), run(call(("web_fetch", {"url": "https://example.com/docs"}))), SURFACES)
    assert (r["strict"], r["lenient"]) == (True, True)


def test_a_legacy_name_passes_only_after_the_layer():
    c = call(("paper", {"url": "https://arxiv.org/abs/1"}), layer={"name": "web_fetch", "args": {"url": "https://arxiv.org/abs/1"}})
    r = g.grade(case(), run(c), SURFACES)
    assert (r["strict"], r["lenient"], r["strict_why"]) == (False, True, "wrong_tool(paper)")


def test_a_renamed_key_is_a_lenient_pass_with_the_reason():
    c = call(("web_fetch", {"link": "https://x.io"}), layer={"name": "web_fetch", "args": {"url": "https://x.io"}})
    r = g.grade(case(), run(c, rewrites=["web_fetch: link -> url"]), SURFACES)
    assert (r["strict"], r["lenient"]) == (False, True)
    assert r["strict_why"] == "unknown_key(link)" and r["rewrites"] == ["web_fetch: link -> url"]


def test_a_rewrite_of_a_valid_call_is_not_strict():
    # The model's call is schema-valid but the layer still changed it (a value coerced).
    c = call(("web_fetch", {"url": "https://x.io", "raw": True}), layer={"name": "web_fetch", "args": {"url": "https://x.io"}})
    r = g.grade(case(), run(c), SURFACES)
    assert (r["strict"], r["lenient"], r["strict_why"]) == (False, True, "rewritten(raw)")


def test_a_refused_call_fails_both_with_the_layer_text():
    c = call(("web_fetch", {}), layer=None, refused=True, result="web_fetch was not run: missing `url`.\nweb_fetch(url, ...)")
    r = g.grade(case(), run(c), SURFACES)
    assert (r["strict"], r["lenient"]) == (False, False)
    assert r["strict_why"] == "invalid_args(refused by the layer)" and r["lenient_why"].startswith("invalid_args(web_fetch was not run")


def test_the_wrong_tool_and_no_call_are_named():
    r = g.grade(case(), run(call(("web_search", {"query": "x"}))), SURFACES)
    assert r["strict_why"] == "wrong_tool(web_search)" and r["first_call"] == "web_search"
    assert g.grade(case(), run(), SURFACES)["strict_why"] == "no_call"


def test_expected_values_are_checked():
    r = g.grade(case(), run(call(("web_fetch", {"url": "ftp://x"}))), SURFACES)
    assert (r["strict"], r["lenient"], r["strict_why"]) == (False, False, "value(url)")


def test_a_look_up_first_then_the_answer_passes_over_two_calls():
    c = case(id="cancel", first_ok=[{"name": "task_list"}],
             accept=[{"name": "task_stop", "required_keys": ["id"], "value_checks": {"id": "^wf_7$"}}])
    good = run(call(("task_list", {})), call(("task_stop", {"id": "wf_7"})))
    assert g.grade(c, good, SURFACES)["strict"] is True
    wrong_id = run(call(("task_list", {})), call(("task_stop", {"id": "wf_1"})))
    assert g.grade(c, wrong_id, SURFACES)["strict_why"] == "value(id)"
    straight = run(call(("task_stop", {"id": "wf_7"})))
    assert g.grade(c, straight, SURFACES)["strict"] is True, "going straight to the answer is fine"
    only_lookup = run(call(("task_list", {})))
    assert g.grade(c, only_lookup, SURFACES)["strict_why"] == "no_second_call"
    guessed = run(call(("web_fetch", {"url": "https://x"})), call(("task_stop", {"id": "wf_7"})))
    assert g.grade(c, guessed, SURFACES)["strict_why"] == "wrong_tool(web_fetch)"


def test_a_workflow_script_must_validate():
    c = case(accept=[{"name": "team", "required_keys": ["script"], "validated": True}])
    sur = {"conductor": {"specs": [SPECS["team"]]}}
    ok = run(call(("team", {"script": "x"}), validated={"ok": True, "text": "valid"}))
    bad = run(call(("team", {"script": "x"}), validated={"ok": False, "text": "line 1"}))
    assert g.grade(c, ok, sur)["strict"] is True
    assert g.grade(c, bad, sur)["strict_why"] == "bad_script"


def test_a_workflow_must_fan_out_and_look_ups_may_repeat():
    c = case(first_ok=[{"name": "read"}, {"name": "glob"}],
             accept=[{"name": "team", "required_keys": ["script"], "validated": True, "fan_out": True}])
    sur = {"conductor": {"specs": [SPECS["team"], SPECS["read"], SPECS["glob"]]}}
    one = run(call(("team", {"script": "def main(args):\n    return agent('x')"}), validated={"ok": True, "text": "valid"}))
    assert g.grade(c, one, sur)["strict_why"] == "no_fan_out"
    many = "def main(args):\n    return parallel([lambda: agent('a'), lambda: agent('b')])"
    ok = run(call(("glob", {"pattern": "*"})), call(("read", {"path": "a"})),
             call(("team", {"script": many}), validated={"ok": True, "text": "valid"}))
    assert g.grade(c, ok, sur)["strict"] is True
    loop = "def main(args):\n    return [agent(c) for c in args['crates']]"
    assert g.fans_out(loop) and not g.fans_out("def main(args):\n    return agent('x')")
    dispatched = run(call(("dispatch", {"role": "researcher", "prompt": "x"})))
    assert g.grade(c, dispatched, sur)["strict_why"] == "wrong_tool(dispatch)"


def test_a_workflow_passes_when_it_validates_and_its_script_fans_out():
    c = case(first_ok=[{"name": "read"}],
             accept=[{"name": "team", "validated": True, "fan_out": True}])
    sur = {"conductor": {"specs": [SPECS["team"], SPECS["read"]]}}
    ok = run(call(("team", {"script": FAN}), validated={"ok": True, "text": "x.star is valid. META: {}"}))
    assert g.grade(c, ok, sur)["strict"] is True
    # One agent is not a fan-out; an invalid call is not either.
    one = run(call(("team", {"script": "def main(args):\n    return agent('x')"}), validated={"ok": True, "text": "valid"}))
    assert g.grade(c, one, sur)["strict_why"] == "no_fan_out"
    refused = run(call(("team", {"script": FAN}), validated={"ok": False, "text": "bad"}))
    assert g.grade(c, refused, sur)["strict_why"] == "bad_script"
    # A saved name is not a script: it does not count as a fan-out.
    named = run(call(("team", {"name": "code-review"}), validated={"ok": True, "text": "code-review.star is valid. META: {}"}))
    assert g.grade(c, named, sur)["strict_why"] == "no_fan_out"


def test_every_workflow_family_case_wants_a_validated_fan_out():
    family = [c for c in CASES if c["tool"] == "team"]
    assert len(family) >= 5
    for c in family:
        entry = c["accept"][0]
        assert entry["name"] == "team" and entry.get("validated") is True and entry.get("fan_out") is True, c["id"]
    review = next(c for c in family if c["id"] == "workflow-review-commits")
    assert len(review["git"]) == 3


def stepped(c, step):
    return {**c, "step": step}


WF = case(first_ok=[{"name": "read"}], accept=[{"name": "team", "validated": True, "fan_out": True}])
WF_SURFACES = {"conductor": {"specs": [SPECS["team"], SPECS["read"]]}}
OVER3 = {"ok": True, "text": "x.star is valid. META: {}"}
FAN = "def main(args):\n    return [agent(c) for c in args['crates']]"
SHORT = {"script": FAN}


def test_first_try_is_a_strict_pass_from_the_first_step_and_a_later_pass_is_rescued():
    first = run(stepped(call(("team", SHORT), validated=OVER3), 0))
    r = g.grade(WF, first, WF_SURFACES)
    assert (r["strict"], r["first_try"], r["rescued"]) == (True, True, False)
    # A look-up first is still a strict pass, but not a first try.
    looked = run(stepped(call(("read", {"path": "a"})), 0), stepped(call(("team", SHORT), validated=OVER3), 1))
    r = g.grade(WF, looked, WF_SURFACES)
    assert (r["strict"], r["first_try"], r["rescued"]) == (True, False, True)
    # The model said nothing useful in step 0 and a harness nudge got the call in step 1.
    nudged = run(stepped(call(("team", SHORT), validated=OVER3), 1))
    r = g.grade(WF, nudged, WF_SURFACES)
    assert (r["strict"], r["first_try"], r["rescued"]) == (True, False, True)
    # A layer fix is lenient only: rescued, not strict.
    fixed = call(("team", {"script": FAN, "role": "x"}), layer={"name": "team", "args": {"script": FAN}}, validated=OVER3)
    r = g.grade(WF, run(stepped(fixed, 0)), WF_SURFACES)
    assert (r["strict"], r["lenient"], r["first_try"], r["rescued"]) == (False, True, False, True)
    # A failure is neither.
    r = g.grade(WF, run(stepped(call(("dispatch", {"role": "researcher", "prompt": "x"})), 0)), WF_SURFACES)
    assert (r["strict"], r["lenient"], r["first_try"], r["rescued"]) == (False, False, False, False)


def test_accept_none_passes_when_nothing_is_launched():
    neg = case(accept=[], accept_none=True)
    assert g.grade(neg, run(), WF_SURFACES)["strict"] is True, "prose is fine"
    looked = g.grade(neg, run(call(("read", {"path": "docs/a.md"}))), WF_SURFACES)
    assert (looked["strict"], looked["lenient"], looked["first_try"], looked["rescued"]) == (True, True, True, False)
    for launched in (("team", SHORT), ("dispatch", {"role": "researcher", "prompt": "x"}), ("agent", {"prompt": "x"})):
        r = g.grade(neg, run(call(launched)), WF_SURFACES)
        assert (r["strict"], r["lenient"]) == (False, False) and r["strict_why"] == f"launched({launched[0]})", launched
    fg = g.grade(neg, run(call(("agent", {"prompt": "x", "background": False}))), WF_SURFACES)
    assert fg["strict"] is True, "a foreground agent is not a launch"


def test_the_workflow_cases_and_negatives_are_in_the_file():
    ids = {c["id"] for c in CASES}
    for want in ("workflow-use-a", "workflow-fanout-words", "workflow-run-as", "workflow-kick-off", "workflow-session-repeat", "workflow-question", "workflow-why-failed"):
        assert want in ids, want
    negatives = [c for c in CASES if c.get("accept_none")]
    assert {c["id"] for c in negatives} == {"workflow-question", "workflow-why-failed"}
    assert all(c["accept"] == [] for c in negatives)
    repeat = next(c for c in CASES if c["id"] == "workflow-session-repeat")
    assert "agent_a990e8f6" in repeat["reminder"] and repeat["tasks"][0]["status"] == "running"


# ---------------------------------------------------------------- the report

def write_run(path, calibrated=True):
    surface = {"kind": "surface", "key": "conductor", "specs": SURFACES["conductor"]["specs"], "tools_chars": 3000, "tools_estimate": 900}
    lines = [surface]
    for i, temp in enumerate([0.0, None, None]):
        c = call(("web_fetch", {"url": "https://example.com/docs"} if i < 2 else {"link": "https://example.com/docs"}),
                 layer={"name": "web_fetch", "args": {"url": "https://example.com/docs"}})
        r = {"kind": "run", "case": "web_fetch-url", "run": i, "surface": "conductor", "calls": [c], "rewrites": [] if i < 2 else ["web_fetch: link -> url"]}
        if calibrated and i == 0:
            r["calibration"] = {"without_tools": 4000, "with_tools": 5200}
        lines.append(r)
    path.write_text("\n".join(json.dumps(x) for x in lines), encoding="utf-8")


def test_the_report_counts_and_calibrates(tmp_path):
    cases = [c for c in CASES if c["id"] == "web_fetch-url"]
    f = tmp_path / "m.B.jsonl"
    write_run(f)
    data = probe.report(cases, {("marlowe-dawn:9b", "B"): f}, tmp_path)
    arm = data["arms"]["marlowe-dawn:9b|B"]
    assert (arm["runs"], arm["strict"], arm["lenient"], arm["scenarios_strict"]) == (3, 2, 3, "2/3")
    assert arm["meets"] == {"strict": False, "lenient": True} and not arm["scenarios_all_pass"]
    row = data["rows"]["web_fetch-url"]["marlowe-dawn:9b|B"]
    assert (row["strict"], row["lenient"], row["rewrite"]) == ("2/3", "3/3", "web_fetch: link -> url")
    cal = data["calibration"]["marlowe-dawn:9b|B"][0]
    assert cal["measured_tokens"] == 1200 and cal["estimate_chars_3"] == 1000 and cal["ratio_to_chars_3"] == 1.2
    md = (tmp_path / "report.md").read_text(encoding="utf-8")
    assert "web_fetch-url" in md and "2/3 / 3/3" in md and "| 1200 |" in md
    assert (tmp_path / "report.json").is_file()


def test_nothing_starts_without_go():
    out = subprocess.run([sys.executable, "-I", str(HERE / "probe.py")], capture_output=True, text=True, timeout=60)
    assert out.returncode == 0 and "not started" in out.stdout, out.stdout + out.stderr


def test_a_paid_model_is_refused():
    out = subprocess.run([sys.executable, "-I", str(HERE / "probe.py"), "--go", "--models", "openrouter/some-model"], capture_output=True, text=True, timeout=60)
    assert out.returncode != 0 and "local Ollama" in out.stderr
