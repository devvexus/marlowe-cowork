META = {"name": "forbidden", "description": "No clock, randomness or files"}

def main(args):
    t = time.now()
    r = random()
    f = open("secrets.txt")
    return [t, r, f]
