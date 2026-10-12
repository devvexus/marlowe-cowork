META = {"name": "top-level-runaway", "description": "The top level is bounded too: the check ends"}

TABLE = [0 for i in range(100000) for j in range(100000)]

def main(args):
    return len(TABLE)
