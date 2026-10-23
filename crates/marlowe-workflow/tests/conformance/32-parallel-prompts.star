META = {"name": "parallel-prompts", "description": "Prompt strings stand for lambda: agent(prompt)", "phases": ["fan-out"]}

def main(args):
    phase("fan-out")
    return parallel([
        "p0",
        lambda: agent("p1"),
        "p2",
    ])
