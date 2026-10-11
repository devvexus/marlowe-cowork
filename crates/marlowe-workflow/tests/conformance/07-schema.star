META = {"name": "schema", "description": "Structured output comes back as a dict"}

FINDING = {
    "type": "object",
    "properties": {
        "file": {"type": "string"},
        "line": {"type": "integer", "minimum": 1},
        "severity": {"enum": ["low", "high"]},
    },
    "required": ["file", "line", "severity"],
}

def main(args):
    f = agent("find one bug", schema = FINDING)
    return {"where": f["file"] + ":" + str(f["line"]), "severity": f["severity"]}
