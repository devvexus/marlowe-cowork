"""Benchmark date strings to epoch milliseconds.

Deliberately locale-independent and deliberately clock-free. `datetime.now` never appears:
every timestamp in a run comes from the corpus or from the synthetic clock, so that a run in
August and a run in December produce the same bytes.

Weekday and month names are matched from explicit tables rather than `%a`/`%b`, because
those are locale-sensitive and would make reproduction depend on the machine's LANG.
"""

from __future__ import annotations

import re
from datetime import datetime, timezone

from .errors import CorpusFormatError

_MONTHS = {
    "january": 1, "february": 2, "march": 3, "april": 4, "may": 5, "june": 6,
    "july": 7, "august": 8, "september": 9, "october": 10, "november": 11,
    "december": 12,
    "jan": 1, "feb": 2, "mar": 3, "apr": 4, "jun": 6, "jul": 7, "aug": 8,
    "sep": 9, "sept": 9, "oct": 10, "nov": 11, "dec": 12,
}

# "2023/05/20 (Sat) 02:36"  -- LongMemEval
_LME = re.compile(r"^\s*(\d{4})/(\d{1,2})/(\d{1,2})(?:\s*\([^)]*\))?\s*(\d{1,2}):(\d{2})")

# "1:56 pm on 8 May, 2023"  -- LoCoMo
_LOCOMO = re.compile(
    r"^\s*(\d{1,2}):(\d{2})\s*([ap]m)\s+on\s+(\d{1,2})\s+([A-Za-z]+),?\s+(\d{4})",
    re.IGNORECASE,
)

# ISO-8601, for fixtures and for any dataset that grows up
_ISO = re.compile(r"^\s*(\d{4})-(\d{2})-(\d{2})[T ](\d{2}):(\d{2})")


def _epoch_ms(y: int, mo: int, d: int, h: int, mi: int) -> int:
    return int(datetime(y, mo, d, h, mi, tzinfo=timezone.utc).timestamp() * 1000)


def parse(value: str, *, dataset: str, where: str | None = None) -> int:
    """Return epoch milliseconds, UTC. Raise rather than guess."""
    if not isinstance(value, str):
        raise CorpusFormatError(dataset, f"expected a date string, got {type(value).__name__}", where=where)

    m = _ISO.match(value)
    if m:
        y, mo, d, h, mi = (int(g) for g in m.groups())
        return _epoch_ms(y, mo, d, h, mi)

    m = _LME.match(value)
    if m:
        y, mo, d, h, mi = (int(g) for g in m.groups())
        return _epoch_ms(y, mo, d, h, mi)

    m = _LOCOMO.match(value)
    if m:
        hour, minute, meridiem, day, month_name, year = m.groups()
        month = _MONTHS.get(month_name.strip().lower())
        if month is None:
            raise CorpusFormatError(dataset, f"unrecognised month {month_name!r}", where=where)
        h = int(hour) % 12
        if meridiem.lower() == "pm":
            h += 12
        return _epoch_ms(int(year), month, int(day), h, int(minute))

    raise CorpusFormatError(
        dataset, f"unrecognised date format: {value!r}", where=where
    )
