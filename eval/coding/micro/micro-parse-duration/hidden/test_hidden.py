from timeparse import parse_duration


def test_minutes():
    assert parse_duration("45m") == 2700


def test_mixed():
    assert parse_duration("1h30m") == 5400


def test_all_three():
    assert parse_duration("2h5m10s") == 7510


def test_empty():
    assert parse_duration("") == 0
