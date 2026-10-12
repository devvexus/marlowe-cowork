from stats import moving_average


def test_basic():
    assert moving_average([1, 2, 3, 4], 2)[0] == 1.5
