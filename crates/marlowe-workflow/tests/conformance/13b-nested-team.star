META = {"name": "nested-team", "description": "team() runs a saved team task one level deep"}

def main(args):
    child = team("child", {"topic": args["topic"]})
    return {"child": child}
