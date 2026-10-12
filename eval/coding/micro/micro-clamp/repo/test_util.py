from util import clamp


def test_inside():
    assert clamp(5, 0, 10) == 5
