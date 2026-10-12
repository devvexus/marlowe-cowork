import re


def parse_duration(text):
    """Seconds in a string like '1h30m10s' (any of the parts may be missing)."""
    total = 0
    for number, unit in re.findall(r"(\d+)([hms])", text):
        total += int(number) * {"h": 3600, "m": 60, "s": 1}[unit]
    return total
