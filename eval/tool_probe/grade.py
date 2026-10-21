"""Grades one probe run (a JSON line from `marlowe headless --probe-first-call`) against its case.

Strict: what the model said, as the provider parsed it, is a call the tool takes as it stands: the
shown tool, every key in the schema, types exact (an untyped property is a string), enums exact,
no rewrite by the forgiving layer, the case's expected keys and values present.
Lenient: the same after the forgiving layer (a legacy name or a spelling it fixes is a pass).

A workflow passes when it validates and fans out: a script that `fans_out`, or the short form
(`prompt`, `items` | `items_from`) whose validation reports "over N items" with N of 2 or more.

A case's `first_ok` lists calls that may come first (read before edit, task_list before
task_stop); the next call then has to be an accepted one. Only the accepted call is held to the
strict rules: the look-up before it is not.
"""

import json
import re


def text_of(v):
    """The text a value check sees: a string as it is, anything else as JSON (true, 460, [..])."""
    return v if isinstance(v, str) else json.dumps(v, ensure_ascii=False)


def _type_ok(prop, v):
    """Exact types. A property with no `type` and no `enum` is a string."""
    if "enum" in prop:
        return True
    t = prop.get("type", "string")
    ts = t if isinstance(t, list) else [t]

    def one(t):
        return {
            "string": isinstance(v, str),
            "integer": isinstance(v, int) and not isinstance(v, bool),
            "number": isinstance(v, (int, float)) and not isinstance(v, bool),
            "boolean": isinstance(v, bool),
            "array": isinstance(v, list),
            "object": isinstance(v, dict),
            "null": v is None,
        }.get(t, True)

    return any(one(t) for t in ts)


def schema_problems(schema, args, path=""):
    """Failure classes of `args` against the shown schema: unknown_key, missing_key, type, enum."""
    if not isinstance(args, dict):
        return ["args(not an object)"]
    props = (schema or {}).get("properties") or {}
    out = []
    for k, v in args.items():
        if props and k not in props:
            out.append(f"unknown_key({path}{k})")
            continue
        prop = props.get(k)
        if prop is None:
            continue
        if "enum" in prop and v not in prop["enum"]:
            out.append(f"enum({path}{k})")
        elif not _type_ok(prop, v):
            out.append(f"type({path}{k})")
        elif prop.get("type") == "array" and isinstance(v, list) and isinstance(prop.get("items"), dict):
            item = prop["items"]
            for i, e in enumerate(v):
                if item.get("type") == "object" and isinstance(e, dict):
                    out += schema_problems(item, e, f"{path}{k}[{i}].")
                elif not _type_ok(item, e) and item.get("type"):
                    out.append(f"type({path}{k}[{i}])")
    # A misspelt key usually leaves its required one missing: the misspelling is the news.
    out += [f"missing_key({path}{k})" for k in (schema or {}).get("required") or [] if k not in args]
    return out


def value_problems(entry, args):
    """Expected keys and values of an accept entry; empty when they hold."""
    if not isinstance(args, dict):
        return ["args(not an object)"]
    out = [f"missing_expected({k})" for k in entry.get("required_keys", []) if k not in args or args[k] in (None, "", [])]
    for k, rx in (entry.get("value_checks") or {}).items():
        if k in args and not re.search(rx, text_of(args[k])):
            out.append(f"value({k})")
    return out


def fans_out(script):
    """True when a workflow script can run more than one agent: two `agent(` calls, or one inside a loop,
    a comprehension or `pipeline(`. A static reading of the text, not a run."""
    if not isinstance(script, str):
        return False
    n = len(re.findall(r"\bagent\(", script))
    return n >= 2 or (n == 1 and bool(re.search(r"\bfor\b|\bpipeline\(", script)))


def short_form_items(call):
    """N of a short-form `workflow` call whose validation said "workflow fanout over N items", else 0."""
    text = (call.get("validated") or {}).get("text") or ""
    m = re.search(r"workflow fanout over (\d+) items", text)
    return int(m.group(1)) if m else 0


