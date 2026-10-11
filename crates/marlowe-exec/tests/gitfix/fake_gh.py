"""A stand-in for the GitHub CLI, for tests only (gh is not signed in on the build machine).

It records every call to calls.jsonl next to it and answers from responses.json. A response key is
a list of words; it matches a call when every word is one of the call's arguments (a word starting
with ~ matches as a substring of any argument). The best match is the key with the most words.
"""
import json
import os
import pathlib
import sys

here = pathlib.Path(__file__).resolve().parent
argv = sys.argv[1:]

record = {
    "argv": argv,
    "cwd": os.getcwd(),
    "env": {k: (k in os.environ) for k in ["GH_TOKEN", "GITHUB_TOKEN", "OPENROUTER_API_KEY", "ANTHROPIC_API_KEY", "HF_TOKEN"]},
    "prompt_disabled": os.environ.get("GH_PROMPT_DISABLED"),
}
if "--input" in argv:
    try:
        record["input"] = json.load(open(argv[argv.index("--input") + 1], encoding="utf-8"))
    except Exception as e:  # noqa: BLE001
        record["input_error"] = str(e)
for a in argv:
    if a.startswith("--body-file="):
        record["body_file"] = open(a.split("=", 1)[1], encoding="utf-8").read()
with open(here / "calls.jsonl", "a", encoding="utf-8") as f:
    f.write(json.dumps(record) + "\n")

if argv[:1] == ["--version"]:
    print("gh version 2.102.0 (fake)")
    sys.exit(0)

responses = json.load(open(here / "responses.json", encoding="utf-8"))


def matches(key):
    for word in key.split():
        if word.startswith("~"):
            if not any(word[1:] in a for a in argv):
                return False
        elif word not in argv:
            return False
    return True


best = None
for key in responses:
    if matches(key) and (best is None or len(key.split()) > len(best.split())):
        best = key
if best is None:
    sys.stderr.write("fake gh: no response for " + " ".join(argv) + "\n")
    sys.exit(99)
r = responses[best]
out = r.get("stdout", "")
sys.stdout.write(out if isinstance(out, str) else json.dumps(out))
sys.stderr.write(r.get("stderr", ""))
sys.exit(r.get("code", 0))
