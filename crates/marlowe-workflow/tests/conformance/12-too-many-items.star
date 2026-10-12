META = {"name": "too-many", "description": "parallel takes at most 4096 items"}

def main(args):
    return parallel([lambda: 1 for _ in range(4097)])
