def moving_average(values, window):
    """Averages of each run of `window` consecutive values, in order."""
    out = []
    for i in range(len(values) - window + 1):
        out.append(sum(values[i:i + window]) / window)
    return out
