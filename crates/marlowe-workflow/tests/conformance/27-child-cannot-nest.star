META = {"name": "grandparent", "description": "A child workflow cannot start another"}

def main(args):
    return workflow("nester", {})
