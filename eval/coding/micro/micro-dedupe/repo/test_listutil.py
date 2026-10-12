from listutil import flatten


def test_flatten():
    assert flatten([[1], [2, 3]]) == [1, 2, 3]
