META = {"name": "budget-loop", "description": "Loop until the budget runs low"}

def main(args):
    done = []
    for i in range(100):
        left = budget.remaining()
        if left != None and left < 10:
            break
        done.append(agent("step " + str(i)))
    return {"steps": len(done), "spent": budget.spent(), "total": budget.total}
