# Built-in /code-review (design G11). Finders per lens, scaled by level; every finding is then
# checked by adversarial verifiers and kept on a majority; `fix` applies what survived.
# args: {"target": "" | "diff" | "<pr number>" | "<base ref>", "level": "low".."max",
#        "fix": bool, "comment": bool, "max_findings": int | "all"}

META = {
    "name": "code-review",
    "description": "Review the pending diff, a pull request or a branch for bugs: finders per lens, adversarial verification, optional fixes",
    "when_to_use": "The user asks to review changes, a PR or a branch for correctness bugs (/code-review).",
    "phases": ["find", "verify", "fix"],
    "args": "target (diff, PR number or base ref); level (low..max); fix; comment; max_findings",
}

LENSES = ["correctness", "security", "performance", "reuse", "tests"]

# level -> how many lenses look, and how many verifiers vote on each finding.
LEVELS = {
    "low": {"lenses": 1, "votes": 1},
    "medium": {"lenses": 2, "votes": 1},
    "high": {"lenses": 3, "votes": 3},
    "xhigh": {"lenses": 4, "votes": 3},
    "max": {"lenses": 5, "votes": 5},
}

LENS_FOCUS = {
    "correctness": "wrong results, unhandled cases, off-by-one, races, broken contracts with callers, error paths that lose data",
    "security": "injection, path traversal, secrets in code or logs, missing authorisation, unsafe deserialisation, trust of untrusted input",
    "performance": "work repeated in loops, quadratic paths on real sizes, blocking calls on hot paths, unbounded growth",
    "reuse": "logic that duplicates an existing helper, a second implementation of something the codebase already has",
    "tests": "changed behaviour with no test, tests that cannot fail, assertions that do not check the claim",
}

SEVERITY_RANK = {"critical": 0, "high": 1, "medium": 2, "low": 3, "info": 4}

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
    "required": ["lens", "severity", "file", "claim", "evidence"],
}

FINDINGS = {
    "type": "object",
    "properties": {"findings": {"type": "array", "items": FINDING}},
    "required": ["findings"],
}

VERDICT = {
    "type": "object",
    "properties": {"real": {"type": "boolean"}, "why": {"type": "string"}},
    "required": ["real", "why"],
}

def how_to_read(target):
    t = target.strip()
    if t == "" or t == "diff":
        return "the pending changes: git{op:\"diff\"} and git{op:\"diff\", staged:true} (or `git diff` and `git diff --staged` in the shell)"
    n = t.lstrip("#")
    if n.isdigit():
        return "pull request " + n + ": gh{op:\"pr_diff\", number:" + n + "} and gh{op:\"pr_view\", number:" + n + "}"
    return "the changes on this branch against " + t + ": git{op:\"diff\", base:\"" + t + "\"} (or `git diff " + t + "...HEAD`)"

def find_prompt(lens, source):
    return ("Review " + source + " for one kind of problem only: " + lens + " (" + LENS_FOCUS[lens] + ").\n" +
            "Read the diff, then the surrounding code wherever the diff alone does not settle it. " +
            "Report only problems you can point at in the changed code, each with the file, the line, " +
            "the claim in one sentence, the evidence (the code or behaviour that shows it) and the fix. " +
            "No style notes, no praise, no speculation. An empty list is a fine answer.\n" +
            "Answer with structured_output: {\"findings\": [...]}, every finding's lens \"" + lens + "\".")

def verify_prompt(f, k, votes):
    where = f["file"] + (":" + str(f["line"]) if f.get("line") else "")
    return ("You are verifier " + str(k + 1) + " of " + str(votes) + " for a code review finding. Be adversarial: " +
            "try to show it is wrong. Read the code at " + where + " and whatever it depends on.\n" +
            "Finding (" + f["lens"] + ", " + f["severity"] + "): " + f["claim"] + "\n" +
            "Evidence given: " + f.get("evidence", "") + "\n" +
            "It is real only if the problem actually happens with this code. " +
            "Answer with structured_output: {\"real\": true|false, \"why\": \"...\"}.")

def key_of(f):
    return f["file"] + ":" + str(f.get("line", 0)) + ":" + f["claim"][:48].lower()

def main(args):
    target = str(args.get("target", "") or "")
    level = args.get("level", "medium") or "medium"
    if level not in LEVELS:
        fail("level must be one of low, medium, high, xhigh, max; got " + str(level))
    plan = LEVELS[level]
    lenses = LENSES[:plan["lenses"]]
    votes = plan["votes"]
    source = how_to_read(target)
    log("level " + level + ": " + str(len(lenses)) + " lens(es), " + str(votes) + " vote(s) per finding")

    phase("find")
    found = parallel([
        (lambda lens = lens: agent(find_prompt(lens, source), label = "find: " + lens, schema = FINDINGS))
        for lens in lenses
    ])
    candidates = []
    seen = {}
    for result in found:
        if result == None:
            continue
        for f in result["findings"]:
            k = key_of(f)
            if k not in seen:
                seen[k] = True
                candidates.append(f)
    log(str(len(candidates)) + " candidate finding(s)")

    phase("verify")

    def check(prev, f, i):
        verdicts = parallel([
            (lambda k = k: agent(verify_prompt(f, k, votes), label = "verify " + str(i + 1) + "." + str(k + 1), schema = VERDICT))
            for k in range(votes)
        ])
        yes = len([v for v in verdicts if v != None and v["real"]])
        return {"finding": f, "yes": yes}

    checked = pipeline(candidates, check)
    kept = [c["finding"] for c in checked if c != None and c["yes"] * 2 > votes]
    kept = sorted(kept, key = lambda f: SEVERITY_RANK.get(f["severity"], 5))
    refuted = len(candidates) - len(kept)
    cap = args.get("max_findings", 10)
    if cap != "all" and cap != None:
        kept = kept[:int(cap)]
    log(str(len(kept)) + " finding(s) kept, " + str(refuted) + " refuted")

    fixed = None
    if args.get("fix", False) and kept:
        phase("fix")
        listing = "\n".join([
            "- " + f["file"] + (":" + str(f["line"]) if f.get("line") else "") + " [" + f["severity"] + "] " + f["claim"] +
            (" Fix: " + f["fix"] if f.get("fix") else "")
            for f in kept
        ])
        fixed = agent(
            "Fix these verified code review findings in the working tree, smallest change each, in the code's own style:\n" +
            listing + "\nThen run the project's tests and say which passed. End with the files you changed and why.",
            label = "fix",
        )

    return {
        "level": level,
        "target": target or "diff",
        "findings": kept,
        "refuted": refuted,
        "fix": fixed,
        "comment": bool(args.get("comment", False)),
    }
