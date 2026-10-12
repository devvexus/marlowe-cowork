META = {"name": "nester", "description": "Tries to start a workflow from a workflow"}

def main(args):
    return workflow("child", {"topic": "x"})
