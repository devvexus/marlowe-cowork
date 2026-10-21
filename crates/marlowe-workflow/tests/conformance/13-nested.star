META = {"name": "nested", "description": "workflow() (the old name of team()) runs a saved team task one level deep"}

def main(args):
    child = workflow("child", {"topic": args["topic"]})
    return {"child": child}
