def clamp(x, low, high):
    """Return x limited to the closed range [low, high]."""
    return max(low, min(x, high))