def fans_out_call(call, args):
    """A workflow call can run more than one agent: its script `fans_out`, or its short form is over 2+ items
    (no items at all is the three-angle default, which validation reports as "over 3 items")."""
    if isinstance(args, dict) and args.get("script"):
        return fans_out(args["script"])
    return short_form_items(call) >= 2


def _view(call, mode):
    """(name, args) of a call as the provider parsed it (raw) or as the layer left it (layer)."""
    if mode == "raw":
        r = call.get("raw") or {}
        return r.get("name"), r.get("args")
    if call.get("refused") or not call.get("layer"):
        p = call.get("prepared") or {}
        return p.get("name"), None
    return call["layer"]["name"], call["layer"].get("args")


def _entry(entries, name):
    return next((e for e in entries if e["name"] == name), None)


def _accepted_call(case, call, mode, surfaces, run):
    """Failure class of a call that is the case's answer, or None when it passes in `mode`."""
    name, args = _view(call, mode)
    entry = _entry(case["accept"], name)
    if entry is None:
        return f"wrong_tool({name})"
    if mode == "layer" and (call.get("refused") or args is None):
        first = (call.get("result") or "").strip().splitlines()[0:1]
        return "invalid_args(" + (first[0][:80] if first else "refused") + ")"
    if mode == "raw":
        if call.get("refused"):
            return "invalid_args(refused by the layer)"
        shown = surfaces.get(run.get("surface"), {})
        spec = next((s for s in shown.get("specs", []) if s["name"] == name), None)
        if spec is not None:
            problems = schema_problems(spec.get("input_schema"), args)
            if problems:
                return problems[0]
        layer = call.get("layer") or {}
        if layer.get("name") != name or layer.get("args") != args:
            return "rewritten(" + ",".join(sorted(set((args or {}).keys()) ^ set((layer.get("args") or {}).keys())) or ["value"]) + ")"
    problems = value_problems(entry, args)
    if problems:
        return problems[0]
    if entry.get("validated") and not (call.get("validated") or {}).get("ok"):
        return "bad_script"
    if entry.get("fan_out") and not fans_out_call(call, args):
        return "no_fan_out"
    return None


def grade_mode(case, run, mode, surfaces):
    """(passed, failure class) for one mode over the run's calls."""
    calls = run.get("calls") or []
    if not calls:
        return False, "no_call" if not run.get("error") else "error"
    name0, args0 = _view(calls[0], mode)
    if _entry(case["accept"], name0) is not None:
        why = _accepted_call(case, calls[0], mode, surfaces, run)
        return why is None, why
    # Look-up calls the case allows may come first (any number); the call after them is the answer.
    for i, call in enumerate(calls):
        name, args = _view(call, mode)
        if _entry(case["accept"], name) is not None:
            why = _accepted_call(case, call, mode, surfaces, run)
            return why is None, why
        ok = _entry(case.get("first_ok", []), name)
        if ok is None or value_problems(ok, args):
            return False, f"wrong_tool({name})"
    return False, "no_second_call"


# The edit tools' own read-first gate fires because a canned `read` is not a real read: not an argument error.
STATE_GATES = ("you have not read it in this session",)


def _ungate(run):
    calls = []
    for c in run.get("calls") or []:
        if c.get("refused") and any(t in (c.get("result") or "") for t in STATE_GATES):
            c = dict(c, refused=False, layer=c.get("layer") or c.get("prepared"))
        calls.append(c)
    return dict(run, calls=calls)


def grade(case, run, surfaces):
    run = _ungate(run)
    strict, strict_why = grade_mode(case, run, "raw", surfaces)
    lenient, lenient_why = grade_mode(case, run, "layer", surfaces)
    first = ((run.get("calls") or [{}])[0].get("raw") or {}).get("name")
    return {
        "strict": strict,
        "lenient": lenient or strict,
        "strict_why": strict_why,
        "lenient_why": None if (lenient or strict) else lenient_why,
        "first_call": first,
        "rewrites": run.get("rewrites") or [],
    }
