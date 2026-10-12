META = {"name": "late-body", "description": "A lambda in a loop reading a variable the loop body sets is refused"}

def main(args):
    thunks = []
    for f in args["files"]:
        p = "Review " + f
        thunks.append(lambda: agent(p))
    return parallel(thunks)
