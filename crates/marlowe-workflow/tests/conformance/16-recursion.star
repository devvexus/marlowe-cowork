META = {"name": "recursion", "description": "Recursion is refused before running"}

def ping(n):
    return pong(n)

def pong(n):
    return ping(n)

def main(args):
    return ping(1)
