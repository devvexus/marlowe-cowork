META = {"name": "nested", "description": "workflow() runs a saved workflow one level deep"}

def main(args):
    child = workflow("child", {"topic": args["topic"]})
    return {"child": child}
