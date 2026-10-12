META = {"name": "pipeline-arity", "description": "Stages may take fewer parameters; a failing stage skips the rest of its item"}

def check(prev, item):
    if item == "bad":
        fail("bad item")
    return prev

def main(args):
    return pipeline(
        ["good", "bad"],
        lambda x: x.upper(),
        check,
        lambda prev, item, index: agent(f"{index}:{prev}"),
    )
