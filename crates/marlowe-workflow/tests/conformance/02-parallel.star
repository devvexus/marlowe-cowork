META = {"name": "parallel", "description": "Three agents at once", "phases": ["fan-out"]}

def main(args):
    phase("fan-out")
    return parallel([
        lambda: agent("p0"),
        lambda: agent("p1"),
        lambda: agent("p2"),
    ])
