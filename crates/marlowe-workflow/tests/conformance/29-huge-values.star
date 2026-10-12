META = {"name": "huge-values", "description": "Repeats, shifts, ranges and a growing heap end with an error, not an allocation"}

def main(args):
    kind = args["kind"]
    if kind == "list":
        return len([0] * 1000000000)
    if kind == "string":
        return len("x" * 2000000000)
    if kind == "bigint-string":
        n = 2000000000
        n = n * 2
        return len("x" * n)
    if kind == "shift":
        return 1 << 100000000
    if kind == "range":
        return max(range(2000000000))
    if kind == "list-range":
        return len(list(range(1000000000)))
    if kind == "doubling":
        s = "x"
        for i in range(64):
            s = s + s
        return len(s)
    if kind == "parallel":
        return parallel(range(1000000000))
    if kind == "lambda-doubling":
        # One statement and no loop: only the size check on `+` sees it.
        twice = lambda s: s + s
        return len(twice(twice(twice(twice(twice(twice("x" * 1000000)))))))
    if kind == "list-concat":
        grow = lambda l: l + l
        return len(grow(grow(grow(grow(grow(grow(grow([0] * 100000))))))))
    return len("ab" * 3) + len([1, 2] * 2) + (3 << 2)
