VALUES = {"I": 1, "V": 5, "X": 10, "L": 50, "C": 100, "D": 500, "M": 1000}


def from_roman(s):
    total = 0
    for i, ch in enumerate(s):
        v = VALUES[ch]
        if i + 1 < len(s) and VALUES[s[i + 1]] > v:
            total -= v
        else:
            total += v
    return total
