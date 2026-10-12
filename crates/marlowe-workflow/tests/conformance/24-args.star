META = {"name": "args", "description": "args arrive verbatim, as a global and as main's parameter"}

def main(a):
    return {"same": a == args, "args": a}
