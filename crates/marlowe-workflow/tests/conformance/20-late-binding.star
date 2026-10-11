META = {"name": "late", "description": "Lambdas in a loop that read the loop variable are refused"}

def main(args):
    return parallel([lambda: agent("file " + f) for f in args["files"]])
