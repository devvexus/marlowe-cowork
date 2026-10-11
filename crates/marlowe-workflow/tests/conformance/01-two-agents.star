META = {"name": "two-agents", "description": "Two agents in sequence; the second sees the first's answer"}

def main(args):
    a = agent("first")
    b = agent("second after " + a)
    return [a, b]
