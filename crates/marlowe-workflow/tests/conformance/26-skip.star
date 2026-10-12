META = {"name": "skip", "description": "A skipped agent is None"}

def main(args):
    return parallel([lambda: agent("keep"), lambda: agent("skip me"), lambda: agent("keep too")])
