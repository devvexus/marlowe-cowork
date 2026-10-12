from timeparse import parse_duration


def test_hours():
    assert parse_duration("2h") == 7200
