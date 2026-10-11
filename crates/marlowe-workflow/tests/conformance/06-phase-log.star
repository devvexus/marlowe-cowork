META = {"name": "phase-log", "description": "Phases and log lines appear once each, in order", "phases": ["one", "two"]}

def main(args):
    phase("one")
    log("start", 1)
    a = agent("a")
    log("after a")
    phase("two")
    b = parallel([lambda: agent("b"), lambda: agent("c")])
    print("done", len(b))
    return a
