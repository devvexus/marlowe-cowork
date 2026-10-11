META = {"name": "budget-ceiling", "description": "agent() throws once the budget is spent"}

def main(args):
    out = []
    for i in range(10):
        out.append(agent("step " + str(i)))
    return out
