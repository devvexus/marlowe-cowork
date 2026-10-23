# Built-in /simplify (design G12): reuse, simplification, efficiency and altitude lenses over the
# changed code, then one agent applies the fixes and keeps the tests green. Quality only: it does
# not hunt for bugs (that is /code-review).
# args: {"target": "" | "diff" | "<base ref>", "apply": bool (default true)}

META = {
    "name": "simplify",
    "description": "Review the changed code for reuse, simplification, efficiency and altitude cleanups, then apply them",
    "when_to_use": "The user asks to simplify or clean up recent changes (/simplify); not for finding bugs.",
    "phases": ["find", "apply"],
    "summary": "clean up changed code",
    "args": "target (diff or base ref); apply (bool)",
}

LENSES = {
    "altitude": "code at the wrong level: policy in a helper, details leaking into a high-level function, a layer that adds nothing",
    "efficiency": "work done twice, allocations or copies in loops, a slower way of doing what a direct call does",
    "reuse": "code that duplicates a helper, type or function the codebase already has; point at the existing one",
    "simplification": "logic that can be shorter and clearer with the same behaviour: dead branches, needless state, nested conditions",
}

FINDING = {
    "type": "object",
    "properties": {
        "lens": {"type": "string"},
        "severity": {"enum": ["info", "low", "medium", "high", "critical"]},
        "file": {"type": "string"},
        "line": {"type": "integer", "minimum": 1},
        "claim": {"type": "string"},
        "evidence": {"type": "string"},
        "fix": {"type": "string"},
    },
    "required": ["lens", "severity", "file", "claim", "evidence", "fix"],
}

FINDINGS = {
    "type": "object",
    "properties": {"findings": {"type": "array", "items": FINDING}},
    "required": ["findings"],
}

def source_of(target):
    t = target.strip()
    if t == "" or t == "diff":
        return "the pending changes (git{op:\"diff\"} and git{op:\"diff\", staged:true})"
    return "the changes on this branch against " + t + " (git{op:\"diff\", base:\"" + t + "\"})"

def find_prompt(lens, source):
    return ("Look at " + source + " through one lens only: " + lens + " (" + LENSES[lens] + "). " +
            "Quality only: do not report bugs. Each finding names the file and line, the claim, the evidence and the exact fix. " +
            "An empty list is a fine answer. Answer with structured_output: {\"findings\": [...]}, lens \"" + lens + "\".")

def main(args):
    source = source_of(str(args.get("target", "") or ""))
    phase("find")
    found = parallel([
        (lambda lens = lens: agent(find_prompt(lens, source), label = "find: " + lens, schema = FINDINGS))
        for lens in sorted(LENSES.keys())
    ])
    findings = []
    for r in found:
        if r != None:
            findings.extend(r["findings"])
    log(str(len(findings)) + " cleanup(s) found")
    applied = None
    if args.get("apply", True) and findings:
        phase("apply")
        listing = "\n".join([
            "- [" + f["lens"] + "] " + f["file"] + (":" + str(f["line"]) if f.get("line") else "") + ": " + f["claim"] + " Fix: " + f["fix"]
            for f in findings
        ])
        applied = agent(
            "Apply these cleanups to the working tree, one at a time, keeping behaviour identical:\n" + listing +
            "\nRun the project's tests after the edits; if a cleanup breaks a test, undo that one and say so. " +
            "End with what you applied, what you skipped and why, and the test result.",
            label = "apply",
        )
    return {"findings": findings, "applied": applied}
