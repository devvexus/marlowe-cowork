META = {"name": "schema-unsat", "description": "An unusable schema throws before any call"}

def main(args):
    return agent("never runs", schema = {"type": "object", "required": ["missing"]})
