import csv


def column_sum(path, column):
    """Sum of the numeric values under the header `column`."""
    total = 0.0
    with open(path, newline="") as f:
        rows = list(csv.reader(f))
    index = rows[0].index(column)
    for row in rows:
        total += float(row[index])
    return total
