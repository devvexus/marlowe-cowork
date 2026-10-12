from roman import from_roman


def test_three():
    assert from_roman("III") == 3
