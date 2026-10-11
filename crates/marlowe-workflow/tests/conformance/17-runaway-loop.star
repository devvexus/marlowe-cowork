META = {"name": "runaway", "description": "A pass that runs too many statements stops"}

def main(args):
    n = 0
    for i in range(1000000000):
        n += 1
    return n
