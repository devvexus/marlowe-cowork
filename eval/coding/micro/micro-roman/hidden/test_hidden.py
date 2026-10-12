from roman import from_roman


def test_subtractive():
    assert from_roman("IV") == 4
    assert from_roman("CM") == 900


def test_long():
    assert from_roman("MCMXCIV") == 1994


def test_big():
    assert from_roman("MMXXIV") == 2024


def test_single():
    assert from_roman("X") == 10
