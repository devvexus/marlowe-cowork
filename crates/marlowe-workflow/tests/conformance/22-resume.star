META = {"name": "resume", "description": "Four agents in sequence, for resume"}

def main(args):
    out = []
    for p in args["prompts"]:
        out.append(agent(p))
    return out
