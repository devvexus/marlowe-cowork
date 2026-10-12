from wrapper import wrap


def test_short():
    assert wrap("hi there", 20) == "hi there"
