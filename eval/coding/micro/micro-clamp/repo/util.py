def clamp(x, low, high):
    """Return x limited to the closed range [low, high]."""
    return min(low, max(x, high))
