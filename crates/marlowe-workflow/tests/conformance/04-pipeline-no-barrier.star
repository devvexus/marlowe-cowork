META = {"name": "pipeline", "description": "Each item moves through the stages on its own"}

def main(args):
    return pipeline(
        ["A", "B"],
        lambda prev, item, i: agent("s1 " + item),
        lambda prev, item, i: agent("s2 " + item),
        lambda prev, item, i: agent("s3 " + item),
    )
