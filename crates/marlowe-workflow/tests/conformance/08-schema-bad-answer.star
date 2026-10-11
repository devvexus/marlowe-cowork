META = {"name": "schema-bad", "description": "An answer that breaks the schema is None"}

def main(args):
    r = agent("answer badly", schema = {"type": "object", "properties": {"n": {"type": "integer"}}, "required": ["n"]})
    return r == None
