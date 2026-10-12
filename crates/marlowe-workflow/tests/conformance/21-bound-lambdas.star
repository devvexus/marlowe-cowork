META = {"name": "bound", "description": "Binding the loop variable as a default is the fix"}

def main(args):
    thunks = []
    for f in args["files"]:
        thunks.append(lambda f=f: agent("file " + f))
    return parallel(thunks)
