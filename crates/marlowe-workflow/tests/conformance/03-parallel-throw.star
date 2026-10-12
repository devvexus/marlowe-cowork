META = {"name": "parallel-throw", "description": "A thunk that fails gives None; parallel never fails"}

def boom():
    fail("this thunk fails")

def main(args):
    return parallel([boom, lambda: agent("ok"), lambda: 1 // 0])
