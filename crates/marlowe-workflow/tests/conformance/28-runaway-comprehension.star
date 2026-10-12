META = {"name": "runaway-comprehension", "description": "Comprehension iterations count like statements"}

def main(args):
    if args.get("flat"):
        return len([0 for i in range(100000) for j in range(100000)])
    if args.get("filtered"):
        return len([0 for i in range(100000) for j in range(100000) if False])
    if args.get("nested"):
        return len([[0 for j in range(100000)] for i in range(100000)])
    if args.get("dict"):
        return len({str(i) + "-" + str(j): 0 for i in range(100000) for j in range(100000)})
    return len([x for x in range(10) if x % 2 == 0])
